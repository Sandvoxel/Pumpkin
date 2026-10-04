#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests fail by panicking"
)]

mod support;

use pumpkin_core::server::server_test_manager::stop_game_tests;
use support::TestServer;

const SUMMON_TEST: &str = "pumpkin:summon_command_regression";
const TICK_BUDGET: u32 = 200;

#[tokio::test(flavor = "multi_thread")]
async fn stop_before_start_settles_every_queued_test() {
    let mut test_server = TestServer::boot().await;
    let batch = test_server.enqueue_game_tests(&[SUMMON_TEST, SUMMON_TEST]);

    stop_game_tests();
    let outcome = test_server.run_until_complete(batch, TICK_BUDGET).await;

    assert_eq!(outcome.report.failed_required(), 0);
    assert_eq!(outcome.count_messages_containing("was stopped"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_mid_run_settles_report_and_ends_run() {
    let mut test_server = TestServer::boot().await;
    let batch = test_server.enqueue_game_tests(&[SUMMON_TEST]);
    test_server.step_n(2).await;

    stop_game_tests();
    let outcome = test_server.run_until_complete(batch, TICK_BUDGET).await;

    assert_eq!(outcome.report.failed_required(), 0);
    assert_eq!(
        outcome.count_messages_containing("was stopped"),
        1,
        "{:?}",
        outcome.messages
    );
    assert_eq!(
        outcome.count_messages_containing(&format!("{SUMMON_TEST} passed")),
        0
    );
}
