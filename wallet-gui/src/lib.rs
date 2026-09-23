#![forbid(unsafe_code)]

//! The Plaine desktop wallet. Keys are handled by plaine-wallet's own code
//! (`plaine_wallet::api`); the node is reached over its local JSON-RPC. The
//! wallet holds no network connection of its own besides that one.

pub mod app;
pub mod kdf;
pub mod model;
pub mod rpc;
