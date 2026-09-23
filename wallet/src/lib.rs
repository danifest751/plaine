#![forbid(unsafe_code)]

pub mod amount;
pub mod api;
pub mod args;
pub mod error;
pub mod genesis;
pub mod journal;
pub mod kdf;
pub mod keyfile;
pub mod rng;
pub mod sanitize;
pub mod sechex;
pub mod secret;
pub mod sig;
pub mod txbuild;
pub mod ui;
pub mod wallet_cli;

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
