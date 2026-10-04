mod state;

pub use state::GameTestState;

use std::sync::Arc;
use std::time::Instant;

use pumpkin_util::math::position::BlockPos;

use crate::block_based::BlockBasedTest;
use crate::error::{GameTestError, GameTestResult};
use crate::model::GameTestRotation;
use crate::structure::{
    GameTestPosition, GameTestStructureTemplate, TestBlockMode, TestStructureInstance,
    clear_success_entities, encase_structure, place_structure_with_controller_rotation,
    remove_barriers,
};
use crate::world::GameTestWorld;

enum RunningEvaluation {
    Continue,
    Passed,
    Failed(GameTestError),
}

pub struct GameTestSession {
    pub test: BlockBasedTest,
    pub state: GameTestState,
    pub placement: Option<TestStructureInstance>,
    world: Arc<dyn GameTestWorld>,
    template: Arc<GameTestStructureTemplate>,
    extra_rotation: GameTestRotation,
    effective_rotation: GameTestRotation,
    test_x: i32,
    test_y: Option<i32>,
    test_z: i32,
    chunks_loaded: bool,
    chunk_wait_ticks: u32,
    started_at: Option<Instant>,
}

/// Vanilla waits indefinitely; a bound turns a never-loading test area into a failure.
pub const MAX_CHUNK_LOAD_WAIT_TICKS: u32 = 20 * 60;

impl GameTestSession {
    #[must_use]
    pub fn new(
        test: BlockBasedTest,
        world: Arc<dyn GameTestWorld>,
        template: Arc<GameTestStructureTemplate>,
        test_x: i32,
        test_z: i32,
    ) -> Self {
        Self::new_with_extra_rotation(
            test,
            world,
            template,
            test_x,
            test_z,
            GameTestRotation::None,
        )
    }

    #[must_use]
    pub fn new_with_extra_rotation(
        test: BlockBasedTest,
        world: Arc<dyn GameTestWorld>,
        template: Arc<GameTestStructureTemplate>,
        test_x: i32,
        test_z: i32,
        extra_rotation: GameTestRotation,
    ) -> Self {
        let effective_rotation = test.rotation().then(extra_rotation);
        Self {
            test,
            state: GameTestState::Queued,
            placement: None,
            world,
            template,
            extra_rotation,
            effective_rotation,
            test_x,
            test_y: None,
            test_z,
            chunks_loaded: false,
            chunk_wait_ticks: 0,
            started_at: None,
        }
    }

    /// Creates the equivalent of vanilla `GameTestInfo::copyReset()`.
    ///
    /// A rerun is a new execution object, not a finished run mutated back to Queued.
    /// The controller coordinates and resolved Y are retained so the replacement is
    /// prepared in place, while all per-attempt state and placement handles are fresh.
    #[must_use]
    pub fn copy_reset(&self) -> Self {
        Self {
            test: self.test.clone(),
            state: GameTestState::Queued,
            placement: None,
            world: self.world.clone(),
            template: self.template.clone(),
            extra_rotation: self.extra_rotation,
            effective_rotation: self.effective_rotation,
            test_x: self.test_x,
            test_y: self.test_y,
            test_z: self.test_z,
            chunks_loaded: false,
            chunk_wait_ticks: 0,
            started_at: None,
        }
    }

    #[must_use]
    pub(crate) fn run_time_ms(&self) -> u128 {
        self.started_at
            .as_ref()
            .map_or(0, |started_at| started_at.elapsed().as_millis())
    }

    pub async fn tick(&mut self) {
        if self.state.is_finished() {
            return;
        }

        // Move the current state out so state transitions can freely borrow `self`
        // across async calls without holding a borrow into `self.state`.
        let state = std::mem::replace(&mut self.state, GameTestState::Queued);
        match state {
            GameTestState::Queued => self.tick_queued().await,
            GameTestState::SettingUp { elapsed_ticks } => self.tick_setup(elapsed_ticks).await,
            GameTestState::Running { elapsed_ticks } => self.tick_running(elapsed_ticks).await,
            finished @ (GameTestState::Passed { .. } | GameTestState::Failed { .. }) => {
                self.state = finished;
            }
        }
    }

    async fn tick_queued(&mut self) {
        let placement = place_structure_with_controller_rotation(
            self.world.as_ref(),
            &self.template,
            self.test.id(),
            self.effective_rotation,
            self.extra_rotation,
            GameTestPosition::new(self.test_x, self.test_y, self.test_z),
            self.test.definition().padding,
        )
        .await;

        match placement {
            Ok(placement) => {
                self.test_y = Some(placement.test_instance_pos().0.y);
                let encased = encase_structure(
                    self.world.as_ref(),
                    &placement,
                    self.test.definition().sky_access,
                )
                .await;
                // Stored before reporting so an encase failure still marks the controller.
                self.placement = Some(placement);
                if let Err(error) = encased {
                    self.finish_failure(0, error, None).await;
                    return;
                }

                // Vanilla's StructureSpawner calls startExecution(1), so even a test
                // with zero setup ticks waits until the next server tick to start.
                self.state = GameTestState::SettingUp { elapsed_ticks: 0 };
            }
            Err(error) => self.finish_failure(0, error, None).await,
        }
    }

    async fn tick_setup(&mut self, elapsed_ticks: u32) {
        // GameTestInfo::tick does not advance tickCount until every chunk intersecting
        // the placed structure is actually loaded and ticking. This check is one-shot
        // per attempt, exactly like vanilla's chunksLoaded flag.
        if !self.chunks_loaded {
            let Some(placement) = &self.placement else {
                self.finish_failure(
                    0,
                    GameTestError::World(
                        "GameTest is ticking without a placed structure".to_string(),
                    ),
                    None,
                )
                .await;
                return;
            };
            let origin = placement.origin();
            let size = placement.size();
            let max = BlockPos::new(
                origin.0.x + size[0],
                origin.0.y + size[1],
                origin.0.z + size[2],
            );
            if !self.world.test_area_loaded_and_ticking(origin, &max).await {
                self.chunk_wait_ticks = self.chunk_wait_ticks.saturating_add(1);
                if self.chunk_wait_ticks > MAX_CHUNK_LOAD_WAIT_TICKS {
                    let error = GameTestError::ChunkLoadTimeout {
                        waited_ticks: MAX_CHUNK_LOAD_WAIT_TICKS,
                    };
                    self.finish_failure(0, error, None).await;
                    return;
                }
                self.state = GameTestState::SettingUp { elapsed_ticks };
                return;
            }
            self.chunks_loaded = true;
        }

        let elapsed_ticks = elapsed_ticks.saturating_add(1);
        if elapsed_ticks <= self.test.setup_ticks() {
            self.state = GameTestState::SettingUp { elapsed_ticks };
            return;
        }

        match self.begin_running(0).await {
            Ok(()) if self.test.max_ticks() > 0 => self.evaluate_test_tick(0).await,
            Ok(()) => self.state = GameTestState::Running { elapsed_ticks: 0 },
            Err(error) => self.finish_failure(0, error, None).await,
        }
    }

    async fn tick_running(&mut self, elapsed_ticks: u32) {
        let tick = elapsed_ticks.saturating_add(1);
        if tick > self.test.max_ticks() {
            self.finish_failure(
                tick,
                GameTestError::Timeout {
                    max_ticks: self.test.max_ticks(),
                },
                None,
            )
            .await;
            return;
        }

        // BlockBasedTestInstance installs onEachTick for the half-open range
        // [0, timeoutTicks), so timeoutTicks itself has no ACCEPT/FAIL/LOG check.
        if tick == self.test.max_ticks() {
            self.state = GameTestState::Running {
                elapsed_ticks: tick,
            };
            return;
        }

        self.evaluate_test_tick(tick).await;
    }

    async fn evaluate_test_tick(&mut self, tick: u32) {
        match self.evaluate_running(tick).await {
            Ok(RunningEvaluation::Passed) => self.handle_attempt_pass(tick).await,
            Ok(RunningEvaluation::Failed(error)) | Err(error) => {
                let marker = assertion_marker(&error);
                self.finish_failure(tick, error, marker).await;
            }
            Ok(RunningEvaluation::Continue) => {
                self.state = GameTestState::Running {
                    elapsed_ticks: tick,
                };
            }
        }
    }

    async fn begin_running(&mut self, tick: u32) -> GameTestResult<()> {
        // Vanilla starts GameTestInfo's stopwatch immediately before invoking the
        // test body, so structure placement and setup ticks are not part of run time.
        self.started_at.get_or_insert_with(Instant::now);

        let start_blocks = self.test_block_positions(TestBlockMode::Start);
        if start_blocks.is_empty() {
            return Err(GameTestError::Assertion {
                tick,
                position: None,
                message: "missing START test block".to_string(),
            });
        }
        if start_blocks.len() != 1 {
            return Err(GameTestError::Assertion {
                tick,
                position: None,
                message: format!(
                    "expected exactly one START test block, found {}",
                    start_blocks.len()
                ),
            });
        }

        if let Some(placement) = &self.placement {
            // GameTestInfo.startTest marks the controller RUNNING immediately before
            // invoking BlockBasedTestInstance.run, which triggers START.
            self.world
                .set_test_instance_running(placement.test_instance_pos())
                .await?;
        }
        self.world.trigger_test_block(&start_blocks[0]).await
    }

    async fn evaluate_running(&self, tick: u32) -> GameTestResult<RunningEvaluation> {
        let accept_blocks = self.test_block_positions(TestBlockMode::Accept);
        if accept_blocks.is_empty() {
            return Ok(RunningEvaluation::Failed(GameTestError::Assertion {
                tick,
                position: None,
                message: "missing ACCEPT test block".to_string(),
            }));
        }

        // Vanilla checks ACCEPT before FAIL; ACCEPT wins if both trigger this tick.
        for position in &accept_blocks {
            if self.world.test_block_triggered(position).await? {
                return Ok(RunningEvaluation::Passed);
            }
        }

        for position in self.test_block_positions(TestBlockMode::Fail) {
            if self.world.test_block_triggered(&position).await? {
                let message = self.world.test_block_message(&position).await?;
                return Ok(RunningEvaluation::Failed(GameTestError::Assertion {
                    tick,
                    position: Some(position),
                    message,
                }));
            }
        }

        for position in self.test_block_positions(TestBlockMode::Log) {
            if self.world.test_block_triggered(&position).await? {
                self.world.trigger_test_block(&position).await?;
                self.world.reset_test_block(&position).await?;
            }
        }

        Ok(RunningEvaluation::Continue)
    }

    async fn handle_attempt_pass(&mut self, tick: u32) {
        if let Some(placement) = &self.placement {
            // GameTestInfo::succeed removes non-player entities before the listeners
            // report success or schedule a copyReset rerun.
            if let Err(error) = clear_success_entities(self.world.as_ref(), placement).await {
                self.state = GameTestState::Failed { tick, error };
                return;
            }

            if let Err(error) = self
                .world
                .set_test_instance_success(placement.test_instance_pos())
                .await
            {
                self.state = GameTestState::Failed { tick, error };
                return;
            }

            // GameTestRunner's batch listener removes the test-instance barrier shell
            // on every passed execution, including executions that will be rerun.
            if let Err(error) = remove_barriers(
                self.world.as_ref(),
                placement,
                self.test.definition().sky_access,
            )
            .await
            {
                self.state = GameTestState::Failed { tick, error };
                return;
            }
        }
        self.state = GameTestState::Passed { tick };
    }

    async fn finish_failure(
        &mut self,
        tick: u32,
        error: GameTestError,
        marker: Option<(BlockPos, String)>,
    ) {
        if let Some(placement) = &self.placement {
            let message = error.to_string();
            if let Err(controller_error) = self
                .world
                .set_test_instance_failure(placement.test_instance_pos(), &message, marker)
                .await
            {
                self.state = GameTestState::Failed {
                    tick,
                    error: controller_error,
                };
                return;
            }
        }
        self.state = GameTestState::Failed { tick, error };
    }

    /// Ends an unfinished run as [`GameTestError::Stopped`], removing its barrier shell.
    /// World cleanup is best-effort: a stop must always end the run.
    pub(crate) async fn stop(&mut self) {
        if self.state.is_finished() {
            return;
        }
        let error = GameTestError::Stopped;
        if let Some(placement) = &self.placement {
            let _ = remove_barriers(
                self.world.as_ref(),
                placement,
                self.test.definition().sky_access,
            )
            .await;
            let _ = self
                .world
                .set_test_instance_failure(placement.test_instance_pos(), &error.to_string(), None)
                .await;
        }
        self.state = GameTestState::Failed { tick: 0, error };
    }

    fn test_block_positions(&self, mode: TestBlockMode) -> Vec<BlockPos> {
        let Some(placement) = &self.placement else {
            return Vec::new();
        };

        self.template
            .blocks()
            .iter()
            .filter(|block| block.test_mode == Some(mode))
            .map(|block| {
                placement.transform(&BlockPos::new(
                    block.position[0],
                    block.position[1],
                    block.position[2],
                ))
            })
            .collect()
    }
}

fn assertion_marker(error: &GameTestError) -> Option<(BlockPos, String)> {
    match error {
        GameTestError::Assertion {
            position: Some(position),
            message,
            ..
        } => Some((*position, message.clone())),
        _ => None,
    }
}

#[derive(Default)]
pub struct TestRunner {
    active: Vec<GameTestSession>,
}

impl TestRunner {
    #[must_use]
    pub const fn new() -> Self {
        Self { active: Vec::new() }
    }

    pub fn enqueue(&mut self, run: GameTestSession) {
        self.active.push(run);
    }

    pub async fn tick(&mut self) {
        for run in &mut self.active {
            run.tick().await;
        }
    }

    #[must_use]
    pub fn active(&self) -> &[GameTestSession] {
        &self.active
    }

    pub fn active_mut(&mut self) -> &mut [GameTestSession] {
        &mut self.active
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail by panicking")]
mod tests {
    use pumpkin_data::Block;
    use serde_json::json;

    use super::*;
    use crate::model::GameTestDefinition;
    use crate::structure::GameTestStructureBlock;
    use crate::testing::{ControllerStatus, MemoryGameTestWorld};

    const ACCEPT_POS: [i32; 3] = [0, 0, 1];

    fn test_block(position: [i32; 3], mode: TestBlockMode) -> GameTestStructureBlock {
        GameTestStructureBlock {
            position,
            state: Block::TEST_BLOCK.default_state.id,
            nbt: None,
            test_mode: Some(mode),
        }
    }

    fn start_accept_session(world: Arc<MemoryGameTestWorld>) -> GameTestSession {
        let definition: GameTestDefinition = serde_json::from_value(json!({
            "type": "minecraft:block_based",
            "environment": "minecraft:default",
            "structure": "test:start_accept",
            "max_ticks": 20,
        }))
        .expect("valid definition");
        let template = GameTestStructureTemplate::new(
            [1, 1, 2],
            vec![
                test_block([0, 0, 0], TestBlockMode::Start),
                test_block(ACCEPT_POS, TestBlockMode::Accept),
            ],
        );
        GameTestSession::new(
            BlockBasedTest::new("test:start_accept", definition),
            world,
            Arc::new(template),
            0,
            0,
        )
    }

    fn controller_pos(session: &GameTestSession) -> BlockPos {
        *session
            .placement
            .as_ref()
            .expect("structure placed")
            .test_instance_pos()
    }

    #[tokio::test]
    async fn passes_when_accept_block_triggers() {
        let world = Arc::new(MemoryGameTestWorld::default());
        let mut session = start_accept_session(world.clone());

        session.tick().await;
        session.tick().await;
        assert!(matches!(session.state, GameTestState::Running { .. }));

        let accept = session
            .placement
            .as_ref()
            .expect("structure placed")
            .transform(&BlockPos::new(ACCEPT_POS[0], ACCEPT_POS[1], ACCEPT_POS[2]));
        world.press_test_block(accept);
        session.tick().await;

        assert!(matches!(session.state, GameTestState::Passed { .. }));
        assert_eq!(
            world.controller_status(&controller_pos(&session)),
            Some(ControllerStatus::Passed)
        );
    }

    #[tokio::test]
    async fn encase_failure_marks_controller_failed() {
        let world = Arc::new(MemoryGameTestWorld::default());
        world.reject_placing(Block::BARRIER.default_state.id);
        let mut session = start_accept_session(world.clone());

        session.tick().await;

        assert!(matches!(
            session.state,
            GameTestState::Failed {
                error: GameTestError::World(_),
                ..
            }
        ));
        assert!(matches!(
            world.controller_status(&controller_pos(&session)),
            Some(ControllerStatus::Failed(_))
        ));
    }

    #[tokio::test]
    async fn setup_times_out_when_test_area_never_loads() {
        let world = Arc::new(MemoryGameTestWorld::default());
        world.set_area_loaded(false);
        let mut session = start_accept_session(world.clone());

        session.tick().await;
        for _ in 0..MAX_CHUNK_LOAD_WAIT_TICKS {
            session.tick().await;
            assert!(matches!(session.state, GameTestState::SettingUp { .. }));
        }
        session.tick().await;

        assert!(matches!(
            session.state,
            GameTestState::Failed {
                error: GameTestError::ChunkLoadTimeout { .. },
                ..
            }
        ));
        assert!(matches!(
            world.controller_status(&controller_pos(&session)),
            Some(ControllerStatus::Failed(_))
        ));
    }
}
