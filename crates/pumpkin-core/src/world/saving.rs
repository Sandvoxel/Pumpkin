use super::World;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::{
    chunk::io::{Dirtiable, FileIO, next_save_generation},
    level::{SyncChunk, SyncEntityChunk},
    poi::PoiStorage,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::oneshot;
use tracing::error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    Autosave,
    Manual,
    Flush,
    Shutdown,
}
impl SaveMode {
    const fn synchronize(self) -> bool {
        matches!(self, Self::Flush | Self::Shutdown)
    }
}

type Completion = oneshot::Sender<Result<u64, String>>;
#[derive(Default)]
pub(super) struct SaveState {
    requests: Mutex<Vec<(SaveMode, Completion)>>,
    pending: Mutex<BTreeMap<u64, Arc<Snapshot>>>,
    writer: tokio::sync::Mutex<()>,
    pub(super) autosave: AtomicBool,
    unload_requests: Mutex<FxHashSet<Vector2<i32>>>,
    unloaded_snapshots: Mutex<FxHashSet<Vector2<i32>>>,
    committed: AtomicU64,
    ticket_releases: Mutex<Vec<(Vector2<i32>, i8)>>,
}

struct Snapshot {
    generation: u64,
    mode: SaveMode,
    terrain: Vec<(Vector2<i32>, SyncChunk)>,
    sources: Vec<Weak<pumpkin_world::chunk::ChunkData>>,
    entities: FxHashMap<Vector2<i32>, Vec<NbtCompound>>,
    entity_sources: FxHashMap<Vector2<i32>, SyncEntityChunk>,
    live_entity_chunks: FxHashSet<Vector2<i32>>,
    poi: PoiStorage,
    custom_data: NbtCompound,
    completions: Mutex<Vec<Completion>>,
}

impl World {
    pub async fn save_with_mode(&self, mode: SaveMode) -> Result<u64, String> {
        let (sender, receiver) = oneshot::channel();
        self.save_state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((mode, sender));
        receiver
            .await
            .map_err(|error| format!("Save completion was interrupted: {error}"))?
    }

    pub(crate) fn defer_ticket_release(&self, pos: Vector2<i32>, level: i8) {
        self.save_state
            .ticket_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((pos, level));
    }

    pub(crate) fn queue_chunk_unload(&self, _pos: Vector2<i32>) {
        self.save_state.autosave.store(true, Ordering::Release);
    }

    pub(super) fn prepare_chunk_unload(&self, pos: Vector2<i32>) -> bool {
        if self
            .level
            .chunk_loading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pos_level
            .get(&pos)
            .is_some_and(|level| *level < pumpkin_world::chunk_system::ChunkLoading::MAX_LEVEL)
        {
            return false;
        }
        if self
            .save_state
            .unloaded_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&pos)
        {
            return true;
        }
        self.save_state
            .unload_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pos);
        false
    }

    fn capture_save(
        &self,
        mode: SaveMode,
        positions: Option<&FxHashSet<Vector2<i32>>>,
        completions: Vec<Completion>,
    ) -> Arc<Snapshot> {
        let generation = next_save_generation();
        let included =
            |pos: &Vector2<i32>| positions.is_none_or(|positions| positions.contains(pos));
        let blocks: Vec<_> = self
            .block_entities
            .iter()
            .map(|entry| *entry.key())
            .filter(&included)
            .collect();
        for pos in blocks {
            self.save_block_entities(pos);
        }
        let mut entities: FxHashMap<_, Vec<_>> = FxHashMap::default();
        for entity in self.entities.load().iter() {
            let base = entity.get_entity();
            let pos = base.chunk_pos.load();
            if !base.is_removed() && included(&pos) {
                let mut record = NbtCompound::new();
                entity.write_nbt(&mut record);
                entities.entry(pos).or_default().push(record);
            }
        }
        let live_entity_chunks: FxHashSet<_> = self
            .level
            .live_entity_chunk_positions()
            .into_iter()
            .filter(&included)
            .collect();
        for &pos in &live_entity_chunks {
            entities.entry(pos).or_default();
        }
        let mut terrain = Vec::new();
        let mut sources = Vec::new();
        for entry in self.level.loaded_chunks.iter() {
            if included(entry.key()) && entry.value().take_dirty() {
                terrain.push((*entry.key(), Arc::new(entry.value().snapshot(generation))));
                sources.push(Arc::downgrade(entry.value()));
            }
        }
        let entity_sources = entities
            .keys()
            .filter_map(|pos| {
                self.level
                    .get_entity_chunk_sync(pos)
                    .map(|chunk| (*pos, Arc::new(chunk.snapshot(generation))))
            })
            .collect();
        Arc::new(Snapshot {
            generation,
            mode,
            terrain,
            sources,
            entities,
            entity_sources,
            live_entity_chunks,
            poi: self
                .portal_poi
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            custom_data: self
                .custom_data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            completions: Mutex::new(completions),
        })
    }

    /// Called after entity and block entity ticks have joined.
    pub(super) fn process_save_requests(self: &Arc<Self>) {
        let requests = std::mem::take(
            &mut *self
                .save_state
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut unloading = std::mem::take(
            &mut *self
                .save_state
                .unload_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        {
            let loading = self
                .level
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            unloading.retain(|pos| {
                loading.pos_level.get(pos).is_none_or(|level| {
                    *level >= pumpkin_world::chunk_system::ChunkLoading::MAX_LEVEL
                })
            });
        };
        let releases = std::mem::take(
            &mut *self
                .save_state
                .ticket_releases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let autosave =
            self.save_state.autosave.swap(false, Ordering::AcqRel) || !releases.is_empty();
        if requests.is_empty() && unloading.is_empty() && !autosave {
            return;
        }
        let mode = if requests.iter().any(|(mode, _)| mode.synchronize()) {
            SaveMode::Flush
        } else if requests.is_empty() {
            SaveMode::Autosave
        } else {
            SaveMode::Manual
        };
        let positions = if autosave || !requests.is_empty() {
            None
        } else {
            Some(&unloading)
        };
        let snapshot = self.capture_save(
            mode,
            positions,
            requests.into_iter().map(|(_, sender)| sender).collect(),
        );
        self.save_state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(snapshot.generation, snapshot.clone());
        // Saved state exists before live objects or public chunks are removed.
        self.finish_chunk_unloads(&unloading, &snapshot);
        if !releases.is_empty() {
            let mut loading = self
                .level
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (pos, level) in releases {
                loading.remove_ticket(pos, level);
            }
            loading.send_change();
        }
        let world = self.clone();
        self.level.spawn_task(async move {
            if let Err(error) = world.persist_pending().await {
                error!("Failed saving world: {error}");
            }
        });
    }

    fn finish_chunk_unloads(&self, unloading: &FxHashSet<Vector2<i32>>, snapshot: &Snapshot) {
        if unloading.is_empty() {
            return;
        }

        let mut removed = Vec::new();
        self.entities.rcu(|current| {
            removed.clear();
            let mut next = (**current).clone();
            next.retain(|entity| {
                if unloading.contains(&entity.get_entity().chunk_pos.load()) {
                    removed.push(entity.clone());
                    false
                } else {
                    true
                }
            });
            next
        });
        for entity in removed {
            self.entity_tracker.remove_entity(entity.as_ref(), self);
            self.spawn_state.load().remove_entity(self, entity.as_ref());
        }
        for pos in unloading {
            self.block_entities.remove(pos);
        }
        self.save_state
            .unloaded_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(unloading.iter().copied());
        for &pos in unloading {
            if let Some(source) = snapshot.entity_sources.get(&pos) {
                let chunk = source.snapshot(0);
                let mut data = chunk
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                super::merge_entity_records(
                    &mut data,
                    snapshot.live_entity_chunks.contains(&pos),
                    snapshot.entities.get(&pos).cloned().unwrap_or_default(),
                );
                drop(data);
                chunk.live.store(false, Ordering::Release);
                chunk.mark_dirty(false);
                self.level.replace_entity_chunk(pos, Arc::new(chunk));
            }
        }
        self.level.level_channel.notify();
    }

    async fn persist_snapshot(&self, snapshot: &Snapshot) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = self.level.fence_chunk_writes().await {
            errors.push(error);
        }
        if let Err(error) = self
            .level
            .chunk_saver
            .save_chunks(&self.level.level_folder, snapshot.terrain.clone())
            .await
        {
            errors.push(error.to_string());
        }
        let mut entity_chunks: Vec<(Vector2<i32>, SyncEntityChunk)> = Vec::new();
        for (&pos, records) in &snapshot.entities {
            let existing = if let Some(chunk) = snapshot.entity_sources.get(&pos) {
                chunk.clone()
            } else {
                match self.level.get_entity_chunk(pos).await {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        errors.push(error.to_string());
                        continue;
                    }
                }
            };
            let chunk = existing.snapshot(snapshot.generation);
            let mut data = chunk
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            super::merge_entity_records(
                &mut data,
                snapshot.live_entity_chunks.contains(&pos),
                records.clone(),
            );
            drop(data);
            entity_chunks.push((pos, Arc::new(chunk)));
        }
        if let Err(error) = self.level.save_entity_chunks(entity_chunks).await {
            errors.push(error.to_string());
        }
        if snapshot.generation >= self.save_state.committed.load(Ordering::Acquire) {
            let mut poi = snapshot.poi.clone();
            let folder = self.level.level_folder.root_folder.clone();
            let custom = snapshot.custom_data.clone();
            let synchronize = snapshot.mode.synchronize();
            let data_result = tokio::task::spawn_blocking(move || {
                poi.save_all()?;
                if synchronize {
                    poi.synchronize()?;
                }
                let path = folder.join("pumpkin_custom_data.nbt");
                if !custom.is_empty() {
                    pumpkin_world::world_info::atomic_write(
                        &path,
                        &pumpkin_nbt::Nbt::from(custom).write(),
                    )?;
                }
                if synchronize && path.exists() {
                    std::fs::File::open(path)?.sync_all()?;
                }
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error: std::io::Error| error.to_string()));
            if let Err(error) = data_result {
                errors.push(error);
            }
        }
        if let Err(error) = self.level.flush_saves(snapshot.mode.synchronize()).await {
            errors.push(error.to_string());
        }
        let mut event = crate::plugin::api::events::world::world_save::WorldSaveEvent::new(
            format!("{:?}", self.dimension),
        );
        if matches!(snapshot.mode, SaveMode::Manual | SaveMode::Flush)
            && let Some(server) = self.server.upgrade()
        {
            server.plugin_manager.fire(&server, &mut event).await;
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn persist_pending(&self) -> Result<(), String> {
        let _writer = self.save_state.writer.lock().await;
        let mut attempted = FxHashSet::default();
        let mut first_error = None;
        loop {
            let next = self
                .save_state
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|(generation, _)| !attempted.contains(*generation))
                .map(|(_, snapshot)| snapshot.clone());
            let Some(snapshot) = next else {
                return first_error.map_or(Ok(()), Err);
            };
            attempted.insert(snapshot.generation);
            let result = self.persist_snapshot(&snapshot).await;
            let response = result
                .as_ref()
                .map(|()| snapshot.generation)
                .map_err(Clone::clone);
            for sender in snapshot
                .completions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain(..)
            {
                let _ = sender.send(response.clone());
            }
            if let Err(error) = result {
                for source in &snapshot.sources {
                    if let Some(source) = source.upgrade() {
                        source.mark_dirty(true);
                    }
                }
                first_error.get_or_insert(error);
            } else {
                self.save_state
                    .committed
                    .fetch_max(snapshot.generation, Ordering::Release);
                self.save_state
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&snapshot.generation);
            }
        }
    }

    pub(crate) async fn save_for_shutdown(&self) -> Result<(), String> {
        let requests = std::mem::take(
            &mut *self
                .save_state
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let snapshot = self.capture_save(
            SaveMode::Shutdown,
            None,
            requests.into_iter().map(|(_, sender)| sender).collect(),
        );
        self.save_state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(snapshot.generation, snapshot);
        self.persist_pending().await
    }
}
