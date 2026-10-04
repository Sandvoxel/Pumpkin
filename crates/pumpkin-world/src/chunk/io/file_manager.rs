use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::future::join_all;
use pumpkin_util::math::vector2::Vector2;
use tokio::{
    join,
    sync::{OnceCell, RwLock, mpsc},
};
use tracing::trace;

use crate::{
    chunk::{ChunkReadingError, ChunkWritingError, io::Dirtiable},
    level::LevelFolder,
};

use super::{ChunkSerializer, FileIO, LoadedData};

/// A simple implementation of the `ChunkSerializer` trait that loads and saves data
/// to disk using parallelism and a lazy-loading cache keyed by file path.
///
/// ### Concurrency model
///
/// * `file_locks` — one `Arc<RwLock<S>>` per on-disk file, created lazily.
///   All readers/writers for the same region file share this lock, so there
///   are never two concurrent writers for the same file.
/// * `watchers` — a ref-count per path.  While a path has active watchers the
///   serializer stays cached; save completion still commits its dirty records.
///
/// ### Lock ordering (must never be violated to avoid deadlocks)
///
/// 1. `file_locks`  (outer)
/// 2. individual `RwLock<S>` inside each loader  (inner)
/// 3. `watchers`  (independent — never held at the same time as either above)
///
/// `watchers` is always acquired in its own critical section, after all
/// serializer locks are released, which keeps it strictly independent.
type PendingChunks<D> = BTreeMap<PathBuf, rustc_hash::FxHashMap<Vector2<i32>, (u64, Arc<D>)>>;

pub struct ChunkFileManager<S: ChunkSerializer<WriteBackend = PathBuf>> {
    file_locks: RwLock<BTreeMap<PathBuf, Arc<ChunkSerializerLazyLoader<S>>>>,
    watchers: RwLock<BTreeMap<PathBuf, usize>>,
    chunk_config: S::ChunkConfig,
    pending: std::sync::Mutex<PendingChunks<S::Data>>,
    committed: std::sync::Mutex<rustc_hash::FxHashMap<(PathBuf, Vector2<i32>), u64>>,
    touched: std::sync::Mutex<std::collections::BTreeSet<PathBuf>>,
}

pub(crate) trait PathFromLevelFolder {
    fn file_path(folder: &LevelFolder, file_name: &str) -> PathBuf;
}

struct ChunkSerializerLazyLoader<S: ChunkSerializer<WriteBackend = PathBuf>> {
    path: PathBuf,
    /// Initialised at most once; subsequent calls reuse the same Arc.
    internal: OnceCell<Arc<RwLock<S>>>,
}

impl<S: ChunkSerializer<WriteBackend = PathBuf> + 'static> ChunkSerializerLazyLoader<S> {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            internal: OnceCell::new(),
        }
    }

    /// Returns `true` only when no outside caller still holds a clone of this
    /// loader *or* the inner serializer.
    ///
    /// # Safety requirement
    /// **Must be called while the write-lock on the parent `file_locks` map is
    /// held.**  That guarantees no new `Arc` clones can be issued while we
    /// inspect the strong counts.
    fn can_remove(loader: &Arc<Self>) -> bool {
        // The map itself holds 1 strong count; anything above that means an
        // active caller still has a handle.
        if Arc::strong_count(loader) > 1 {
            return false;
        }
        loader
            .internal
            .get()
            .is_none_or(|arc| Arc::strong_count(arc) == 1)
    }

    /// Returns the serializer, initialising it from disk on the first call.
    async fn get(&self) -> Result<Arc<RwLock<S>>, ChunkReadingError> {
        self.internal
            .get_or_try_init(|| async {
                let serializer = self.read_from_disk().await?;
                Ok(Arc::new(RwLock::new(serializer)))
            })
            .await
            .cloned()
    }

    async fn read_from_disk(&self) -> Result<S, ChunkReadingError> {
        trace!("Opening file from disk: {}", self.path.display());

        S::load(&self.path).await
    }
}

impl<S: ChunkSerializer<WriteBackend = PathBuf>> ChunkFileManager<S> {
    pub fn new(chunk_config: S::ChunkConfig) -> Self {
        Self {
            file_locks: RwLock::new(BTreeMap::new()),
            watchers: RwLock::new(BTreeMap::new()),
            chunk_config,
            pending: std::sync::Mutex::new(BTreeMap::new()),
            committed: std::sync::Mutex::new(rustc_hash::FxHashMap::default()),
            touched: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }
    }
}

impl<S: ChunkSerializer<WriteBackend = PathBuf>> ChunkFileManager<S> {
    /// Returns the serializer for `path`, inserting a lazy-loader if absent.
    ///
    /// Uses an optimistic read-first pattern: in the common case (cache hit)
    /// we never need a write-lock on the map.
    async fn get_serializer(&self, path: &Path) -> Result<Arc<RwLock<S>>, ChunkReadingError> {
        {
            let locks = self.file_locks.read().await;
            if let Some(loader) = locks.get(path) {
                // Clone the Arc *before* releasing the lock so it stays alive.
                let loader = loader.clone();
                drop(locks);
                return loader.get().await;
            }
        }

        let loader = {
            let mut locks = self.file_locks.write().await;
            locks
                .entry(path.into())
                .or_insert_with(|| Arc::new(ChunkSerializerLazyLoader::new(path.into())))
                .clone()
            // Write-lock dropped here — `loader.get()` may block on I/O and
            // must not hold the map lock.
        };

        loader.get().await
    }

    async fn commit_pending(
        &self,
        path: &PathBuf,
        writer: &mut S,
    ) -> Result<(), ChunkWritingError> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
            .unwrap_or_default();
        let mut applied = rustc_hash::FxHashSet::default();
        let mut first_error = None;
        for (pos, (_, chunk)) in &pending {
            chunk.take_dirty();
            match writer.update_chunk(chunk.clone(), &self.chunk_config).await {
                Ok(()) => {
                    applied.insert(*pos);
                }
                Err(error) => {
                    chunk.mark_dirty(true);
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Err(error) = writer.write(path).await {
            for (_, chunk) in pending.values() {
                chunk.mark_dirty(true);
            }
            return Err(ChunkWritingError::IoError(error));
        }
        {
            let mut committed = self
                .committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut all_pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entries) = all_pending.get_mut(path) {
                for pos in applied {
                    if let Some((generation, _)) = entries.remove(&pos) {
                        committed.insert((path.clone(), pos), generation);
                    }
                }
                if entries.is_empty() {
                    all_pending.remove(path);
                }
            }
            self.touched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.clone())
        };
        first_error.map_or(Ok(()), Err)
    }

    /// Attempt to evict the cached serializer for `path`.
    ///
    /// The entry is only removed when *both* conditions hold:
    /// 1. No watcher still references the path.
    /// 2. No other `Arc` clone is live (ensured via `can_remove`).
    async fn maybe_evict(&self, path: &PathBuf) {
        if self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(path)
        {
            return;
        }
        // Check watchers independently of file_locks to honour lock ordering.
        let still_watched = {
            let watchers = self.watchers.read().await;
            watchers.get(path).is_some_and(|&c| c > 0)
        };

        if still_watched {
            return;
        }

        let mut locks = self.file_locks.write().await;
        let removable = locks
            .get(path)
            .is_some_and(ChunkSerializerLazyLoader::can_remove);

        if removable {
            locks.remove(path);
            trace!("Evicted serializer cache for {}", path.display());
        } else {
            trace!(
                "Skipping eviction for {} — references still live",
                path.display()
            );
        }
    }
}

impl<P, S> FileIO for ChunkFileManager<S>
where
    P: PathFromLevelFolder + Send + Sync + Sized + Dirtiable + 'static,
    S: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    S::ChunkConfig: Send + Sync,
{
    type Data = Arc<S::Data>;

    async fn watch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        let paths: Vec<_> = chunks
            .iter()
            .map(|c| P::file_path(folder, &S::get_chunk_key(c)))
            .collect();

        let mut watchers = self.watchers.write().await;
        for path in paths {
            *watchers.entry(path).or_insert(0) += 1;
        }
    }

    async fn unwatch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        let paths: Vec<_> = chunks
            .iter()
            .map(|c| P::file_path(folder, &S::get_chunk_key(c)))
            .collect();

        let mut paths_to_evict = Vec::new();
        {
            let mut watchers = self.watchers.write().await;
            for path in paths {
                if let std::collections::btree_map::Entry::Occupied(mut e) = watchers.entry(path) {
                    let count = e.get_mut();
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        let (path, _) = e.remove_entry();
                        paths_to_evict.push(path);
                    }
                }
            }
        }

        for path in paths_to_evict {
            self.maybe_evict(&path).await;
        }
    }

    async fn clear_watched_chunks(&self) {
        let paths: Vec<PathBuf> = {
            let mut watchers = self.watchers.write().await;
            let keys: Vec<_> = watchers.keys().cloned().collect();
            watchers.clear();
            keys
        };
        for path in paths {
            self.maybe_evict(&path).await;
        }
    }

    async fn fetch_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunk_coords: &'a [Vector2<i32>],
        stream: mpsc::Sender<LoadedData<Self::Data, ChunkReadingError>>,
    ) {
        // Group requested chunk coords by their region file.
        let mut regions_chunks: BTreeMap<String, Vec<Vector2<i32>>> = BTreeMap::new();
        for at in chunk_coords {
            regions_chunks
                .entry(S::get_chunk_key(at))
                .or_default()
                .push(*at);
        }

        let region_tasks = regions_chunks.into_iter().map(|(file_name, chunks)| {
            let task_stream = stream.clone();
            async move {
                let path = P::file_path(folder, &file_name);

                let chunk_serializer = match self.get_serializer(&path).await {
                    Ok(s) => s,
                    Err(ChunkReadingError::ChunkNotExist) => {
                        return;
                    }
                    Err(err) => {
                        // Best-effort: report the error for the first coord in the batch.
                        let _ = task_stream.send(LoadedData::Error((chunks[0], err))).await;
                        return;
                    }
                };

                // A bounded channel of 1 keeps backpressure between the
                // serializer and the caller without unbounded buffering.
                let (send, mut recv) = mpsc::channel::<LoadedData<S::Data, ChunkReadingError>>(1);

                // Forward received chunks, wrapping them in `Arc`.
                // Captured move is intentional — `task_stream` is consumed here.
                let forward = async move {
                    while let Some(data) = recv.recv().await {
                        let wrapped = data.map_loaded(Arc::new);
                        if task_stream.send(wrapped).await.is_err() {
                            // Receiver dropped; abort early to avoid wasted work.
                            return;
                        }
                    }
                };

                // Hold the read lock only for the duration of `get_chunks`.
                let read = async move {
                    let serializer = chunk_serializer.read().await;
                    serializer.get_chunks(chunks, send).await;
                };

                join!(forward, read);

                // Evict if not watched and references are dropped
                self.maybe_evict(&path).await;
            }
        });

        join_all(region_tasks).await;
    }

    async fn save_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunks_data: Vec<(Vector2<i32>, Self::Data)>,
    ) -> Result<(), ChunkWritingError> {
        let mut regions: BTreeMap<String, Vec<_>> = BTreeMap::new();
        for (pos, chunk) in chunks_data {
            if chunk.is_dirty() {
                let generation = match chunk.save_generation() {
                    0 => super::next_save_generation(),
                    generation => generation,
                };
                regions
                    .entry(S::get_chunk_key(&pos))
                    .or_default()
                    .push((pos, generation, chunk));
            }
        }
        let tasks = regions.into_iter().map(|(name, chunks)| async move {
            let path = P::file_path(folder, &name);
            let serializer = self.get_serializer(&path).await.map_err(|error| {
                ChunkWritingError::IoError(std::io::Error::other(error.to_string()))
            })?;
            let mut writer = serializer.write().await;
            {
                let committed = self
                    .committed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let entries = pending.entry(path.clone()).or_default();
                for (pos, generation, chunk) in chunks {
                    if committed
                        .get(&(path.clone(), pos))
                        .is_some_and(|previous| *previous >= generation)
                    {
                        continue;
                    }
                    if entries
                        .get(&pos)
                        .is_none_or(|(previous, _)| *previous <= generation)
                    {
                        entries.insert(pos, (generation, chunk));
                    }
                }
            }
            let result = self.commit_pending(&path, &mut writer).await;
            drop(writer);
            drop(serializer);
            if result.is_ok() {
                self.maybe_evict(&path).await;
            }
            result
        });
        let mut first_error = None;
        for result in join_all(tasks).await {
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn flush(&self, synchronize: bool) -> Result<(), ChunkWritingError> {
        let mut first_error = None;
        let loaders: Vec<_> = self.file_locks.read().await.values().cloned().collect();
        for loader in loaders {
            match loader.get().await {
                Ok(serializer) => {
                    let mut writer = serializer.write().await;
                    if let Err(error) = self.commit_pending(&loader.path, &mut writer).await {
                        first_error.get_or_insert(error);
                    }
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        ChunkWritingError::IoError(std::io::Error::other(error.to_string()))
                    });
                }
            }
        }
        if synchronize {
            let paths: Vec<_> = self
                .touched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned()
                .collect();
            for path in paths {
                let result = async {
                    let serializer = self.get_serializer(&path).await.map_err(|error| {
                        ChunkWritingError::IoError(std::io::Error::other(error.to_string()))
                    })?;
                    serializer
                        .read()
                        .await
                        .synchronize(&path)
                        .await
                        .map_err(ChunkWritingError::IoError)
                }
                .await;
                match result {
                    Ok(()) => {
                        self.touched
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&path);
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Wait for serializers currently present in the cache. Save request acknowledgements
    /// and `flush` provide completion for queued writes.
    async fn block_and_await_ongoing_tasks(&self) {
        // Snapshot the current set of loaders under a read-lock so we do
        // not block new insertions longer than necessary.
        let loaders: Vec<Arc<ChunkSerializerLazyLoader<S>>> =
            { self.file_locks.read().await.values().cloned().collect() };

        // For each loader that has been initialised, acquire a write-lock
        // and release it immediately.  This guarantees that any concurrent
        // read or write operation that was in progress has finished.
        let drain_tasks = loaders.into_iter().map(|loader| async move {
            if let Some(serializer_arc) = loader.internal.get() {
                // Acquiring + immediately dropping the write-lock acts as a
                // barrier: it can only succeed once all current lock holders
                // have released their guards.
                let _guard = serializer_arc.write().await;
            }
        });

        join_all(drain_tasks).await;
    }
}

pub enum LevelFileIO<Linear, Anvil, Pump>
where
    Linear: ChunkSerializer<WriteBackend = PathBuf>,
    Anvil: ChunkSerializer<WriteBackend = PathBuf>,
    Pump: ChunkSerializer<WriteBackend = PathBuf>,
{
    Linear(ChunkFileManager<Linear>),
    Anvil(ChunkFileManager<Anvil>),
    Pump(ChunkFileManager<Pump>),
}

impl<P, Linear, Anvil, Pump> FileIO for LevelFileIO<Linear, Anvil, Pump>
where
    P: PathFromLevelFolder + Send + Sync + Sized + Dirtiable + 'static,
    Linear: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    Anvil: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    Pump: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    Linear::ChunkConfig: Send + Sync,
    Anvil::ChunkConfig: Send + Sync,
    Pump::ChunkConfig: Send + Sync,
{
    type Data = Arc<P>;

    async fn fetch_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunk_coords: &'a [Vector2<i32>],
        stream: tokio::sync::mpsc::Sender<LoadedData<Self::Data, ChunkReadingError>>,
    ) {
        match self {
            Self::Linear(io) => io.fetch_chunks(folder, chunk_coords, stream).await,
            Self::Anvil(io) => io.fetch_chunks(folder, chunk_coords, stream).await,
            Self::Pump(io) => io.fetch_chunks(folder, chunk_coords, stream).await,
        }
    }

    async fn save_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunks_data: Vec<(Vector2<i32>, Self::Data)>,
    ) -> Result<(), ChunkWritingError> {
        match self {
            Self::Linear(io) => io.save_chunks(folder, chunks_data).await,
            Self::Anvil(io) => io.save_chunks(folder, chunks_data).await,
            Self::Pump(io) => io.save_chunks(folder, chunks_data).await,
        }
    }

    async fn flush(&self, synchronize: bool) -> Result<(), ChunkWritingError> {
        match self {
            Self::Linear(io) => io.flush(synchronize).await,
            Self::Anvil(io) => io.flush(synchronize).await,
            Self::Pump(io) => io.flush(synchronize).await,
        }
    }

    async fn watch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        match self {
            Self::Linear(io) => io.watch_chunks(folder, chunks).await,
            Self::Anvil(io) => io.watch_chunks(folder, chunks).await,
            Self::Pump(io) => io.watch_chunks(folder, chunks).await,
        }
    }

    async fn unwatch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        match self {
            Self::Linear(io) => io.unwatch_chunks(folder, chunks).await,
            Self::Anvil(io) => io.unwatch_chunks(folder, chunks).await,
            Self::Pump(io) => io.unwatch_chunks(folder, chunks).await,
        }
    }

    async fn clear_watched_chunks(&self) {
        match self {
            Self::Linear(io) => io.clear_watched_chunks().await,
            Self::Anvil(io) => io.clear_watched_chunks().await,
            Self::Pump(io) => io.clear_watched_chunks().await,
        }
    }

    async fn block_and_await_ongoing_tasks(&self) {
        match self {
            Self::Linear(io) => io.block_and_await_ongoing_tasks().await,
            Self::Anvil(io) => io.block_and_await_ongoing_tasks().await,
            Self::Pump(io) => io.block_and_await_ongoing_tasks().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{
        ChunkSerializingError,
        format::anvil::{AnvilChunkFile, SingleChunkDataSerializer},
    };
    use bytes::Bytes;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Record {
        bytes: Bytes,
        dirty: AtomicBool,
        generation: u64,
    }
    impl Record {
        fn new(bytes: Bytes, generation: u64) -> Arc<Self> {
            Arc::new(Self {
                bytes,
                dirty: AtomicBool::new(true),
                generation,
            })
        }
    }
    impl Dirtiable for Record {
        fn is_dirty(&self) -> bool {
            self.dirty.load(Ordering::Acquire)
        }
        fn mark_dirty(&self, flag: bool) {
            self.dirty.store(flag, Ordering::Release);
        }
        fn take_dirty(&self) -> bool {
            self.dirty.swap(false, Ordering::AcqRel)
        }
        fn save_generation(&self) -> u64 {
            self.generation
        }
    }
    impl PathFromLevelFolder for Record {
        fn file_path(folder: &LevelFolder, name: &str) -> PathBuf {
            folder.region_folder.join(name)
        }
    }
    impl SingleChunkDataSerializer for Record {
        fn position(&self) -> (i32, i32) {
            (0, 0)
        }
        fn to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
            Ok(self.bytes.clone())
        }
        fn from_bytes(bytes: &Bytes, _pos: Vector2<i32>) -> Result<Self, ChunkReadingError> {
            Ok(Self {
                bytes: bytes.clone(),
                dirty: AtomicBool::new(false),
                generation: 0,
            })
        }
    }
    fn folder(root: &Path) -> LevelFolder {
        LevelFolder {
            root_folder: root.into(),
            dim_folder: root.into(),
            region_folder: root.into(),
            entities_folder: root.into(),
            poi_folder: root.into(),
        }
    }
    async fn read(
        manager: &ChunkFileManager<AnvilChunkFile<Record>>,
        folder: &LevelFolder,
    ) -> Result<Bytes, Box<dyn std::error::Error>> {
        let (tx, mut rx) = mpsc::channel(1);
        manager
            .fetch_chunks(folder, &[Vector2::new(0, 0)], tx)
            .await;
        match rx.recv().await {
            Some(LoadedData::Loaded(record)) => Ok(record.bytes.clone()),
            Some(LoadedData::Error((_, error))) => Err(error.into()),
            _ => Err("Missing record".into()),
        }
    }

    #[tokio::test]
    async fn watched_saves_complete_and_stale_snapshots_cannot_overwrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let folder = folder(directory.path());
        let manager = ChunkFileManager::<AnvilChunkFile<Record>>::new(
            pumpkin_config::chunk::AnvilChunkConfig::default(),
        );
        let pos = Vector2::new(0, 0);
        manager.watch_chunks(&folder, &[pos]).await;
        manager
            .save_chunks(
                &folder,
                vec![(pos, Record::new(Bytes::from_static(b"newer"), 2))],
            )
            .await?;
        assert!(folder.region_folder.join("r.0.0.mca").exists());
        manager
            .save_chunks(
                &folder,
                vec![(pos, Record::new(Bytes::from_static(b"older"), 1))],
            )
            .await?;
        manager.flush(true).await?;
        assert_eq!(read(&manager, &folder).await?, Bytes::from_static(b"newer"));
        Ok(())
    }

    #[tokio::test]
    async fn failed_save_retains_dirty_snapshot_for_flush_retry()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let folder = folder(directory.path());
        let manager = ChunkFileManager::<AnvilChunkFile<Record>>::new(
            pumpkin_config::chunk::AnvilChunkConfig::default(),
        );
        let pos = Vector2::new(0, 0);
        manager
            .save_chunks(
                &folder,
                vec![(pos, Record::new(Bytes::from_static(b"original"), 1))],
            )
            .await?;
        let original = std::fs::read(directory.path().join("r.0.0.mca"))?;
        let mut state = 0x1234_5678u32;
        let payload: Vec<_> = (0..1_140_725)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let record = Record::new(payload.into(), 2);
        let external = directory.path().join("c.0.0.mcc");
        std::fs::create_dir(&external)?;
        assert!(
            manager
                .save_chunks(&folder, vec![(pos, record.clone())])
                .await
                .is_err()
        );
        assert!(record.is_dirty());
        assert_eq!(std::fs::read(directory.path().join("r.0.0.mca"))?, original);
        manager.unwatch_chunks(&folder, &[pos]).await;
        std::fs::remove_dir(&external)?;
        manager.flush(true).await?;
        assert_eq!(read(&manager, &folder).await?, record.bytes);
        Ok(())
    }
}
