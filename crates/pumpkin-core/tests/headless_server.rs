#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests fail by panicking"
)]

mod support;

use support::TestServer;

fn world_age(test_server: &TestServer) -> i64 {
    test_server
        .overworld()
        .level_time
        .lock()
        .expect("level time lock")
        .world_age
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_and_tick_void_world() {
    let mut test_server = TestServer::boot().await;
    let age_before = world_age(&test_server);

    test_server.step_n(20).await;

    assert!(test_server.world_dir().join("level.dat").exists());
    assert_eq!(world_age(&test_server), age_before + 20);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sequential_servers() {
    let first_dir = {
        let mut test_server = TestServer::boot().await;
        test_server.step_n(5).await;
        test_server.world_dir().to_path_buf()
    };

    let mut test_server = TestServer::boot().await;
    test_server.step_n(5).await;
    assert_ne!(first_dir, test_server.world_dir());
}
