use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PLAINE_GIT_SHA");
    println!("cargo:rerun-if-env-changed=PLAINE_REQUIRE_BUILD_ID");

    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace = workspace_root(&manifest);
    let (sha, state, origin) = identify(&manifest, &workspace);

    let line = format!("plaine-build: {} {} ({})", sha, state, origin);

    if origin == "none" {
        println!(
            "cargo:warning=build identity UNKNOWN for {}: no .git above {}, and \
             .git-archival.txt is absent or still carries unsubstituted $Format \
             placeholders. The binary will say UNKNOWN in --version. Set \
             PLAINE_GIT_SHA, or build from a checkout or a `git archive` tarball.",
            std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
            manifest.display()
        );
        if std::env::var("PLAINE_REQUIRE_BUILD_ID").as_deref() == Ok("1") {
            // a signed release binary with no traceable commit is worse than a failed build.
            panic!("PLAINE_REQUIRE_BUILD_ID=1 but this build cannot name its commit");
        }
    }

    println!("cargo:rustc-env=PLAINE_BUILD_SHA={}", sha);
    println!("cargo:rustc-env=PLAINE_BUILD_STATE={}", state);
    println!("cargo:rustc-env=PLAINE_BUILD_ORIGIN={}", origin);
    println!("cargo:rustc-env=PLAINE_BUILD_LINE={}", line);
}

fn workspace_root(from: &Path) -> PathBuf {
    let mut best = from.to_path_buf();
    let mut d = Some(from);
    while let Some(p) = d {
        let m = p.join("Cargo.toml");
        if let Ok(t) = std::fs::read_to_string(&m) {
            if t.lines().any(|l| l.trim_start().starts_with("[workspace]")) {
                best = p.to_path_buf();
            }
        }
        d = p.parent();
    }
    best
}

fn identify(manifest: &Path, workspace: &Path) -> (String, String, String) {
    if let Some(v) = std::env::var_os("PLAINE_GIT_SHA") {
        let v = v.to_string_lossy().trim().to_ascii_lowercase();
        if is_sha(&v) {
            return (v, "unknown-state".into(), "env".into());
        }
        // don't fall back to UNKNOWN here: a build system that sets this wrong should hear about it.
        panic!("PLAINE_GIT_SHA is {v:?}, not 40 hex characters");
    }

    if let Some(root) = find_up(manifest, ".git") {
        if let Some((sha, state)) = from_git(&root, workspace) {
            return (sha, state, "git".into());
        }
    }

    if let Some(root) = find_up(manifest, ".git-archival.txt") {
        let f = root.join(".git-archival.txt");

        println!("cargo:rerun-if-changed={}", f.display());
        if let Some(sha) = from_archival(&f) {
            return (sha, "archived".into(), "archive".into());
        }
    }

    ("UNKNOWN".into(), "UNKNOWN".into(), "none".into())
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn find_up(start: &Path, name: &str) -> Option<PathBuf> {
    let mut d = Some(start);
    while let Some(p) = d {
        if p.join(name).exists() {
            return Some(p.to_path_buf());
        }
        d = p.parent();
    }
    None
}

fn from_git(root: &Path, workspace: &Path) -> Option<(String, String)> {
    let sha = git(root, &["rev-parse", "HEAD"])?
        .trim()
        .to_ascii_lowercase();
    if !is_sha(&sha) {
        return None;
    }

    for p in ["HEAD", "index", "packed-refs"] {
        if let Some(f) = git(root, &["rev-parse", "--git-path", p]) {
            declare(root, f.trim());
        }
    }
    if let Some(r) = git(root, &["symbolic-ref", "--quiet", "HEAD"]) {
        if let Some(f) = git(root, &["rev-parse", "--git-path", r.trim()]) {
            declare(root, f.trim());
        }
    }

    if let Some(list) = git(
        root,
        &["ls-files", "-z", "--", &workspace.to_string_lossy()],
    ) {
        for f in list.split('\0').filter(|s| !s.is_empty()) {
            declare(root, f);
        }
    }

    let ws = workspace.to_string_lossy().into_owned();
    let status =
        git(root, &["status", "--porcelain", "--no-renames", "--", &ws]).unwrap_or_default();
    let mut dirty = false;
    let mut untracked = false;
    for l in status.lines() {
        if l.is_empty() {
            continue;
        }
        if l.starts_with("??") {
            untracked = true;
        } else {
            dirty = true;
        }
    }
    let state = match (dirty, untracked) {
        (false, false) => "clean",
        (true, false) => "dirty",
        (false, true) => "untracked",
        (true, true) => "dirty+untracked",
    };
    Some((sha, state.into()))
}

fn declare(root: &Path, p: &str) {
    let p = Path::new(p);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    if abs.exists() {
        println!("cargo:rerun-if-changed={}", abs.display());
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8(o.stdout).ok()
}

fn from_archival(p: &Path) -> Option<String> {
    let text = std::fs::read_to_string(p).ok()?;
    for l in text.lines() {
        let l = l.trim();
        if let Some(v) = l.strip_prefix("commit:") {
            let v = v.trim().to_ascii_lowercase();
            if is_sha(&v) {
                return Some(v);
            }
            return None;
        }
    }
    None
}
