use bytes::{Buf, BufMut, Bytes};
use rustc_hash::FxHashSet;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const SECTOR_BYTES: usize = 4096;
pub const CHUNK_COUNT: usize = 1024;
const HEADER_BYTES: usize = SECTOR_BYTES * 2;
const EXTERNAL_STREAM_FLAG: u8 = 0x80;
const EXTERNAL_CHUNK_THRESHOLD: usize = 256;
const MAX_SECTOR_OFFSET: usize = 0x00ff_ffff;

#[derive(Debug, Clone)]
pub struct RegionRecord {
    pub compression: u8,
    pub payload: Bytes,
    pub timestamp: u32,
    external: bool,
    pub load_error: Option<String>,
}

impl RegionRecord {
    pub const fn new(compression: u8, payload: Bytes, timestamp: u32) -> Self {
        Self {
            compression,
            external: (payload.len() + 5).div_ceil(SECTOR_BYTES) >= EXTERNAL_CHUNK_THRESHOLD,
            payload,
            timestamp,
            load_error: None,
        }
    }

    const fn sector_count(&self) -> usize {
        if self.external {
            1
        } else {
            (self.payload.len() + 5).div_ceil(SECTOR_BYTES)
        }
    }

    fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        let length = if self.external {
            1
        } else {
            u32::try_from(self.payload.len() + 1)
                .map_err(|_| invalid("Chunk payload is too large"))?
        };
        writer.write_all(&length.to_be_bytes())?;
        writer.write_all(&[self.compression
            | if self.external {
                EXTERNAL_STREAM_FLAG
            } else {
                0
            }])?;
        if !self.external {
            writer.write_all(&self.payload)?;
        }
        let used = if self.external {
            5
        } else {
            self.payload.len() + 5
        };
        let padding = self.sector_count() * SECTOR_BYTES - used;
        writer.write_all(&[0; SECTOR_BYTES][..padding])
    }
}

/// Anvil framing shared by terrain, entities and POI. NBT is opaque here.
#[derive(Debug, Clone)]
pub struct AnvilRegion {
    pub records: Vec<Option<RegionRecord>>,
    locations: Vec<u32>,
    dirty: FxHashSet<usize>,
    pub blocked: FxHashSet<usize>,
    pub validated: FxHashSet<usize>,
}

impl Default for AnvilRegion {
    fn default() -> Self {
        Self {
            records: vec![None; CHUNK_COUNT],
            locations: vec![0; CHUNK_COUNT],
            dirty: FxHashSet::default(),
            blocked: FxHashSet::default(),
            validated: FxHashSet::default(),
        }
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl AnvilRegion {
    pub fn read(bytes: &Bytes) -> io::Result<Self> {
        if bytes.len() < HEADER_BYTES {
            return Err(invalid("Truncated region header"));
        }
        Self::read_records(&bytes[..HEADER_BYTES], bytes.len(), |start, end| {
            Ok(bytes.slice(start..end))
        })
    }

    fn read_records(
        header: &[u8],
        file_length: usize,
        mut read: impl FnMut(usize, usize) -> io::Result<Bytes>,
    ) -> io::Result<Self> {
        let mut locations = &header[..SECTOR_BYTES];

        let mut timestamps = &header[SECTOR_BYTES..HEADER_BYTES];
        let mut region = Self::default();
        let mut used = FxHashSet::default();
        for index in 0..CHUNK_COUNT {
            let location = locations.get_u32();
            let timestamp = timestamps.get_u32();
            region.locations[index] = location;
            if location == 0 {
                continue;
            }
            let offset = (location >> 8) as usize;
            let sectors = (location & 0xff) as usize;
            if offset < 2 || sectors == 0 {
                return Err(invalid(format!("Invalid location for chunk {index}")));
            }
            for sector in offset..offset + sectors {
                if !used.insert(sector) {
                    return Err(invalid(format!("Overlapping allocation for chunk {index}")));
                }
            }
            let start = offset * SECTOR_BYTES;
            let end = ((offset + sectors) * SECTOR_BYTES).min(file_length);
            if start + 5 > end {
                return Err(invalid(format!("Truncated chunk header at index {index}")));
            }
            let mut record = read(start, end)?;
            let length = record.get_u32() as usize;
            let compression = record.get_u8();
            let external = compression & EXTERNAL_STREAM_FLAG != 0;
            let compression = compression & !EXTERNAL_STREAM_FLAG;
            if !matches!(compression, 1..=4 | 127) {
                return Err(invalid(format!("Unknown compression for chunk {index}")));
            }
            // Length includes the compression byte, which has already been consumed.
            if length == 0
                || length - 1 > record.len()
                || (external && (length != 1 || sectors != 1))
            {
                return Err(invalid(format!(
                    "Invalid payload length for chunk {index}: {length}"
                )));
            }
            // A final sector need not be padded, but its allocation must reach that sector.
            if (offset + sectors - 1) * SECTOR_BYTES >= file_length {
                return Err(invalid(format!(
                    "Allocation exceeds file for chunk {index}"
                )));
            }
            region.records[index] = Some(RegionRecord {
                compression,
                payload: record.slice(..length - 1),
                timestamp,
                external,
                load_error: external.then(|| "External chunk requires its region path".to_owned()),
            });
        }
        Ok(region)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let file_length = usize::try_from(file.metadata()?.len())
            .map_err(|_| invalid("Region file is too large"))?;
        let mut header = vec![0; HEADER_BYTES];
        file.read_exact(&mut header)?;
        let mut region = Self::read_records(&header, file_length, |start, end| {
            file.seek(SeekFrom::Start(start as u64))?;
            let mut bytes = vec![0; end - start];
            file.read_exact(&mut bytes)?;
            Ok(bytes.into())
        })?;
        for (index, record) in region.records.iter_mut().enumerate() {
            if let Some(record) = record.as_mut().filter(|record| record.external) {
                match external_path(path, index).and_then(|path| {
                    const MAX_EXTERNAL_BYTES: u64 = 512 * 1024 * 1024;
                    let file = fs::File::open(path)?;
                    let mut payload = Vec::new();
                    file.take(MAX_EXTERNAL_BYTES + 1)
                        .read_to_end(&mut payload)?;
                    if payload.len() as u64 > MAX_EXTERNAL_BYTES {
                        return Err(invalid("External chunk exceeds size limit"));
                    }
                    Ok(payload)
                }) {
                    Ok(payload) => {
                        record.payload = payload.into();
                        record.load_error = None;
                    }
                    Err(error) => {
                        record.load_error = Some(error.to_string());
                        region.blocked.insert(index);
                    }
                }
            }
        }
        Ok(region)
    }

    pub fn set(&mut self, index: usize, record: Option<RegionRecord>) -> io::Result<()> {
        if self.blocked.contains(&index) {
            return Err(invalid(format!(
                "Unreadable chunk {index} cannot be replaced"
            )));
        }
        self.records[index] = record;
        self.dirty.insert(index);
        Ok(())
    }

    pub fn write(&mut self, path: &Path, in_place: bool) -> io::Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let indices: Vec<_> = self.dirty.iter().copied().collect();
        // Each external file is atomically replaced before its reference is published.
        for &index in &indices {
            if let Some(record) = self.records[index]
                .as_ref()
                .filter(|record| record.external)
            {
                atomic_write(&external_path(path, index)?, &record.payload)?;
            }
        }
        let mut new_locations = self.locations.clone();
        if in_place && path.exists() {
            let mut file = OpenOptions::new().read(true).write(true).open(path)?;
            let mut used = FxHashSet::default();
            used.extend(0..2);
            for location in &self.locations {
                let offset = (location >> 8) as usize;
                let sectors = (location & 0xff) as usize;
                used.extend(offset..offset + sectors);
            }
            // Old allocations remain occupied until the new header has been committed.
            for &index in &indices {
                if let Some(record) = &self.records[index] {
                    let count = record.sector_count();
                    let mut offset = 2;
                    while (offset..offset + count).any(|sector| used.contains(&sector)) {
                        offset += 1;
                    }
                    new_locations[index] = location(offset, count)?;
                    used.extend(offset..offset + count);
                    file.seek(SeekFrom::Start((offset * SECTOR_BYTES) as u64))?;
                    record.write(&mut file)?;
                } else {
                    new_locations[index] = 0;
                }
            }
            file.flush()?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&self.header(&new_locations))?;
            file.flush()?;
        } else {
            let temp = temporary_path(path);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            let result = (|| {
                let mut offset = 2;
                for (index, record) in self.records.iter().enumerate() {
                    new_locations[index] = if let Some(record) = record {
                        let count = record.sector_count();
                        let entry = location(offset, count)?;
                        offset += count;
                        entry
                    } else {
                        0
                    };
                }
                file.write_all(&self.header(&new_locations))?;
                for record in self.records.iter().flatten() {
                    record.write(&mut file)?;
                }
                file.flush()?;
                fs::rename(&temp, path)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            result?;
        }
        self.locations = new_locations;
        // Only remove obsolete payloads after the inline record or deletion is committed.
        for &index in &indices {
            if self.records[index]
                .as_ref()
                .is_none_or(|record| !record.external)
            {
                match fs::remove_file(external_path(path, index)?) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        self.dirty.clear();
        Ok(())
    }

    fn header(&self, locations: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES);
        for &location in locations {
            bytes.put_u32(location);
        }
        for record in &self.records {
            bytes.put_u32(record.as_ref().map_or(0, |record| record.timestamp));
        }
        bytes
    }

    pub fn synchronize(&self, path: &Path) -> io::Result<()> {
        if path.exists() {
            OpenOptions::new().write(true).open(path)?.sync_all()?;
        }
        for (index, record) in self.records.iter().enumerate() {
            if record.as_ref().is_some_and(|record| record.external) {
                OpenOptions::new()
                    .write(true)
                    .open(external_path(path, index)?)?
                    .sync_all()?;
            }
        }
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

fn location(offset: usize, count: usize) -> io::Result<u32> {
    if offset > MAX_SECTOR_OFFSET || count == 0 || count >= EXTERNAL_CHUNK_THRESHOLD {
        return Err(invalid("Anvil allocation exceeds location limits"));
    }
    Ok(((offset as u32) << 8) | count as u32)
}

fn external_path(path: &Path, index: usize) -> io::Result<PathBuf> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| invalid("Invalid region filename"))?;
    let mut parts = name.split('.');
    if parts.next() != Some("r") {
        return Err(invalid("Invalid region filename"));
    }
    let x: i32 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| invalid("Invalid region X"))?;
    let z: i32 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| invalid("Invalid region Z"))?;
    let x = x
        .checked_mul(32)
        .and_then(|x| x.checked_add((index % 32) as i32))
        .ok_or_else(|| invalid("Chunk X exceeds coordinate limits"))?;
    let z = z
        .checked_mul(32)
        .and_then(|z| z.checked_add((index / 32) as i32))
        .ok_or_else(|| invalid("Chunk Z exceeds coordinate limits"))?;
    Ok(path.with_file_name(format!("c.{x}.{z}.mcc")))
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = temporary_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_boundaries_and_external_transitions() -> io::Result<()> {
        for in_place in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("r.-1.-2.mca");
            let external = directory.path().join("c.-1.-33.mcc");
            let index = CHUNK_COUNT - 1;
            let mut region = AnvilRegion::default();
            for size in [255 * SECTOR_BYTES - 5, 256 * SECTOR_BYTES - 5, 1_140_725, 7] {
                let payload = Bytes::from(vec![42; size]);
                region.set(index, Some(RegionRecord::new(3, payload.clone(), 1)))?;
                region.write(&path, in_place)?;
                let bytes = fs::read(&path)?;
                let location =
                    u32::from_be_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap());
                let offset = (location >> 8) as usize * SECTOR_BYTES;
                if size >= 256 * SECTOR_BYTES - 5 {
                    assert_eq!(location & 0xff, 1);
                    assert_eq!(&bytes[offset..offset + 5], &[0, 0, 0, 1, 0x83]);
                    assert_eq!(fs::read(&external)?, payload);
                } else {
                    assert!(!external.exists());
                }
                let loaded = AnvilRegion::load(&path)?;
                assert_eq!(loaded.records[index].as_ref().unwrap().payload, payload);
                region = loaded;
            }
            region.set(index, None)?;
            region.write(&path, in_place)?;
            assert!(AnvilRegion::load(&path)?.records[index].is_none());
        }
        Ok(())
    }

    #[test]
    fn malformed_framing_and_unpadded_final_sector() -> io::Result<()> {
        let mut bytes = vec![0; HEADER_BYTES];
        bytes[..4].copy_from_slice(&0x201u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 2, 3, 42]);
        let loaded = AnvilRegion::read(&bytes.clone().into())?;
        assert_eq!(
            loaded.records[0].as_ref().unwrap().payload,
            Bytes::from_static(&[42])
        );
        bytes.pop();
        assert!(AnvilRegion::read(&bytes.into()).is_err());
        assert!(location(MAX_SECTOR_OFFSET + 1, 1).is_err());
        assert!(location(2, 256).is_err());
        Ok(())
    }

    #[test]
    fn failed_write_keeps_pending_record_and_original_file() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("r.0.0.mca");
        let mut region = AnvilRegion::default();
        region.set(0, Some(RegionRecord::new(3, Bytes::from_static(b"old"), 1)))?;
        region.write(&path, false)?;
        let original = fs::read(&path)?;
        fs::create_dir(directory.path().join("c.0.0.mcc"))?;
        region.set(
            0,
            Some(RegionRecord::new(
                3,
                Bytes::from(vec![42; 256 * SECTOR_BYTES]),
                2,
            )),
        )?;
        assert!(region.write(&path, true).is_err());
        assert_eq!(fs::read(&path)?, original);
        assert!(region.dirty.contains(&0));
        fs::remove_dir(directory.path().join("c.0.0.mcc"))?;
        region.write(&path, true)?;
        assert_eq!(
            AnvilRegion::load(&path)?.records[0]
                .as_ref()
                .unwrap()
                .payload
                .len(),
            256 * SECTOR_BYTES
        );
        Ok(())
    }
}
