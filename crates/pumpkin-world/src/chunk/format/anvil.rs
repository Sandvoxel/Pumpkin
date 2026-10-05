use crate::chunk::{
    ChunkReadingError, ChunkSerializingError, ChunkWritingError, CompressionError,
    io::{
        ChunkSerializer, Dirtiable, LoadedData,
        region::{AnvilRegion, RegionRecord},
        run_blocking,
    },
};
use bytes::Bytes;
use flate2::read::{GzDecoder, GzEncoder, ZlibDecoder, ZlibEncoder};
use lz4_java_wrc::Context;
use pumpkin_config::chunk::AnvilChunkConfig;
use pumpkin_util::math::vector2::Vector2;
use std::{
    io::{Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub const REGION_SIZE: usize = 32;
pub const SUBREGION_BITS: u8 = pumpkin_util::math::ceil_log2(REGION_SIZE as u32);
pub const SUBREGION_AND: i32 = (1 << SUBREGION_BITS) - 1;
pub const CHUNK_COUNT: usize = REGION_SIZE * REGION_SIZE;
pub const WORLD_DATA_VERSION: i32 = crate::world_info::CURRENT_WORLD_DATA_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Compression {
    GZip = Self::GZIP_ID,
    ZLib = Self::ZLIB_ID,
    LZ4 = Self::LZ4_ID,
    Custom = Self::CUSTOM_ID,
}

pub enum CompressionRead<R: Read> {
    GZip(GzDecoder<R>),
    ZLib(ZlibDecoder<R>),
    LZ4(lz4_java_wrc::Lz4BlockInput<R>),
}
impl<R: Read> Read for CompressionRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::GZip(reader) => reader.read(buf),
            Self::ZLib(reader) => reader.read(buf),
            Self::LZ4(reader) => reader.read(buf),
        }
    }
}

pub struct AnvilChunkFile<S: SingleChunkDataSerializer> {
    region: Mutex<AnvilRegion>,
    write_in_place: bool,
    _dummy: PhantomData<S>,
}
impl<S: SingleChunkDataSerializer> Default for AnvilChunkFile<S> {
    fn default() -> Self {
        Self {
            region: Mutex::new(AnvilRegion::default()),
            write_in_place: true,
            _dummy: PhantomData,
        }
    }
}
impl Compression {
    const GZIP_ID: u8 = 1;
    const ZLIB_ID: u8 = 2;
    const NO_COMPRESSION_ID: u8 = 3;
    const LZ4_ID: u8 = 4;
    const CUSTOM_ID: u8 = 127;

    pub(crate) fn decompress_data(
        self,
        compressed_data: &[u8],
    ) -> Result<Box<[u8]>, CompressionError> {
        fn decode<R: std::io::Read>(mut reader: R, capacity: usize) -> std::io::Result<Box<[u8]>> {
            const MAX_CHUNK_BYTES: u64 = 512 * 1024 * 1024;
            let mut buf = Vec::with_capacity(capacity);
            reader
                .by_ref()
                .take(MAX_CHUNK_BYTES + 1)
                .read_to_end(&mut buf)?;
            if buf.len() as u64 > MAX_CHUNK_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Decompressed chunk exceeds size limit",
                ));
            }
            Ok(buf.into_boxed_slice())
        }

        let initial_capacity = compressed_data.len();

        match self {
            Self::GZip => decode(GzDecoder::new(compressed_data), initial_capacity)
                .map_err(CompressionError::GZipError),
            Self::ZLib => decode(ZlibDecoder::new(compressed_data), initial_capacity)
                .map_err(CompressionError::ZlibError),
            Self::LZ4 => decode(
                lz4_java_wrc::Lz4BlockInput::new(compressed_data),
                initial_capacity,
            )
            .map_err(CompressionError::LZ4Error),
            Self::Custom => Err(CompressionError::UnknownCompression),
        }
    }

    const LZ4_COMPRESSION_LEVEL_BASE: u32 = 10;
    pub(crate) fn compress_data(
        self,
        uncompressed_data: &[u8],
        compression_level: u32,
    ) -> Result<Vec<u8>, CompressionError> {
        match self {
            Self::GZip => {
                let mut encoder = GzEncoder::new(
                    uncompressed_data,
                    flate2::Compression::new(compression_level),
                );
                let mut chunk_data = Vec::new();
                encoder
                    .read_to_end(&mut chunk_data)
                    .map_err(CompressionError::GZipError)?;
                Ok(chunk_data)
            }
            Self::ZLib => {
                let mut encoder = ZlibEncoder::new(
                    uncompressed_data,
                    flate2::Compression::new(compression_level),
                );
                let mut chunk_data = Vec::new();
                encoder
                    .read_to_end(&mut chunk_data)
                    .map_err(CompressionError::ZlibError)?;
                Ok(chunk_data)
            }
            Self::LZ4 => {
                const MAGIC: &[u8; 8] = b"LZ4Block";
                const HEADER_LENGTH: usize = 21;
                const COMPRESSION_METHOD_RAW: u8 = 0x10;

                let mut compressed_data = Vec::new();
                let block_size = 1 << (Self::LZ4_COMPRESSION_LEVEL_BASE + compression_level);
                let mut encoder = lz4_java_wrc::Lz4BlockOutput::with_context(
                    &mut compressed_data,
                    Context::default(),
                    block_size,
                )
                .map_err(CompressionError::LZ4Error)?;
                encoder
                    .write_all(uncompressed_data)
                    .map_err(CompressionError::LZ4Error)?;
                encoder.flush().map_err(CompressionError::LZ4Error)?;
                drop(encoder);
                // lz4-java-wrc only flushes payload blocks; mirror LZ4BlockOutputStream.finish().
                let mut terminator = [0; HEADER_LENGTH];
                terminator[..MAGIC.len()].copy_from_slice(MAGIC);
                terminator[MAGIC.len()] = COMPRESSION_METHOD_RAW | compression_level as u8;
                compressed_data.extend_from_slice(&terminator);
                Ok(compressed_data)
            }
            Self::Custom => Err(CompressionError::UnknownCompression),
        }
    }

    /// Returns Ok when a compression is found otherwise an Err
    #[expect(clippy::result_unit_err)]
    pub const fn from_byte(byte: u8) -> Result<Option<Self>, ()> {
        match byte {
            Self::GZIP_ID => Ok(Some(Self::GZip)),
            Self::ZLIB_ID => Ok(Some(Self::ZLib)),
            // Uncompressed (since a version before 1.15.1)
            Self::NO_COMPRESSION_ID => Ok(None),
            Self::LZ4_ID => Ok(Some(Self::LZ4)),
            Self::CUSTOM_ID => Ok(Some(Self::Custom)),
            // Unknown format
            _ => Err(()),
        }
    }
}

impl From<pumpkin_config::chunk::Compression> for Compression {
    fn from(value: pumpkin_config::chunk::Compression) -> Self {
        // :c
        match value {
            pumpkin_config::chunk::Compression::GZip => Self::GZip,
            pumpkin_config::chunk::Compression::ZLib => Self::ZLib,
            pumpkin_config::chunk::Compression::LZ4 => Self::LZ4,
            pumpkin_config::chunk::Compression::Custom => Self::Custom,
        }
    }
}

pub trait SingleChunkDataSerializer: Send + Sync + Sized + Dirtiable + 'static {
    fn to_bytes(&self) -> Result<Bytes, ChunkSerializingError>;
    fn from_bytes(bytes: &Bytes, pos: Vector2<i32>) -> Result<Self, ChunkReadingError>;
    fn position(&self) -> (i32, i32);
}

impl<S: SingleChunkDataSerializer> AnvilChunkFile<S> {
    #[must_use]
    pub const fn get_region_coords(at: &Vector2<i32>) -> (i32, i32) {
        (at.x >> SUBREGION_BITS, at.y >> SUBREGION_BITS)
    }
    #[must_use]
    pub const fn get_chunk_index(x: i32, z: i32) -> usize {
        (((z & SUBREGION_AND) << SUBREGION_BITS) + (x & SUBREGION_AND)) as usize
    }
}

fn decode<S: SingleChunkDataSerializer>(
    record: &RegionRecord,
    pos: Vector2<i32>,
) -> Result<S, ChunkReadingError> {
    if let Some(error) = &record.load_error {
        return Err(ChunkReadingError::IoError(std::io::Error::other(
            error.clone(),
        )));
    }
    let compression = Compression::from_byte(record.compression)
        .map_err(|()| ChunkReadingError::Compression(CompressionError::UnknownCompression))?;
    if let Some(compression) = compression {
        let bytes = compression
            .decompress_data(&record.payload)
            .map_err(ChunkReadingError::Compression)?;
        S::from_bytes(&Bytes::from(bytes), pos)
    } else {
        S::from_bytes(&record.payload, pos)
    }
}

impl<S: SingleChunkDataSerializer> ChunkSerializer for AnvilChunkFile<S> {
    type Data = S;
    type WriteBackend = PathBuf;
    type ChunkConfig = AnvilChunkConfig;

    fn should_write(&self, _is_watched: bool) -> bool {
        true
    }
    fn get_chunk_key(chunk: &Vector2<i32>) -> String {
        let (x, z) = Self::get_region_coords(chunk);
        format!("./r.{x}.{z}.mca")
    }
    async fn write(&self, path: &PathBuf) -> Result<(), std::io::Error> {
        let mut guard = self.region.lock().await;
        let mut region = guard.clone();
        let path = path.clone();
        let in_place = self.write_in_place;
        let region = run_blocking(move || {
            region.write(&path, in_place)?;
            Ok::<_, std::io::Error>(region)
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))??;
        *guard = region;
        Ok(())
    }
    async fn synchronize(&self, path: &PathBuf) -> Result<(), std::io::Error> {
        let region = self.region.lock().await.clone();
        let path = path.clone();
        run_blocking(move || region.synchronize(&path))
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?
    }
    fn read(bytes: Bytes) -> Result<Self, ChunkReadingError> {
        Ok(Self {
            region: Mutex::new(AnvilRegion::read(&bytes).map_err(ChunkReadingError::IoError)?),
            ..Self::default()
        })
    }
    async fn load(path: &Path) -> Result<Self, ChunkReadingError> {
        let path = path.to_path_buf();
        let region = run_blocking(move || AnvilRegion::load(&path))
            .await
            .map_err(|error| ChunkReadingError::IoError(std::io::Error::other(error.to_string())))?
            .map_err(ChunkReadingError::IoError)?;
        Ok(Self {
            region: Mutex::new(region),
            ..Self::default()
        })
    }
    async fn update_chunk(
        &mut self,
        chunk: Arc<S>,
        config: &AnvilChunkConfig,
    ) -> Result<(), ChunkWritingError> {
        let (x, z) = chunk.position();
        let index = Self::get_chunk_index(x, z);
        let original = {
            let region = self.region.lock().await;
            if region.validated.contains(&index) {
                None
            } else {
                region.records[index].clone()
            }
        };
        if let Some(record) = original {
            let validation = run_blocking(move || decode::<S>(&record, Vector2::new(x, z)))
                .await
                .map_err(|error| {
                    ChunkWritingError::IoError(std::io::Error::other(error.to_string()))
                })?;
            let mut region = self.region.lock().await;
            if let Err(error) = validation {
                region.blocked.insert(index);
                return Err(ChunkWritingError::IoError(std::io::Error::other(
                    error.to_string(),
                )));
            }
            region.validated.insert(index);
        }
        let compression = self.region.lock().await.records[index]
            .as_ref()
            .map_or_else(
                || Some(config.compression.algorithm.into()),
                |record| Compression::from_byte(record.compression).ok().flatten(),
            );
        let config_snapshot = config.clone();
        let record = run_blocking(move || {
            let bytes = chunk
                .to_bytes()
                .map_err(|error| ChunkWritingError::ChunkSerializingError(error.to_string()))?;
            let payload = if let Some(compression) = compression {
                compression
                    .compress_data(&bytes, config_snapshot.compression.level)
                    .map_err(ChunkWritingError::Compression)?
                    .into()
            } else {
                bytes
            };
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;
            Ok::<_, ChunkWritingError>(RegionRecord::new(
                compression.map_or(3, |compression| compression as u8),
                payload,
                timestamp,
            ))
        })
        .await
        .map_err(|error| ChunkWritingError::IoError(std::io::Error::other(error.to_string())))??;
        self.region
            .lock()
            .await
            .set(index, Some(record))
            .map_err(ChunkWritingError::IoError)?;
        self.write_in_place = config.write_in_place;
        Ok(())
    }
    async fn get_chunks(
        &self,
        chunks: Vec<Vector2<i32>>,
        stream: tokio::sync::mpsc::Sender<LoadedData<S, ChunkReadingError>>,
    ) {
        let items: Vec<_> = {
            let region = self.region.lock().await;
            chunks
                .into_iter()
                .map(|pos| {
                    (
                        pos,
                        region.records[Self::get_chunk_index(pos.x, pos.y)].clone(),
                    )
                })
                .collect()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(items.len().max(1));
        rayon::spawn(move || {
            use rayon::prelude::*;
            items.into_par_iter().for_each(|(pos, record)| {
                let result = record.map_or_else(
                    || LoadedData::Missing(pos),
                    |record| match decode::<S>(&record, pos) {
                        Ok(chunk) => LoadedData::Loaded(chunk),
                        Err(error) => LoadedData::Error((pos, error)),
                    },
                );
                let _ = tx.blocking_send(result);
            });
        });
        while let Some(item) = rx.recv().await {
            match &item {
                LoadedData::Error((pos, _)) => {
                    self.region
                        .lock()
                        .await
                        .blocked
                        .insert(Self::get_chunk_index(pos.x, pos.y));
                }
                LoadedData::Loaded(chunk) => {
                    let (x, z) = chunk.position();
                    self.region
                        .lock()
                        .await
                        .validated
                        .insert(Self::get_chunk_index(x, z));
                }
                LoadedData::Missing(_) => {}
            }
            if stream.send(item).await.is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::io::region::RegionRecord;

    #[test]
    fn lz4_stream_matches_java_finish() -> Result<(), Box<dyn std::error::Error>> {
        // Generated with Minecraft 26.3's bundled lz4-java 1.10.1.
        let fixtures: &[(u32, &[u8], &[u8])] = &[
            (0, b"", b"LZ4Block\x10\0\0\0\0\0\0\0\0\0\0\0\0"),
            (
                6,
                b"Pumpkin",
                b"LZ4Block\x16\x07\0\0\0\x07\0\0\0\x56\x38\xc9\x09PumpkinLZ4Block\x16\0\0\0\0\0\0\0\0\0\0\0\0",
            ),
            (15, b"", b"LZ4Block\x1f\0\0\0\0\0\0\0\0\0\0\0\0"),
        ];
        for &(level, input, expected) in fixtures {
            assert_eq!(Compression::LZ4.compress_data(input, level)?, expected);
        }
        Ok(())
    }

    struct RawChunk(Bytes);
    impl Dirtiable for RawChunk {
        fn is_dirty(&self) -> bool {
            true
        }
        fn mark_dirty(&self, _flag: bool) {}
    }
    impl SingleChunkDataSerializer for RawChunk {
        fn position(&self) -> (i32, i32) {
            (0, 0)
        }
        fn to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
            Ok(self.0.clone())
        }
        fn from_bytes(bytes: &Bytes, _pos: Vector2<i32>) -> Result<Self, ChunkReadingError> {
            Ok(Self(bytes.clone()))
        }
    }

    #[tokio::test]
    async fn oversized_record_uses_vanilla_external_stub() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("r.0.0.mca");
        let mut state = 0x1234_5678u32;
        let bytes: Vec<u8> = (0..1_140_725)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let chunk = Arc::new(RawChunk(bytes.into()));
        let mut file = AnvilChunkFile::<RawChunk>::default();
        file.update_chunk(chunk, &AnvilChunkConfig::default())
            .await?;
        file.write(&path).await?;
        let bytes = std::fs::read(&path)?;
        assert_eq!(&bytes[..4], &[0, 0, 2, 1]);
        assert_eq!(&bytes[8192..8197], &[0, 0, 0, 1, 0x84]);
        assert!(directory.path().join("c.0.0.mcc").exists());
        Ok(())
    }

    #[test]
    fn external_payloads_decode_all_vanilla_compression_types()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("r.0.0.mca");
        let input = Bytes::from_static(b"independent payload");
        for id in 1..=4 {
            let compression = Compression::from_byte(id).map_err(|()| "compression")?;
            let payload = if let Some(compression) = compression {
                compression.compress_data(&input, 6)?.into()
            } else {
                input.clone()
            };
            // Independent vanilla framing: length=1, one sector, compression|0x80.
            let mut bytes = vec![0; 3 * 4096];
            bytes[..4].copy_from_slice(&0x201u32.to_be_bytes());
            bytes[8192..8196].copy_from_slice(&1u32.to_be_bytes());
            bytes[8196] = id | 0x80;
            std::fs::write(&path, bytes)?;
            std::fs::write(directory.path().join("c.0.0.mcc"), payload)?;
            let region = AnvilRegion::load(&path)?;
            let record: &RegionRecord = region.records[0].as_ref().ok_or("missing record")?;
            assert_eq!(decode::<RawChunk>(record, Vector2::new(0, 0))?.0, input);
        }
        Ok(())
    }
}

#[cfg(test)]
mod preservation_tests {
    use super::*;
    use crate::chunk::ChunkData;

    #[tokio::test]
    async fn invalid_nbt_record_cannot_be_replaced() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("r.0.0.mca");
        let mut region = AnvilRegion::default();
        region.set(
            0,
            Some(RegionRecord::new(3, Bytes::from_static(b"invalid NBT"), 1)),
        )?;
        region.write(&path, false)?;
        let original = std::fs::read(&path)?;
        let mut file = AnvilChunkFile::<ChunkData>::load(&path).await?;
        assert!(
            file.update_chunk(
                Arc::new(ChunkData::empty(0, 0)),
                &AnvilChunkConfig::default()
            )
            .await
            .is_err()
        );
        file.write(&path).await?;
        assert_eq!(std::fs::read(&path)?, original);
        Ok(())
    }
}
