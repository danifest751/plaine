//! The argon2id that plaine-wallet's `argon2id-v1` key files name but that crate
//! cannot link: its dependency policy admits no KDF library. This workspace can,
//! and installs it into plaine-wallet once, at start-up.

/// argon2id, version 0x13, 32 bytes of output.
pub fn argon2id(
    passphrase: &[u8],
    salt: &[u8],
    passes: u32,
    memory_kib: u32,
    lanes: u32,
) -> Result<[u8; 32], String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(memory_kib, passes, lanes, Some(32)).map_err(|e| e.to_string())?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase, salt, &mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Makes plaine-wallet seal and open `argon2id-v1` key files in this process.
/// Call it first thing in `main`; calling it again does nothing.
pub fn install() {
    plaine_wallet::kdf::install_argon2id(argon2id);
}
