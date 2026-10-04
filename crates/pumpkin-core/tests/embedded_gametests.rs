#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests fail by panicking"
)]

mod support;

use pumpkin_gametest::TestType;
use support::TestServer;

const TICK_BUDGET: u32 = 2_000;

/// Depends on unseeded mob pathing; passes roughly 1 run in 6 until world RNG is seedable.
const NONDETERMINISTIC_TESTS: &[&str] = &["pumpkin:creeper_should_run_from_cat"];

#[tokio::test(flavor = "multi_thread")]
#[ignore = "mob AI uses unseeded RNG; see NONDETERMINISTIC_TESTS"]
async fn creeper_should_run_from_cat() {
    let mut test_server = TestServer::boot().await;
    test_server
        .run_game_tests(&["pumpkin:creeper_should_run_from_cat"], TICK_BUDGET)
        .await
        .assert_all_required_passed();
}

#[tokio::test(flavor = "multi_thread")]
async fn summon_command_regression() {
    let mut test_server = TestServer::boot().await;
    test_server
        .run_game_tests(&["pumpkin:summon_command_regression"], TICK_BUDGET)
        .await
        .assert_all_required_passed();
}

#[tokio::test(flavor = "multi_thread")]
async fn all_embedded_block_based_tests_pass() {
    let mut test_server = TestServer::boot().await;
    let datapacks = &test_server.server.datapack_manager;
    let test_ids: Vec<String> = datapacks
        .get_test_instance_names()
        .into_iter()
        .filter(|id| !NONDETERMINISTIC_TESTS.contains(&id.as_str()))
        .filter(|id| {
            datapacks.get_test_instance(id).is_some_and(|instance| {
                instance.instance_type == TestType::BlockBased && !instance.manual_only
            })
        })
        .collect();
    assert!(
        !test_ids.is_empty(),
        "no embedded block-based GameTests found"
    );

    let test_ids: Vec<&str> = test_ids.iter().map(String::as_str).collect();
    test_server
        .run_game_tests(&test_ids, TICK_BUDGET)
        .await
        .assert_all_required_passed();
}
