#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Node {
    pub child: Child,
    pub rpc: u16,
    pub p2p: u16,
    pub stratum: u16,
    pub dir: PathBuf,
    pub log: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("plaine-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

pub fn write_config(dir: &Path, data: &Path, p2p: u16, rpc: u16, stratum: u16, seeds: &[String]) -> PathBuf {
    let seeds = if seeds.is_empty() {
        String::new()
    } else {
        format!(
            "seeds = [{}]\n",
            seeds.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ")
        )
    };
    let text = format!(
        "[node]\nnetwork = \"main\"\ndata_dir = {:?}\n\n[p2p]\nlisten = \"127.0.0.1:{p2p}\"\nuse_embedded_seeds = false\n{seeds}\n\
         [rpc]\nlisten = \"127.0.0.1:{rpc}\"\n\n[stratum]\nlisten = \"127.0.0.1:{stratum}\"\n",
        data.display().to_string().replace('\\', "/")
    );
    let p = dir.join("noded.toml");
    std::fs::write(&p, text).expect("write config");
    p
}

pub fn start(name: &str, dir: &Path, config: &Path, p2p: u16, rpc: u16, stratum: u16) -> Node {
    let log = dir.join(format!("{name}.log"));
    let out = std::fs::File::create(&log).expect("log file");
    let err = out.try_clone().expect("log clone");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_plaine-noded"));
    cmd.arg("--config").arg(config).stdout(Stdio::from(out)).stderr(Stdio::from(err));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn().expect("spawn plaine-noded");
    let n = Node {
        child,
        rpc,
        p2p,
        stratum,
        dir: dir.to_path_buf(),
        log,
    };
    wait_for_rpc(&n, Duration::from_secs(60));
    n
}

pub fn wait_for_rpc(n: &Node, within: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if rpc(n.rpc, "chain_getInfo", "[]").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "the node never answered RPC on 127.0.0.1:{} within {within:?}\n--- its log ---\n{}",
        n.rpc,
        std::fs::read_to_string(&n.log).unwrap_or_default()
    );
}

pub fn rpc(port: u16, method: &str, params: &str) -> Option<String> {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\",\"params\":{params}}}");
    let req = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    s.write_all(req.as_bytes()).ok()?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (_, json) = out.split_once("\r\n\r\n")?;
    if json.contains("\"error\"") {
        return None;
    }
    let i = json.find("\"result\":")? + "\"result\":".len();
    Some(json[i..].trim_end_matches(&['}', '\n', ' '][..]).to_string())
}

pub fn num(json: &str, key: &str) -> Option<u64> {
    let i = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &json[i..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

pub fn text(json: &str, key: &str) -> Option<String> {
    let i = json.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let rest = &json[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub fn height(port: u16) -> Option<u64> {
    num(&rpc(port, "chain_getInfo", "[]")?, "height")
}

pub fn tip_hash(port: u16) -> Option<String> {
    text(&rpc(port, "chain_getInfo", "[]")?, "tipHash")
}

pub fn wait_until<T: std::fmt::Debug>(
    what: &str,
    within: Duration,
    mut probe: impl FnMut() -> T,
    ok: impl Fn(&T) -> bool,
) -> T {
    let t0 = Instant::now();
    let mut last = probe();
    while t0.elapsed() < within {
        if ok(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(250));
        last = probe();
    }
    if ok(&last) {
        return last;
    }
    panic!("{what}: never satisfied within {within:?}; last observation {last:?}");
}

pub const TEST_ADDRESS: &str = "plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5";

fn mine_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("miner")
        .join("Cargo.toml")
}

pub fn miner_binary() -> Option<PathBuf> {
    let manifest = mine_manifest();
    if !manifest.exists() {
        return None;
    }
    let exe = manifest
        .parent()?
        .join("target")
        .join("release")
        .join(if cfg!(windows) { "plaine-miner.exe" } else { "plaine-miner" });
    if exe.exists() {
        return Some(exe);
    }
    let st = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "--bin", "plaine-miner", "--manifest-path"])
        .arg(&manifest)
        .status()
        .ok()?;
    st.success().then_some(exe).filter(|e| e.exists())
}

static MINER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn wait_until_healthy<T: std::fmt::Debug>(
    what: &str,
    ceiling: Duration,
    healthy: impl Fn() -> Result<(), String>,
    mut probe: impl FnMut() -> T,
    ok: impl Fn(&T) -> bool,
) -> T {
    let t0 = Instant::now();
    let mut last = probe();
    while t0.elapsed() < ceiling {
        if ok(&last) {
            return last;
        }
        if let Err(why) = healthy() {
            panic!(
                "{what}: gave up after {:.1}s because the system stopped being able to do it:                  {why}. Last observation {last:?}",
                t0.elapsed().as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
        last = probe();
    }
    if ok(&last) {
        return last;
    }
    panic!(
        "{what}: still healthy but never satisfied within {ceiling:?}; last observation {last:?}. Health held throughout, so this is a real stall, not a slow box."
    );
}

pub const STALL_CEILING: Duration = Duration::from_secs(600);

/// Threads for the test miner: all but two cores, so a run leaves the machine usable.
pub fn miner_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(2)
}

pub fn mine_to(node: &Node, target: u64, within: Duration) -> u64 {
    mine_to_address(node, TEST_ADDRESS, target, within)
}

pub fn mine_to_address(node: &Node, address: &str, target: u64, within: Duration) -> u64 {
    let _serial = MINER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let exe = miner_binary().expect(
        "plaine-miner could not be built. These tests need real proof of work: at POW_LIMIT a \
         block is ~65,536 Isochron hashes and nothing else in the workspace can produce one.",
    );
    let mut m = Command::new(exe)
        .args([
            "--address",
            &format!("{address}.it"),
            "--stratum",
            &format!("127.0.0.1:{}", node.stratum),
            "--threads",
            &miner_threads().to_string(),
            "--deadline",
            &within.as_secs().to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn plaine-miner");
    let t0 = Instant::now();
    let mut h = height(node.rpc).unwrap_or(0);
    while t0.elapsed() < within && h < target {
        std::thread::sleep(Duration::from_millis(500));
        h = height(node.rpc).unwrap_or(h);
    }
    let _ = m.kill();
    let _ = m.wait();
    h
}

pub struct RunningMiner {
    child: Child,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Drop for RunningMiner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn mine_background(node: &Node, deadline: Duration) -> RunningMiner {
    let serial = MINER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let exe = miner_binary().expect(
        "plaine-miner could not be built. These tests need real proof of work: at POW_LIMIT a \
         block is ~65,536 Isochron hashes and nothing else in the workspace can produce one.",
    );
    let child = Command::new(exe)
        .args([
            "--address",
            &format!("{TEST_ADDRESS}.it"),
            "--stratum",
            &format!("127.0.0.1:{}", node.stratum),
            "--threads",
            &miner_threads().to_string(),
            "--deadline",
            &deadline.as_secs().to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn plaine-miner");
    RunningMiner { child, _serial: serial }
}

pub fn walk_chain(port: u16) -> Vec<(u64, String)> {
    let tip = height(port).expect("the node must report a height");
    let mut out = Vec::with_capacity(tip as usize + 1);
    for h in 0..=tip {
        let r = rpc(port, "chain_getHeaderByHeight", &format!("[{h}]")).unwrap_or_else(|| {
            panic!("the node claims height {tip} but cannot serve header {h}")
        });
        let hash = text(&r, "hash")
            .unwrap_or_else(|| panic!("header {h} came back without a hash: {r}"));
        let reported = num(&r, "height")
            .unwrap_or_else(|| panic!("header {h} came back without a height: {r}"));
        assert_eq!(reported, h, "the node served height {reported} when asked for {h}");
        out.push((h, hash));
    }
    out
}

pub fn assert_chain_intact(port: u16, chain: &[(u64, String)]) {
    let mut seen = std::collections::HashSet::new();
    for (h, hash) in chain {
        assert!(
            seen.insert(hash.clone()),
            "hash {hash} appears at more than one height; height {h} is a DUPLICATE"
        );
    }
    for w in chain.windows(2) {
        let (parent_h, parent_hash) = &w[0];
        let (child_h, _) = &w[1];
        assert_eq!(*child_h, parent_h + 1, "heights {parent_h} and {child_h} are not contiguous");
        let r = rpc(port, "chain_getHeaderByHeight", &format!("[{child_h}]"))
            .unwrap_or_else(|| panic!("header {child_h} vanished between reads"));
        let prev = text(&r, "prevHash")
            .unwrap_or_else(|| panic!("header {child_h} has no prevHash: {r}"));
        assert_eq!(
            &prev, parent_hash,
            "height {child_h} does not point at height {parent_h}: the chain is spliced"
        );
    }
}

pub fn hard_kill(n: &mut Node) {
    n.child.kill().expect("kill");
    n.child.wait().expect("reap");
}

#[allow(dead_code)]
pub fn request_stop(n: &Node) -> std::io::Result<()> {
    let pid = n.child.id();
    #[cfg(unix)]
    {
        let st = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !st.success() {
            return Err(std::io::Error::other(format!("`kill -TERM {pid}` failed: {st}")));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        const CTRL_BREAK_EVENT: u32 = 1;
        #[link(name = "kernel32")]
        extern "system" {
            fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
        }
        let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[allow(dead_code)]
pub fn ensure_console() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetConsoleWindow() -> *mut core::ffi::c_void;
            fn AllocConsole() -> i32;
        }

        unsafe {
            if GetConsoleWindow().is_null() {
                AllocConsole();
            }
        }
    }
}

#[allow(dead_code)]
pub fn wait_exit(n: &mut Node, within: Duration) -> Option<i32> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if let Ok(Some(st)) = n.child.try_wait() {
            return st.code();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}
