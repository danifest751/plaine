#[path = "common/mod.rs"]
mod common;

use common::*;
use std::time::Duration;

// The stratum server listens on loopback unless the config opens it. A node on
// loopback has to say so, and how to open it, or an operator whose rigs sit on
// other machines is left wondering why they cannot connect.

fn log_of(n: &Node) -> String {
    let mut text = String::new();
    wait_until(
        "the node logs its stratum listener",
        Duration::from_secs(30),
        || {
            text = std::fs::read_to_string(&n.log).unwrap_or_default();
            text.contains("listening on")
        },
        |found| *found,
    );
    text
}

#[test]
fn a_loopback_stratum_says_how_to_open_it() {
    let dir = scratch("stratum-loopback");
    let data = dir.join("data");
    let cfg = write_config(&dir, &data, 20_551, 20_552, 20_553, &[]);
    let n = start("loopback", &dir, &cfg, 20_551, 20_552, 20_553);
    wait_for_rpc(&n, Duration::from_secs(30));

    let log = log_of(&n);
    assert!(log.contains("listening on 127.0.0.1:20553"), "{log}");
    assert!(
        log.contains("only miners on this machine can connect")
            && log.contains("listen = \"0.0.0.0:20553\""),
        "a loopback listener must say how to let other machines in:\n{log}"
    );
}

#[test]
fn an_open_stratum_gives_no_loopback_hint() {
    let dir = scratch("stratum-open");
    let data = dir.join("data");
    let cfg = write_config(&dir, &data, 20_561, 20_562, 20_563, &[]);
    let text = std::fs::read_to_string(&cfg).expect("read config");
    std::fs::write(&cfg, text.replace("127.0.0.1:20563", "0.0.0.0:20563")).expect("write config");
    let n = start("open", &dir, &cfg, 20_561, 20_562, 20_563);
    wait_for_rpc(&n, Duration::from_secs(30));

    let log = log_of(&n);
    assert!(log.contains("listening on 0.0.0.0:20563"), "{log}");
    assert!(!log.contains("only miners on this machine"), "{log}");
}
