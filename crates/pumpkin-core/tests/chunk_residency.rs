#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests fail by panicking"
)]

mod support;

use std::time::Duration;

use pumpkin_core::command::CommandSender;
use pumpkin_data::entity::EntityType;
use pumpkin_util::math::vector2::Vector2;
use support::TestServer;

/// The chunk scheduler unloads on its own thread, at least once per second of wall time.
const SCHEDULER_UNLOAD_PERIOD: Duration = Duration::from_millis(1_500);
/// Generating a chunk off the player-priority path takes ~10s in debug builds.
const CHUNK_LOAD_TIMEOUT: Duration = Duration::from_secs(60);

const FAR_CHUNK: Vector2<i32> = Vector2::new(40, 40);

async fn step_past_unload_sweeps(test_server: &mut TestServer) {
    for _ in 0..3 {
        test_server.step_n(100).await;
        tokio::time::sleep(SCHEDULER_UNLOAD_PERIOD).await;
    }
    test_server.step().await;
}

fn is_loaded_and_active(test_server: &TestServer, chunk: Vector2<i32>) -> bool {
    let world = test_server.overworld();
    world.level.is_chunk_loaded(&chunk)
        && world
            .active_chunks
            .read()
            .expect("active chunks lock")
            .contains(&chunk)
}

async fn wait_until_loaded_and_active(test_server: &mut TestServer, chunk: Vector2<i32>) {
    let deadline = tokio::time::Instant::now() + CHUNK_LOAD_TIMEOUT;
    while !is_loaded_and_active(test_server, chunk) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "chunk {chunk:?} not loaded within {CHUNK_LOAD_TIMEOUT:?}"
        );
        test_server.step().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn set_forced(test_server: &TestServer, chunk: Vector2<i32>, forced: bool) {
    let world = test_server.overworld();
    {
        let mut forced_chunks = world.forced_chunks.lock().expect("forced chunks lock");
        if forced {
            forced_chunks.insert(chunk);
        } else {
            forced_chunks.remove(&chunk);
        }
    }
    world.update_active_chunks();
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_chunk_loads_and_stays_loaded_without_players() {
    let mut test_server = TestServer::boot().await;
    set_forced(&test_server, FAR_CHUNK, true);

    wait_until_loaded_and_active(&mut test_server, FAR_CHUNK).await;
    step_past_unload_sweeps(&mut test_server).await;

    assert!(is_loaded_and_active(&test_server, FAR_CHUNK));
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_chunk_keeps_entities_past_memory_cleanup() {
    let mut test_server = TestServer::boot().await;
    set_forced(&test_server, FAR_CHUNK, true);
    wait_until_loaded_and_active(&mut test_server, FAR_CHUNK).await;

    let block_x = FAR_CHUNK.x * 16 + 8;
    let block_z = FAR_CHUNK.y * 16 + 8;
    let source = CommandSender::Console.into_source(&test_server.server);
    let dispatcher = test_server.server.command_dispatcher.load();
    dispatcher.handle_command(&source, &format!("setblock {block_x} 64 {block_z} bedrock"));
    dispatcher.handle_command(&source, &format!("summon cat {block_x} 65 {block_z}"));
    drop(dispatcher);

    step_past_unload_sweeps(&mut test_server).await;

    let world = test_server.overworld();
    let cats = world
        .entities
        .load()
        .iter()
        .filter(|entity| entity.get_entity().entity_type == &EntityType::CAT)
        .count();
    assert_eq!(cats, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unforced_chunk_unloads_without_players() {
    let mut test_server = TestServer::boot().await;
    let world = test_server.overworld();
    world.level.get_or_fetch_chunk(FAR_CHUNK, |_| ()).await;
    assert!(world.level.is_chunk_loaded(&FAR_CHUNK));

    step_past_unload_sweeps(&mut test_server).await;

    assert!(!world.level.is_chunk_loaded(&FAR_CHUNK));
}

#[tokio::test(flavor = "multi_thread")]
async fn released_forced_chunk_unloads() {
    let mut test_server = TestServer::boot().await;
    set_forced(&test_server, FAR_CHUNK, true);
    wait_until_loaded_and_active(&mut test_server, FAR_CHUNK).await;

    set_forced(&test_server, FAR_CHUNK, false);
    step_past_unload_sweeps(&mut test_server).await;

    assert!(!test_server.overworld().level.is_chunk_loaded(&FAR_CHUNK));
}
