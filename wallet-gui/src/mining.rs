//! Mining from the wallet: starts `plaine-miner` as a child process, paying to the
//! wallet's address, and reads its `--status-format json` lines. Mining needs no
//! key, so it keeps running while the wallet is locked; closing the wallet stops it.

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// How hard to mine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Half the processors at idle priority: the machine stays usable.
    Background,
    /// Every processor at the priority the OS gives.
    Maximum,
}

impl Profile {
    /// The miner's arguments for this profile on a machine with `cpus` processors.
    pub fn args(&self, cpus: usize) -> Vec<String> {
        match self {
            Profile::Background => vec![
                "--threads".into(),
                (cpus / 2).max(1).to_string(),
                "--cpu-priority".into(),
                "0".into(),
            ],
            Profile::Maximum => vec!["--threads".into(), cpus.max(1).to_string()],
        }
    }
}

/// What the miner last reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MinerState {
    pub running: bool,
    pub hashrate: u64,
    pub avg: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub blocks: u64,
    pub height: u64,
    pub uptime: u64,
    pub threads: u64,
    /// The last line the miner wrote to stderr: why it is not getting work, for one.
    pub last_error: Option<String>,
    /// How it ended, once it has.
    pub exit: Option<String>,
}

impl MinerState {
    /// Folds one line of `--status-format json` into the state. Lines that are
    /// not such an object are ignored.
    pub fn apply(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let n = |k: &str| v.get(k).and_then(Value::as_u64);
        match v.get("event").and_then(Value::as_str) {
            Some("status") => {
                self.hashrate = n("hashrate").unwrap_or(0);
                self.avg = n("avg").unwrap_or(0);
                self.accepted = n("accepted").unwrap_or(self.accepted);
                self.rejected = n("rejected").unwrap_or(self.rejected);
                self.blocks = n("blocks").unwrap_or(self.blocks);
                self.height = n("height").unwrap_or(self.height);
                self.uptime = n("uptime").unwrap_or(self.uptime);
                self.threads = n("threads").unwrap_or(self.threads);
            }
            Some("share") => {
                self.accepted = n("total_accepted").unwrap_or(self.accepted);
                self.rejected = n("total_rejected").unwrap_or(self.rejected);
                if v.get("accepted").and_then(Value::as_bool) == Some(false) {
                    let why = v.get("message").and_then(Value::as_str).unwrap_or("");
                    self.last_error = Some(format!("share rejected: {why}"));
                }
            }
            Some("block") => {
                self.blocks += 1;
                self.height = n("height").unwrap_or(self.height);
            }
            Some("summary") => {
                self.accepted = n("accepted").unwrap_or(self.accepted);
                self.rejected = n("rejected").unwrap_or(self.rejected);
                self.blocks = n("blocks").unwrap_or(self.blocks);
                self.hashrate = 0;
            }
            _ => {}
        }
    }
}

/// A running `plaine-miner`. Dropping it stops the miner.
pub struct Miner {
    child: Child,
    state: Arc<Mutex<MinerState>>,
}

impl Miner {
    /// Starts `exe` mining to `address` (with `rig` as the worker name) through the
    /// stratum server at `stratum`.
    pub fn start(
        exe: &Path,
        address: &str,
        rig: &str,
        stratum: &str,
        profile: Profile,
    ) -> std::io::Result<Miner> {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let login = if rig.trim().is_empty() {
            address.to_string()
        } else {
            format!("{address}.{}", rig.trim())
        };
        let mut cmd = Command::new(exe);
        cmd.args(["--address", &login, "--stratum", stratum.trim()])
            .args(profile.args(cpus))
            .args(["--status", "2", "--status-format", "json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        let state = Arc::new(Mutex::new(MinerState {
            running: true,
            ..MinerState::default()
        }));
        if let Some(out) = child.stdout.take() {
            let s = Arc::clone(&state);
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Ok(mut st) = s.lock() {
                        st.apply(&line);
                    }
                }
            });
        }
        if let Some(err) = child.stderr.take() {
            let s = Arc::clone(&state);
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let line = line.trim().trim_start_matches("plaine-miner: ").to_string();
                    if !line.is_empty() {
                        if let Ok(mut st) = s.lock() {
                            st.last_error = Some(line);
                        }
                    }
                }
            });
        }
        Ok(Miner { child, state })
    }

    /// What the miner last reported, and whether it is still running.
    pub fn state(&mut self) -> MinerState {
        if let Ok(Some(status)) = self.child.try_wait() {
            if let Ok(mut st) = self.state.lock() {
                if st.running {
                    st.running = false;
                    st.hashrate = 0;
                    st.exit = Some(match status.code() {
                        Some(0) => "stopped".to_string(),
                        Some(c) => format!("exited with code {c}"),
                        None => "stopped".to_string(),
                    });
                }
            }
        }
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(mut st) = self.state.lock() {
            st.running = false;
            st.hashrate = 0;
            st.exit = Some("stopped".into());
        }
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where the miner is looked for when Settings names none: beside this program.
pub fn default_miner_path() -> PathBuf {
    let name = if cfg!(windows) {
        "plaine-miner.exe"
    } else {
        "plaine-miner"
    };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// The stratum address that goes with a node's RPC address: same host, port 9258.
pub fn stratum_for(node_rpc: &str) -> String {
    let host = node_rpc
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(node_rpc);
    format!("{host}:9258")
}

/// A hash rate for people: `940 H/s`, `22.3 kH/s`, `1.25 MH/s`.
pub fn rate(h: u64) -> String {
    match h {
        h if h >= 1_000_000 => format!("{:.2} MH/s", h as f64 / 1e6),
        h if h >= 1_000 => format!("{:.1} kH/s", h as f64 / 1e3),
        h => format!("{h} H/s"),
    }
}
