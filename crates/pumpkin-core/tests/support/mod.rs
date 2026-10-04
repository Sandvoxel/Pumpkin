//! In-process server for integration tests: temp void world, no sockets, manual ticking.
#![allow(
    dead_code,
    reason = "shared by several test binaries, each using a different subset"
)]

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, RwLock};

use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use pumpkin_core::data::VanillaData;
use pumpkin_core::data::banned_ip::BannedIpList;
use pumpkin_core::data::banned_player::BannedPlayerList;
use pumpkin_core::data::op::OperatorConfig;
use pumpkin_core::data::usercache::UserCache;
use pumpkin_core::data::whitelist::WhitelistConfig;
use pumpkin_core::server::Server;
use pumpkin_core::server::server_test_manager::{
    GameTestBatchReport, GameTestQueueEntry, GameTestRetryOptions, enqueue_game_test,
    tick_game_tests,
};
use pumpkin_core::world::World;
use pumpkin_data::dimension::Dimension;
use pumpkin_gametest::{GameTestReporter, GameTestRunner};
use pumpkin_util::text::TextComponent;
use pumpkin_util::world_seed::Seed;
use pumpkin_world::world_info::anvil::AnvilLevelInfo;
use pumpkin_world::world_info::{LevelData, WorldInfoWriter};
use tempfile::TempDir;

/// `GameTest` queue and chunk leases are process globals, so servers must not overlap.
static SERVER_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

const TEST_SEED: Seed = Seed(0);

pub struct TestServer {
    pub server: Arc<Server>,
    runner: GameTestRunner,
    world_dir: TempDir,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl TestServer {
    pub async fn boot() -> Self {
        let lock = SERVER_LOCK.lock().await;
        let world_dir = tempfile::tempdir().expect("create temp world dir");

        let basic_config = BasicConfiguration {
            seed: TEST_SEED,
            default_level_name: world_dir.path().to_string_lossy().into_owned(),
            ..BasicConfiguration::default()
        };
        let mut advanced_config = AdvancedConfiguration::default();
        advanced_config.networking.java.enabled = false;
        advanced_config.networking.bedrock.enabled = false;
        advanced_config.networking.bedrock.online_mode = false;
        let telemetry_config = TelemetryConfig {
            enabled: false,
            ..TelemetryConfig::default()
        };

        AnvilLevelInfo
            .write_world_info(
                &LevelData::default_with_preset(TEST_SEED, "the_void"),
                world_dir.path(),
            )
            .expect("write void level.dat");

        let server = Server::new(
            basic_config,
            advanced_config,
            telemetry_config,
            in_memory_vanilla_data(),
            Vec::new(),
        )
        .await
        .expect("boot server");

        Self {
            server,
            runner: GameTestRunner::new(),
            world_dir,
            _lock: lock,
        }
    }

    pub fn world_dir(&self) -> &Path {
        self.world_dir.path()
    }

    pub fn overworld(&self) -> Arc<World> {
        self.server.get_world_from_dimension(&Dimension::OVERWORLD)
    }

    pub async fn step(&mut self) {
        self.server.tick();
        tick_game_tests(&self.server, &mut self.runner).await;
    }

    pub async fn step_n(&mut self, ticks: u32) {
        for _ in 0..ticks {
            self.step().await;
        }
    }

    /// Runs `test_ids` as one batch at the origin and steps until the batch reports
    /// completion. Panics if it hasn't finished within `tick_budget` ticks.
    pub async fn run_game_tests(&mut self, test_ids: &[&str], tick_budget: u32) -> BatchOutcome {
        let batch = self.enqueue_game_tests(test_ids);
        self.run_until_complete(batch, tick_budget).await
    }

    /// Queues `test_ids` as one batch, laid out in a row along +x from the origin.
    pub fn enqueue_game_tests(&self, test_ids: &[&str]) -> PendingBatch {
        let reporter = Arc::new(RecordingReporter::default());
        let report = Arc::new(GameTestBatchReport::new(reporter.clone(), test_ids.len()));
        let world = self.overworld();

        for (index, test_id) in test_ids.iter().enumerate() {
            let offset = i32::try_from(index).expect("test count fits i32") * TEST_SPACING;
            enqueue_game_test(
                GameTestQueueEntry::new(
                    *test_id,
                    world.clone(),
                    offset,
                    0,
                    0,
                    GameTestRetryOptions::new(1, false),
                    report.clone(),
                )
                .with_per_test_reporter(reporter.clone()),
            );
        }

        PendingBatch { report, reporter }
    }

    pub async fn run_until_complete(
        &mut self,
        batch: PendingBatch,
        tick_budget: u32,
    ) -> BatchOutcome {
        let PendingBatch { report, reporter } = batch;
        for _ in 0..tick_budget {
            if report.is_complete() {
                return BatchOutcome {
                    report,
                    messages: reporter.take(),
                };
            }
            self.step().await;
        }

        panic!(
            "GameTest batch did not finish within {tick_budget} ticks; messages: {:?}",
            reporter.take()
        );
    }
}

const TEST_SPACING: i32 = 64;

pub struct PendingBatch {
    report: Arc<GameTestBatchReport>,
    reporter: Arc<RecordingReporter>,
}

pub struct BatchOutcome {
    pub report: Arc<GameTestBatchReport>,
    pub messages: Vec<String>,
}

impl BatchOutcome {
    pub fn count_messages_containing(&self, needle: &str) -> usize {
        self.messages
            .iter()
            .filter(|message| message.contains(needle))
            .count()
    }

    pub fn assert_all_required_passed(&self) {
        assert_eq!(
            self.report.failed_required(),
            0,
            "required GameTests failed: {:?}",
            self.messages
        );
    }
}

#[derive(Default)]
struct RecordingReporter {
    messages: StdMutex<Vec<String>>,
}

impl RecordingReporter {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.messages.lock().expect("reporter lock"))
    }
}

impl GameTestReporter for RecordingReporter {
    fn send_message(&self, message: TextComponent) {
        self.messages
            .lock()
            .expect("reporter lock")
            .push(message.get_text());
    }
}

/// `VanillaData::load` reads and writes JSON files in the process working directory.
fn in_memory_vanilla_data() -> VanillaData {
    VanillaData {
        banned_ip_list: RwLock::new(BannedIpList::default()),
        banned_player_list: RwLock::new(BannedPlayerList::default()),
        operator_config: RwLock::new(OperatorConfig::default()),
        user_cache: RwLock::new(UserCache::default()),
        whitelist_config: RwLock::new(WhitelistConfig::default()),
    }
}
