// The Mining tab's model: the miner's JSON lines folded into what the tab shows,
// the profiles, and where the miner and the stratum server are looked for.

use plaine_wallet_gui::mining::{stratum_for, MinerState, Profile};

// Lines exactly as plaine-miner --status-format json wrote them against a live node.
const LIVE: [&str; 6] = [
    r#"{"event":"status","hashrate":5926,"avg":5923,"accepted":0,"rejected":0,"blocks":0,"job":"00000000","height":10956,"uptime":3,"threads":2}"#,
    r#"{"event":"status","hashrate":5928,"avg":5925,"accepted":0,"rejected":0,"blocks":0,"job":"00000000","height":10956,"uptime":6,"threads":2}"#,
    r#"{"event":"status","hashrate":5972,"avg":5941,"accepted":0,"rejected":0,"blocks":0,"job":"00000000","height":10956,"uptime":9,"threads":2}"#,
    r#"{"event":"share","accepted":true,"total_accepted":1,"total_rejected":0}"#,
    r#"{"event":"status","hashrate":5926,"avg":5937,"accepted":1,"rejected":0,"blocks":0,"job":"00000001","height":10956,"uptime":12,"threads":2}"#,
    r#"{"event":"summary","accepted":1,"rejected":0,"blocks":0,"hashes":89652,"avg":5936,"uptime":15}"#,
];

#[test]
fn the_miners_own_lines_fold_into_the_tab() {
    let mut s = MinerState::default();
    for l in &LIVE[..5] {
        s.apply(l);
    }
    assert_eq!(
        (s.hashrate, s.avg, s.accepted, s.rejected),
        (5926, 5937, 1, 0)
    );
    assert_eq!((s.height, s.uptime, s.threads), (10_956, 12, 2));
    s.apply(LIVE[5]);
    assert_eq!(s.hashrate, 0, "the summary means it stopped");
    assert_eq!(s.accepted, 1);
}

#[test]
fn a_rejected_share_and_a_block_are_shown() {
    let mut s = MinerState::default();
    s.apply(r#"{"event":"share","accepted":false,"total_accepted":3,"total_rejected":1,"code":23,"message":"low difficulty"}"#);
    assert_eq!((s.accepted, s.rejected), (3, 1));
    assert_eq!(
        s.last_error.as_deref(),
        Some("share rejected: low difficulty")
    );
    s.apply(r#"{"event":"block","height":77,"hash":"00ab"}"#);
    assert_eq!((s.blocks, s.height), (1, 77));
    s.apply("plaine-miner: something for people");
    s.apply("{not json");
    assert_eq!(s.blocks, 1, "other lines change nothing");
}

#[test]
fn profiles_leave_the_machine_usable_or_take_all_of_it() {
    assert_eq!(
        Profile::Background.args(16),
        ["--threads", "8", "--cpu-priority", "0"]
    );
    assert_eq!(
        Profile::Background.args(1),
        ["--threads", "1", "--cpu-priority", "0"]
    );
    assert_eq!(Profile::Maximum.args(16), ["--threads", "16"]);
}

#[test]
fn stratum_goes_to_the_nodes_host() {
    assert_eq!(stratum_for("127.0.0.1:9257"), "127.0.0.1:9258");
    assert_eq!(stratum_for("10.1.2.3:19257"), "10.1.2.3:9258");
    assert_eq!(stratum_for("node.lan"), "node.lan:9258");
}
