use rustc_hash::FxHashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::chunk::{
    format::anvil::Compression as AnvilCompression,
    io::region::{AnvilRegion, RegionRecord},
};
use pumpkin_config::chunk::AnvilChunkConfig;
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use serde::{Deserialize, Serialize};

/// POI type identifier for nether portals
pub const POI_TYPE_NETHER_PORTAL: &str = "minecraft:nether_portal";

use crate::world_info::CURRENT_WORLD_DATA_VERSION as DATA_VERSION;

/// A single Point of Interest entry (serializable)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoiEntry {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    #[serde(rename = "type")]
    pub poi_type: String,
    pub free_tickets: i32,
    #[serde(skip)]
    retained: NbtCompound,
}

impl PoiEntry {
    #[must_use]
    pub fn new_portal(pos: BlockPos) -> Self {
        Self {
            x: pos.0.x,
            y: pos.0.y,
            z: pos.0.z,
            poi_type: POI_TYPE_NETHER_PORTAL.to_string(),
            free_tickets: 0,
            retained: NbtCompound::new(),
        }
    }

    #[must_use]
    pub const fn pos(&self) -> BlockPos {
        BlockPos(Vector3::new(self.x, self.y, self.z))
    }
}

/// POI section data (serializable) - vanilla format
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoiSectionData {
    #[serde(default)]
    pub valid: i8,
    #[serde(default)]
    pub records: Vec<PoiEntry>,
    #[serde(skip)]
    retained: NbtCompound,
}

/// POI chunk data (serializable) - vanilla format
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoiChunkData {
    pub data_version: i32,
    /// Sections keyed by Y section coordinate (e.g., "-1", "0", "1", "4")
    pub sections: FxHashMap<String, PoiSectionData>,
    #[serde(skip)]
    retained: NbtCompound,
}

/// POI data for a single region (32x32 chunks) using MCA format
#[derive(Debug, Default, Clone)]
pub struct PoiRegion {
    /// Entries indexed by position
    entries: FxHashMap<(i32, i32, i32), PoiEntry>,
    /// Track which chunks are dirty
    dirty_chunks: rustc_hash::FxHashSet<(i32, i32)>,
    dirty: bool,
    transport: AnvilRegion,
    load_error: Option<String>,
    record_errors: FxHashMap<usize, String>,
    chunks: FxHashMap<usize, PoiChunkData>,
}

impl PoiRegion {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    const fn pos_key(pos: &BlockPos) -> (i32, i32, i32) {
        (pos.0.x, pos.0.y, pos.0.z)
    }

    /// Get chunk index in MCA file (0-1023)
    const fn chunk_index(chunk_x: i32, chunk_z: i32) -> usize {
        let local_x = chunk_x & 31;
        let local_z = chunk_z & 31;
        ((local_z << 5) | local_x) as usize
    }

    /// Returns section key as just the Y section coordinate (like vanilla)
    fn section_key(pos: &BlockPos) -> String {
        let section_y = pos.0.y >> 4;
        section_y.to_string()
    }

    pub fn add(&mut self, mut entry: PoiEntry) {
        let chunk_x = entry.x >> 4;
        let chunk_z = entry.z >> 4;
        self.dirty_chunks.insert((chunk_x, chunk_z));
        let key = (entry.x, entry.y, entry.z);
        if let Some(previous) = self.entries.get(&key) {
            entry.retained.clone_from(&previous.retained);
        }
        self.entries.insert(key, entry);
        self.dirty = true;
    }

    pub fn remove(&mut self, pos: &BlockPos) -> bool {
        let key = Self::pos_key(pos);
        if self.entries.remove(&key).is_some() {
            let chunk_x = pos.0.x >> 4;
            let chunk_z = pos.0.z >> 4;
            self.dirty_chunks.insert((chunk_x, chunk_z));
            self.dirty = true;
            return true;
        }
        false
    }

    #[must_use]
    pub fn get_all(&self) -> Vec<&PoiEntry> {
        self.entries.values().collect()
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.dirty_chunks.clear();
    }

    fn get_chunk_data(&self, chunk_x: i32, chunk_z: i32) -> Option<PoiChunkData> {
        let mut data = self
            .chunks
            .get(&Self::chunk_index(chunk_x, chunk_z))
            .cloned()
            .unwrap_or_default();
        data.data_version = DATA_VERSION;
        for section in data.sections.values_mut() {
            section.records.clear();
        }
        for entry in self
            .entries
            .values()
            .filter(|entry| entry.x >> 4 == chunk_x && entry.z >> 4 == chunk_z)
        {
            data.sections
                .entry(Self::section_key(&entry.pos()))
                .or_insert_with(|| PoiSectionData {
                    valid: 1,
                    ..PoiSectionData::default()
                })
                .records
                .push(entry.clone());
        }
        (!data.sections.is_empty() || !data.retained.is_empty()).then_some(data)
    }

    fn compress_chunk_data(
        chunk_data: &PoiChunkData,
        compression: u8,
        level: u32,
    ) -> std::io::Result<Vec<u8>> {
        let mut root = chunk_data.retained.clone();
        root.put_int("DataVersion", DATA_VERSION);
        let mut sections = NbtCompound::new();
        for (key, section) in &chunk_data.sections {
            let mut section_nbt = section.retained.clone();
            section_nbt.put_byte("Valid", section.valid);
            let records = section
                .records
                .iter()
                .map(|record| {
                    let mut nbt = record.retained.clone();
                    for key in ["x", "y", "z"] {
                        nbt.child_tags.remove(key);
                    }
                    nbt.put("pos", NbtTag::IntArray(vec![record.x, record.y, record.z]));
                    nbt.put_string("type", record.poi_type.clone());
                    nbt.put_int("free_tickets", record.free_tickets);
                    NbtTag::Compound(nbt)
                })
                .collect();
            section_nbt.put_list("Records", records);
            sections.put_compound(key, section_nbt);
        }
        root.put_compound("Sections", sections);
        let bytes = pumpkin_nbt::Nbt::from(root).write();
        AnvilCompression::from_byte(compression)
            .map_err(|()| std::io::Error::other("Unknown POI compression"))?
            .map_or_else(
                || Ok(bytes.to_vec()),
                |compression| {
                    compression
                        .compress_data(&bytes, level)
                        .map_err(|error| std::io::Error::other(error.to_string()))
                },
            )
    }

    fn decompress_chunk_data(compressed: &[u8], compression: u8) -> std::io::Result<PoiChunkData> {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid POI NBT");
        let compression = AnvilCompression::from_byte(compression).map_err(|()| invalid())?;
        let bytes = match compression {
            Some(compression) => compression
                .decompress_data(compressed)
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .into_vec(),
            None => compressed.to_vec(),
        };
        let mut cursor = Cursor::new(bytes);
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(
            pumpkin_nbt::deserializer::NbtStreamReader(&mut cursor),
        );
        let root = pumpkin_nbt::Nbt::read(&mut reader)
            .map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?
            .root_tag;
        let mut sections = FxHashMap::default();
        if let Some(tag) = root.get("Sections") {
            let NbtTag::Compound(section_tags) = tag else {
                return Err(invalid());
            };
            for (key, tag) in &section_tags.child_tags {
                let NbtTag::Compound(section) = tag else {
                    return Err(invalid());
                };
                key.parse::<i32>().map_err(|_| invalid())?;
                let records = section.get_list("Records").ok_or_else(invalid)?;
                let mut entries = Vec::new();
                for tag in records {
                    let NbtTag::Compound(record) = tag else {
                        return Err(invalid());
                    };
                    let pos = match record.get("pos") {
                        Some(NbtTag::IntArray(pos)) if pos.len() == 3 => [pos[0], pos[1], pos[2]],
                        Some(_) => return Err(invalid()),
                        None => [
                            record.get_int("x").ok_or_else(invalid)?,
                            record.get_int("y").ok_or_else(invalid)?,
                            record.get_int("z").ok_or_else(invalid)?,
                        ],
                    };
                    entries.push(PoiEntry {
                        x: pos[0],
                        y: pos[1],
                        z: pos[2],
                        poi_type: record.get_string("type").ok_or_else(invalid)?.to_string(),
                        free_tickets: match record.get("free_tickets") {
                            None => 0,
                            Some(NbtTag::Int(value)) => *value,
                            Some(_) => return Err(invalid()),
                        },
                        retained: record.clone(),
                    });
                }
                sections.insert(
                    key.to_string(),
                    PoiSectionData {
                        valid: section.get_byte("Valid").unwrap_or(0),
                        records: entries,
                        retained: section.clone(),
                    },
                );
            }
        }
        Ok(PoiChunkData {
            data_version: root.get_int("DataVersion").unwrap_or(DATA_VERSION),
            sections,
            retained: root,
        })
    }

    pub fn save(&mut self, path: &Path) -> std::io::Result<()> {
        self.save_with_config(path, &AnvilChunkConfig::default())
    }

    fn save_with_config(&mut self, path: &Path, config: &AnvilChunkConfig) -> std::io::Result<()> {
        if let Some(error) = &self.load_error {
            return Err(std::io::Error::other(error.clone()));
        }
        if !self.dirty {
            return self
                .record_errors
                .values()
                .next()
                .map_or(Ok(()), |error| Err(std::io::Error::other(error.clone())));
        }
        let mut transport = self.transport.clone();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as u32);
        let mut applied = Vec::new();
        let mut first_error = self
            .record_errors
            .values()
            .next()
            .map(|error| std::io::Error::other(error.clone()));
        for &(x, z) in &self.dirty_chunks {
            let index = Self::chunk_index(x, z);
            let compression = transport.records[index].as_ref().map_or(
                AnvilCompression::from(config.compression.algorithm) as u8,
                |record| record.compression,
            );
            let record = self
                .get_chunk_data(x, z)
                .map(|data| {
                    Self::compress_chunk_data(&data, compression, config.compression.level)
                        .map(|payload| RegionRecord::new(compression, payload.into(), timestamp))
                })
                .transpose()?;
            match transport.set(index, record) {
                Ok(()) => applied.push((x, z)),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        transport.write(path, config.write_in_place)?;
        self.transport = transport;
        for pos in applied {
            self.dirty_chunks.remove(&pos);
        }
        self.dirty = !self.dirty_chunks.is_empty();
        first_error.map_or(Ok(()), Err)
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let transport = AnvilRegion::load(path)?;
        let mut region = Self {
            transport,
            ..Self::default()
        };
        for index in 0..region.transport.records.len() {
            let Some(record) = &region.transport.records[index] else {
                continue;
            };
            let data = record.load_error.as_ref().map_or_else(
                || Self::decompress_chunk_data(&record.payload, record.compression),
                |error| Err(std::io::Error::other(error.clone())),
            );
            match data {
                Ok(data) => {
                    for section in data.sections.values() {
                        for entry in &section.records {
                            region
                                .entries
                                .insert((entry.x, entry.y, entry.z), entry.clone());
                        }
                    }
                    region.chunks.insert(index, data);
                }
                Err(error) => {
                    warn!("Failed to parse POI chunk at index {index}: {error}");
                    region.transport.blocked.insert(index);
                    region.record_errors.insert(index, error.to_string());
                }
            }
        }
        Ok(region)
    }
}

/// Region-based POI storage using MCA format
#[derive(Clone)]
pub struct PoiStorage {
    /// Path to the poi folder
    folder: PathBuf,
    /// Loaded regions, keyed by (`region_x`, `region_z`)
    regions: FxHashMap<(i32, i32), PoiRegion>,
    config: AnvilChunkConfig,
}

impl PoiStorage {
    #[must_use]
    pub fn new(poi_folder: PathBuf) -> Self {
        Self {
            folder: poi_folder,
            regions: FxHashMap::default(),
            config: AnvilChunkConfig::default(),
        }
    }

    #[must_use]
    pub fn new_with_config(poi_folder: PathBuf, config: AnvilChunkConfig) -> Self {
        Self {
            folder: poi_folder,
            regions: FxHashMap::default(),
            config,
        }
    }

    const fn region_coords(pos: &BlockPos) -> (i32, i32) {
        let chunk_x = pos.0.x >> 4;
        let chunk_z = pos.0.z >> 4;
        (chunk_x >> 5, chunk_z >> 5)
    }

    fn region_path(&self, rx: i32, rz: i32) -> PathBuf {
        self.folder.join(format!("r.{rx}.{rz}.mca"))
    }

    fn get_or_load_region(&mut self, rx: i32, rz: i32) -> &mut PoiRegion {
        let path = self.region_path(rx, rz);
        self.regions.entry((rx, rz)).or_insert_with(|| {
            PoiRegion::load(&path).unwrap_or_else(|e| {
                if path.exists() {
                    warn!("Failed to load POI region {}: {}", path.display(), e);
                }
                PoiRegion {
                    load_error: Some(e.to_string()),
                    ..PoiRegion::default()
                }
            })
        })
    }

    pub fn add(&mut self, pos: BlockPos, poi_type: &str) {
        self.add_with_free_tickets(pos, poi_type, 0);
    }

    pub fn add_with_free_tickets(&mut self, pos: BlockPos, poi_type: &str, free_tickets: i32) {
        let (rx, rz) = Self::region_coords(&pos);
        let region = self.get_or_load_region(rx, rz);
        region.add(PoiEntry {
            x: pos.0.x,
            y: pos.0.y,
            z: pos.0.z,
            poi_type: poi_type.to_string(),
            free_tickets,
            retained: NbtCompound::new(),
        });
    }

    pub fn add_portal(&mut self, pos: BlockPos) {
        self.add(pos, POI_TYPE_NETHER_PORTAL);
    }

    pub fn remove(&mut self, pos: &BlockPos) -> bool {
        let (rx, rz) = Self::region_coords(pos);
        let region = self.get_or_load_region(rx, rz);
        region.remove(pos)
    }

    /// Get all POI positions within a square radius (for portal search)
    #[expect(clippy::similar_names)]
    pub fn get_in_square(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
    ) -> Vec<BlockPos> {
        let min_x = center.0.x - radius;
        let max_x = center.0.x + radius;
        let min_z = center.0.z - radius;
        let max_z = center.0.z + radius;

        // Calculate which regions we need to check
        let min_rx = (min_x >> 4) >> 5;
        let max_rx = (max_x >> 4) >> 5;
        let min_rz = (min_z >> 4) >> 5;
        let max_rz = (max_z >> 4) >> 5;

        let mut results = Vec::new();

        for rx in min_rx..=max_rx {
            for rz in min_rz..=max_rz {
                let region = self.get_or_load_region(rx, rz);
                for entry in region.get_all() {
                    if let Some(filter_type) = poi_type
                        && entry.poi_type != filter_type
                    {
                        continue;
                    }

                    let dx = (entry.x - center.0.x).abs();
                    let dz = (entry.z - center.0.z).abs();
                    if dx <= radius && dz <= radius {
                        results.push(entry.pos());
                    }
                }
            }
        }

        results
    }

    /// Finds the closest POI whose type matches `matches`, considering
    /// entries within `radius` blocks of `center` on the x/z axes (like
    /// vanilla's `PoiManager.findClosestWithType`: a chebyshev square gather
    /// followed by picking the smallest 3D squared distance).
    ///
    /// Returns the entry's position together with its type.
    pub fn find_closest_matching(
        &mut self,
        center: BlockPos,
        radius: i32,
        matches: impl Fn(&str) -> bool,
    ) -> Option<(BlockPos, String)> {
        let min_rx = ((center.0.x - radius) >> 4) >> 5;
        let max_rx = ((center.0.x + radius) >> 4) >> 5;
        let min_rz = ((center.0.z - radius) >> 4) >> 5;
        let max_rz = ((center.0.z + radius) >> 4) >> 5;

        let mut best: Option<(BlockPos, String, i64)> = None;

        for rx in min_rx..=max_rx {
            for rz in min_rz..=max_rz {
                let region = self.get_or_load_region(rx, rz);
                for entry in region.get_all() {
                    if (entry.x - center.0.x).abs() > radius
                        || (entry.z - center.0.z).abs() > radius
                        || !matches(&entry.poi_type)
                    {
                        continue;
                    }

                    let dx = i64::from(entry.x - center.0.x);
                    let dy = i64::from(entry.y - center.0.y);
                    let dz = i64::from(entry.z - center.0.z);
                    let distance_sq = dx * dx + dy * dy + dz * dz;

                    if best.as_ref().is_none_or(|(_, _, d)| distance_sq < *d) {
                        best = Some((entry.pos(), entry.poi_type.clone(), distance_sq));
                    }
                }
            }
        }

        best.map(|(pos, poi_type, _)| (pos, poi_type))
    }

    pub fn save_all(&mut self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.folder)?;

        let mut saved = 0;
        let mut first_error = None;
        for ((rx, rz), region) in &mut self.regions {
            if region.is_dirty() || region.load_error.is_some() || !region.record_errors.is_empty()
            {
                let path = self.folder.join(format!("r.{rx}.{rz}.mca"));
                match region.save_with_config(&path, &self.config) {
                    Ok(()) => saved += 1,
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }

        if saved > 0 {
            info!("Saved {saved} POI region(s)");
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn synchronize(&self) -> std::io::Result<()> {
        for (&(x, z), region) in &self.regions {
            let path = self.region_path(x, z);
            if path.exists() {
                region.transport.synchronize(&path)?;
            }
        }
        Ok(())
    }

    /// Get count of loaded regions
    #[must_use]
    pub fn loaded_region_count(&self) -> usize {
        self.regions.len()
    }

    /// Get total POI count across all loaded regions
    #[must_use]
    pub fn total_poi_count(&self) -> usize {
        self.regions.values().map(|r| r.get_all().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanilla_poi_and_unsupported_fields_survive_portal_changes() -> std::io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("r.-1.-1.mca");
        let mut home = NbtCompound::new();
        home.put("pos", NbtTag::IntArray(vec![-1, 64, -1]));
        home.put_string("type", "minecraft:home".into());
        home.put_int("free_tickets", 1);
        home.put_string("example:record", "keep".into());
        let mut section = NbtCompound::new();
        section.put_bool("Valid", false);
        section.put_string("example:section", "keep".into());
        section.put_list("Records", vec![NbtTag::Compound(home.clone())]);
        let mut sections = NbtCompound::new();
        sections.put_compound("4", section);
        let mut root = NbtCompound::new();
        root.put_compound("Sections", sections);
        root.put_string("example:root", "keep".into());
        let mut transport = AnvilRegion::default();
        transport.set(
            1023,
            Some(RegionRecord::new(
                3,
                pumpkin_nbt::Nbt::from(root).write(),
                0,
            )),
        )?;
        transport.write(&path, false)?;
        let mut storage = PoiStorage::new(directory.path().to_path_buf());
        storage.add_portal(BlockPos::new(-2, 65, -1));
        storage.save_all()?;
        let transport = AnvilRegion::load(&path)?;
        let record = transport.records[1023].as_ref().unwrap();
        assert_eq!(record.compression, 3);
        let data = PoiRegion::decompress_chunk_data(&record.payload, record.compression)?;
        assert_eq!(data.retained.get_string("example:root"), Some("keep"));
        let section = &data.sections["4"];
        assert_eq!(section.valid, 0);
        assert_eq!(section.retained.get_string("example:section"), Some("keep"));
        let saved_home = section
            .records
            .iter()
            .find(|entry| entry.poi_type == "minecraft:home")
            .unwrap();
        assert_eq!(saved_home.retained, home);
        for entry in &section.records {
            assert!(entry.retained.has("pos"));
            assert!(!entry.retained.has("x"));
        }
        Ok(())
    }

    #[test]
    fn unreadable_poi_survives_saving_other_chunks() -> std::io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("r.0.0.mca");
        let payload = bytes::Bytes::from_static(b"invalid NBT");
        let mut transport = AnvilRegion::default();
        transport.set(0, Some(RegionRecord::new(3, payload.clone(), 0)))?;
        transport.write(&path, false)?;
        let mut storage = PoiStorage::new(directory.path().to_path_buf());
        storage.add_portal(BlockPos::new(16, 64, 0));
        assert!(storage.save_all().is_err());
        let transport = AnvilRegion::load(&path)?;
        assert_eq!(transport.records[0].as_ref().unwrap().payload, payload);
        assert!(transport.records[1].is_some());
        let damaged = vec![1u8; 8192];
        std::fs::write(directory.path().join("r.1.0.mca"), &damaged)?;
        storage.add_portal(BlockPos::new(512, 64, 0));
        assert!(storage.save_all().is_err());
        assert_eq!(std::fs::read(directory.path().join("r.1.0.mca"))?, damaged);
        Ok(())
    }

    #[test]
    fn poi_entry() {
        let entry = PoiEntry::new_portal(BlockPos(Vector3::new(100, 64, 200)));
        assert_eq!(entry.x, 100);
        assert_eq!(entry.y, 64);
        assert_eq!(entry.z, 200);
        assert_eq!(entry.poi_type, POI_TYPE_NETHER_PORTAL);
    }

    #[test]
    fn poi_region() {
        let mut region = PoiRegion::new();
        region.add(PoiEntry::new_portal(BlockPos(Vector3::new(100, 64, 200))));
        region.add(PoiEntry::new_portal(BlockPos(Vector3::new(101, 64, 200))));

        assert_eq!(region.get_all().len(), 2);
        assert!(region.is_dirty());

        region.remove(&BlockPos(Vector3::new(100, 64, 200)));
        assert_eq!(region.get_all().len(), 1);
    }

    #[test]
    fn poi_find_closest_matching() {
        let mut storage = PoiStorage::new(std::env::temp_dir().join("pumpkin_poi_closest_test"));

        storage.add_portal(BlockPos(Vector3::new(100, 64, 100)));
        storage.add_portal(BlockPos(Vector3::new(120, 64, 100)));
        storage.add(BlockPos(Vector3::new(101, 64, 100)), "minecraft:home");

        let center = BlockPos(Vector3::new(105, 64, 100));
        let (pos, poi_type) = storage
            .find_closest_matching(center, 256, |t| t == POI_TYPE_NETHER_PORTAL)
            .unwrap();
        assert_eq!(pos, BlockPos(Vector3::new(100, 64, 100)));
        assert_eq!(poi_type, POI_TYPE_NETHER_PORTAL);

        // The overall closest one ignores the type filter mismatch above.
        let (pos, poi_type) = storage
            .find_closest_matching(center, 256, |_| true)
            .unwrap();
        assert_eq!(pos, BlockPos(Vector3::new(101, 64, 100)));
        assert_eq!(poi_type, "minecraft:home");

        assert!(
            storage
                .find_closest_matching(center, 256, |t| t == "minecraft:lodestone")
                .is_none()
        );
        // Out of horizontal range.
        assert!(
            storage
                .find_closest_matching(BlockPos(Vector3::new(1000, 64, 100)), 16, |_| true)
                .is_none()
        );
    }

    #[test]
    fn poi_storage_mca() {
        let dir = std::env::temp_dir().join("pumpkin_poi_mca_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut storage = PoiStorage::new(dir.join("poi"));

        storage.add_portal(BlockPos(Vector3::new(100, 64, 100)));
        storage.add_portal(BlockPos(Vector3::new(110, 64, 100)));
        storage.add_portal(BlockPos(Vector3::new(1000, 64, 1000))); // Different region

        let results = storage.get_in_square(
            BlockPos(Vector3::new(105, 64, 100)),
            16,
            Some(POI_TYPE_NETHER_PORTAL),
        );
        assert_eq!(results.len(), 2);

        storage.save_all().unwrap();

        // Verify .mca file was created
        let mca_path = dir.join("poi").join("r.0.0.mca");
        assert!(mca_path.exists());

        // Reload and verify
        let mut storage2 = PoiStorage::new(dir.join("poi"));
        let results2 = storage2.get_in_square(
            BlockPos(Vector3::new(105, 64, 100)),
            16,
            Some(POI_TYPE_NETHER_PORTAL),
        );
        assert_eq!(results2.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
