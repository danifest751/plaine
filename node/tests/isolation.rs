#[path = "common/mod.rs"]
mod common;

use common::*;
use std::time::Duration;

// A node started by these tests must never reach the public network. With no
// seeds written, the config falls back to the embedded mainnet seeds, and a test
// node then downloads the real chain - which carries more work than anything a
// test mines, reorgs the test's chain away, and made ibd.rs fail whenever the
// real peers answered quickly enough.

fn dial_line(log: &std::path::Path) -> String {
    let mut line = String::new();
    wait_until(
        "the node logs how many seeds it dialled",
        Duration::from_secs(30),
        || {
            line = std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .find(|l| l.contains("seed address(es) dialled"))
                .unwrap_or_default()
                .to_string();
            !line.is_empty()
        },
        |found| *found,
    );
    line
}

#[test]
fn a_node_without_seeds_dials_nobody() {
    let dir = scratch("isolation-none");
    let data = dir.join("data");
    let cfg = write_config(&dir, &data, 20_401, 20_402, 20_403, &[]);
    let n = start("none", &dir, &cfg, 20_401, 20_402, 20_403);
    wait_for_rpc(&n, Duration::from_secs(30));

    let line = dial_line(&n.log);
    assert!(
        line.contains(" 0 seed address(es) dialled"),
        "a test node with no seeds dialled the embedded mainnet seeds: {line}"
    );
}

#[test]
fn a_node_with_a_local_seed_dials_only_that() {
    let dir = scratch("isolation-one");
    let data = dir.join("data");
    let cfg = write_config(&dir, &data, 20_411, 20_412, 20_413, &["127.0.0.1:20401".into()]);
    let n = start("one", &dir, &cfg, 20_411, 20_412, 20_413);
    wait_for_rpc(&n, Duration::from_secs(30));

    let line = dial_line(&n.log);
    assert!(
        line.contains(" 1 seed address(es) dialled"),
        "a test node given one local seed dialled something else as well: {line}"
    );
}
