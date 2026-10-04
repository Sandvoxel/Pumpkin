//! In-memory [`GameTestWorld`] for exercising the runner without a server.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use pumpkin_data::{Block, BlockStateId};
use pumpkin_nbt::NbtCompound;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::world::BlockFlags;

use crate::error::{GameTestError, GameTestResult};
use crate::model::GameTestRotation;
use crate::world::GameTestWorld;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControllerStatus {
    Running,
    Passed,
    Failed(String),
}

/// Flat world at y = 0 with every chunk loaded unless [`Self::set_area_loaded`] says otherwise.
pub struct MemoryGameTestWorld {
    blocks: Mutex<HashMap<BlockPos, BlockStateId>>,
    triggered_test_blocks: Mutex<HashSet<BlockPos>>,
    controllers: Mutex<HashMap<BlockPos, ControllerStatus>>,
    area_loaded: AtomicBool,
    rejected_state: Mutex<Option<BlockStateId>>,
}

impl Default for MemoryGameTestWorld {
    fn default() -> Self {
        Self {
            blocks: Mutex::default(),
            triggered_test_blocks: Mutex::default(),
            controllers: Mutex::default(),
            area_loaded: AtomicBool::new(true),
            rejected_state: Mutex::default(),
        }
    }
}

impl MemoryGameTestWorld {
    pub fn set_area_loaded(&self, loaded: bool) {
        self.area_loaded.store(loaded, Ordering::Release);
    }

    /// Makes every later attempt to place `state` fail with a world error.
    pub fn reject_placing(&self, state: BlockStateId) {
        *lock(&self.rejected_state) = Some(state);
    }

    pub fn press_test_block(&self, position: BlockPos) {
        lock(&self.triggered_test_blocks).insert(position);
    }

    #[must_use]
    pub fn controller_status(&self, position: &BlockPos) -> Option<ControllerStatus> {
        lock(&self.controllers).get(position).cloned()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[async_trait]
impl GameTestWorld for MemoryGameTestWorld {
    async fn block_state_id(&self, position: &BlockPos) -> BlockStateId {
        lock(&self.blocks)
            .get(position)
            .copied()
            .unwrap_or(Block::AIR.default_state.id)
    }

    async fn set_block_state(
        &self,
        position: &BlockPos,
        block_state_id: BlockStateId,
        _flags: BlockFlags,
    ) -> GameTestResult<()> {
        if *lock(&self.rejected_state) == Some(block_state_id) {
            return Err(GameTestError::World(format!(
                "rejected {block_state_id:?} at {position}"
            )));
        }
        lock(&self.blocks).insert(*position, block_state_id);
        Ok(())
    }

    async fn rotate_block_state(
        &self,
        block_state_id: BlockStateId,
        _rotation: GameTestRotation,
    ) -> GameTestResult<BlockStateId> {
        Ok(block_state_id)
    }

    async fn set_block_entity_nbt(
        &self,
        _position: &BlockPos,
        _nbt: &NbtCompound,
    ) -> GameTestResult<()> {
        Ok(())
    }

    async fn clear_non_player_entities(
        &self,
        _min: &BlockPos,
        _max: &BlockPos,
    ) -> GameTestResult<()> {
        Ok(())
    }

    async fn clear_scheduled_block_ticks(
        &self,
        _min: &BlockPos,
        _max: &BlockPos,
    ) -> GameTestResult<()> {
        Ok(())
    }

    async fn clear_block_events(&self, _min: &BlockPos, _max: &BlockPos) -> GameTestResult<()> {
        Ok(())
    }

    async fn test_area_loaded_and_ticking(&self, _min: &BlockPos, _max: &BlockPos) -> bool {
        self.area_loaded.load(Ordering::Acquire)
    }

    async fn set_test_instance_running(&self, position: &BlockPos) -> GameTestResult<()> {
        lock(&self.controllers).insert(*position, ControllerStatus::Running);
        Ok(())
    }

    async fn set_test_instance_success(&self, position: &BlockPos) -> GameTestResult<()> {
        lock(&self.controllers).insert(*position, ControllerStatus::Passed);
        Ok(())
    }

    async fn set_test_instance_failure(
        &self,
        position: &BlockPos,
        message: &str,
        _marker: Option<(BlockPos, String)>,
    ) -> GameTestResult<()> {
        lock(&self.controllers).insert(*position, ControllerStatus::Failed(message.to_string()));
        Ok(())
    }

    async fn trigger_test_block(&self, position: &BlockPos) -> GameTestResult<()> {
        lock(&self.triggered_test_blocks).insert(*position);
        Ok(())
    }

    async fn reset_test_block(&self, position: &BlockPos) -> GameTestResult<()> {
        lock(&self.triggered_test_blocks).remove(position);
        Ok(())
    }

    async fn test_block_triggered(&self, position: &BlockPos) -> GameTestResult<bool> {
        Ok(lock(&self.triggered_test_blocks).contains(position))
    }

    async fn test_block_message(&self, _position: &BlockPos) -> GameTestResult<String> {
        Ok(String::new())
    }

    async fn surface_height(&self, _x: i32, _z: i32) -> i32 {
        0
    }
}
