// The desktop wallet lives in its own workspace so the node and the command-line
// wallet never link a GUI toolkit or a KDF library. Their lock file is the proof:
// if anything here leaks into it, this fails. The same idea as upstream's
// miner/tests/node_separation.rs.

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn the_node_and_wallet_lock_file_has_no_gui_and_no_kdf_library() {
    let lock = std::fs::read_to_string(root().join("Cargo.lock")).expect("read Cargo.lock");
    for name in [
        "egui",
        "eframe",
        "egui_kittest",
        "winit",
        "glow",
        "argon2",
        "blake2",
    ] {
        // Line by line: a Windows checkout may have turned the newlines into CRLF.
        let line = format!("name = \"{name}\"");
        assert!(
            !lock.lines().any(|l| l.trim() == line),
            "`{name}` is in the node's and wallet's Cargo.lock; it belongs to wallet-gui/ only"
        );
    }
}

#[test]
fn the_main_workspace_does_not_list_the_gui() {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).expect("read Cargo.toml");
    let members = manifest
        .split("members")
        .nth(1)
        .and_then(|m| m.split(']').next())
        .expect("a members list");
    assert!(
        !members.contains("wallet-gui"),
        "wallet-gui must stay out of the main workspace"
    );
}
