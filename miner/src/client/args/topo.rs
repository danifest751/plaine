#[derive(Debug, Clone)]
pub struct Cpu {
    pub id: usize,
    pub group: u16,
    pub package: i32,
    pub core: i32,
    pub class: Option<u8>,
    pub l2_kib: Option<u32>,
    pub l2_shared: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Topology {
    pub model: String,
    pub cpus: Vec<Cpu>,
    pub source: &'static str,
    pub notes: Vec<String>,
}

impl Topology {
    pub fn detect() -> Topology {
        let mut t = read_platform();
        if t.cpus.is_empty() {
            let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            t.notes.push(format!(
                "no usable topology source; assuming {n} unrelated processors \
                 (std::thread::available_parallelism)"
            ));
            t.source = "available_parallelism";
            t.cpus = (0..n)
                .map(|id| Cpu {
                    id,
                    group: 0,
                    package: -1,
                    core: -1,
                    class: None,
                    l2_kib: None,
                    l2_shared: None,
                })
                .collect();
        }
        t.cpus.sort_by_key(|c| c.id);
        t
    }

    pub fn logical(&self) -> usize {
        self.cpus.len()
    }

    pub fn packages(&self) -> Option<usize> {
        if self.cpus.is_empty() || self.cpus.iter().any(|c| c.package < 0) {
            return None;
        }
        let mut v: Vec<i32> = self.cpus.iter().map(|c| c.package).collect();
        v.sort_unstable();
        v.dedup();
        Some(v.len())
    }

    pub fn cores(&self) -> Option<Vec<Vec<usize>>> {
        if self.cpus.is_empty() || self.cpus.iter().any(|c| c.core < 0 || c.package < 0) {
            return None;
        }
        let mut keys: Vec<(i32, i32)> = self.cpus.iter().map(|c| (c.package, c.core)).collect();
        keys.sort_unstable();
        keys.dedup();
        Some(
            keys.iter()
                .map(|k| {
                    self.cpus
                        .iter()
                        .filter(|c| (c.package, c.core) == *k)
                        .map(|c| c.id)
                        .collect()
                })
                .collect(),
        )
    }

    pub fn smt(&self) -> bool {
        matches!(self.cores(), Some(cs) if cs.iter().any(|c| c.len() > 1))
    }

    pub fn one_per_core(&self) -> Option<Vec<usize>> {
        Some(self.cores()?.iter().filter_map(|c| c.first().copied()).collect())
    }

    /// CPUs ordered so that taking the first N spreads them over as many distinct cores
    /// as possible: every core's first sibling, then every core's second, and so on.
    ///
    /// This is the default pin order. Pinning matters here because a worker's 64 KiB pad
    /// lives in its core's private L2, and a thread the scheduler moves leaves its pad
    /// behind - measured at 25.0 kH/s pinned against 23.3-23.7 unpinned on a Ryzen 7
    /// 8745HS, reproducibly. Taking CPUs in plain id order instead would double-book the
    /// first cores whenever fewer workers than CPUs are asked for.
    pub fn spread(&self) -> Vec<usize> {
        let Some(mut cores) = self.cores() else {
            return self.cpus.iter().map(|c| c.id).collect();
        };
        // Faster cores first: on a phone the big cores often carry the highest ids,
        // and a miner asked for fewer threads than CPUs should get them. The sort is
        // stable, so a machine without core classes keeps plain id order.
        let class_of = |id: usize| self.cpus.iter().find(|c| c.id == id).and_then(|c| c.class);
        cores.sort_by_key(|core| std::cmp::Reverse(core.first().and_then(|&id| class_of(id))));
        let deepest = cores.iter().map(|c| c.len()).max().unwrap_or(0);
        let mut out = Vec::with_capacity(self.cpus.len());
        for rank in 0..deepest {
            for core in &cores {
                if let Some(&id) = core.get(rank) {
                    out.push(id);
                }
            }
        }
        out
    }

    pub fn classes(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self.cpus.iter().filter_map(|c| c.class).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn hybrid(&self) -> bool {
        self.classes().len() > 1
    }

    pub fn performance_cpus(&self) -> Option<Vec<usize>> {
        let classes = self.classes();
        if classes.len() < 2 {
            return None;
        }
        let top = *classes.last()?;
        Some(self.cpus.iter().filter(|c| c.class == Some(top)).map(|c| c.id).collect())
    }

    pub fn sibling_strides(&self) -> Vec<usize> {
        let Some(cores) = self.cores() else { return Vec::new() };
        let mut v: Vec<usize> = cores
            .iter()
            .filter(|c| c.len() > 1)
            .flat_map(|c| c.windows(2).map(|w| w[1].saturating_sub(w[0])))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn cpu(&self, id: usize) -> Option<&Cpu> {
        self.cpus.iter().find(|c| c.id == id)
    }

    pub fn cores_covered(&self, list: &[usize]) -> Option<usize> {
        if self.cpus.iter().any(|c| c.core < 0 || c.package < 0) {
            return None;
        }
        let mut keys: Vec<(i32, i32)> =
            list.iter().filter_map(|id| self.cpu(*id)).map(|c| (c.package, c.core)).collect();
        keys.sort_unstable();
        keys.dedup();
        Some(keys.len())
    }

    pub fn class_label(&self, class: Option<u8>) -> String {
        let classes = self.classes();
        match class {
            None => "-".into(),
            Some(c) if classes.len() < 2 => format!("{c}"),
            Some(c) if Some(&c) == classes.last() => "P".into(),
            Some(c) if Some(&c) == classes.first() => "E".into(),
            Some(c) => format!("{c}"),
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("plaine-miner topology\n\n");
        if !self.model.is_empty() {
            s.push_str(&format!("  model    {}\n", self.model));
        }
        s.push_str(&format!("  source   {}\n", self.source));
        let sockets = match self.packages() {
            Some(n) => format!("{n} socket(s)"),
            None => "sockets unknown".to_string(),
        };
        let cores = match self.cores() {
            Some(c) => format!("{} physical cores", c.len()),
            None => "physical cores unknown".to_string(),
        };
        s.push_str(&format!(
            "  machine  {sockets}, {cores}, {} logical processors\n",
            self.logical()
        ));

        match self.cores() {
            Some(cores) => {
                s.push_str("\n  socket  core  class  CPUs                   L2\n");
                for group in &cores {
                    let Some(first) = group.first().and_then(|id| self.cpu(*id)) else { continue };
                    let list =
                        group.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",");
                    s.push_str(&format!(
                        "  {:<7} {:<5} {:<6} {:<22} {}\n",
                        first.package,
                        first.core,
                        self.class_label(first.class),
                        list,
                        match (first.l2_kib, first.l2_shared) {
                            (Some(k), Some(n)) if n > group.len() => {
                                format!("{k} KiB shared by {n} CPUs")
                            }
                            (Some(k), _) => format!("{k} KiB"),
                            (None, _) => "-".into(),
                        }
                    ));
                }
            }
            None => {
                let list =
                    self.cpus.iter().map(|c| c.id.to_string()).collect::<Vec<_>>().join(",");
                s.push_str(&format!("\n  CPUs     {list}\n"));
            }
        }

        s.push('\n');
        let strides = self.sibling_strides();
        if strides.is_empty() {
            s.push_str("  SMT      no sibling reported: one logical processor per core\n");
        } else {
            let show = strides.iter().map(|k| format!("n/n+{k}")).collect::<Vec<_>>().join(", ");
            s.push_str(&format!(
                "  SMT      siblings are numbered {show} on this machine\n\
                 \x20          it is n/n+1 on most desktop parts and n/n+28 on a dual\n\
                 \x20          Broadwell, so a pin list copied from another machine can\n\
                 \x20          silently double-book every core here\n"
            ));
        }
        if self.hybrid() {
            for c in self.classes().iter().rev() {
                let ids: Vec<String> = self
                    .cpus
                    .iter()
                    .filter(|x| x.class == Some(*c))
                    .map(|x| x.id.to_string())
                    .collect();
                s.push_str(&format!(
                    "  class {c}  {} - {} CPUs: {}\n",
                    self.class_label(Some(*c)),
                    ids.len(),
                    ids.join(",")
                ));
            }
            s.push_str(
                "           efficiency cores run about 28% fewer hashes per clock at the\n\
                 \x20          frozen 64 KiB pad, so a rate measured over a mix of both\n\
                 \x20          is a machine total and not a per-core figure\n",
            );
        }

        s.push_str("\n  ready-made pin lists\n");
        if self.smt() {
            if let Some(v) = self.one_per_core() {
                s.push_str(&format!(
                    "    one thread per core   --cpu-affinity {}\n",
                    fmt_list(&v)
                ));
            }
        }
        if let Some(p) = self.performance_cpus() {
            s.push_str(&format!("    performance cores     --cpu-affinity {}\n", fmt_list(&p)));
        }
        s.push_str(&format!(
            "    every processor       --threads {}   (the default)\n",
            self.logical()
        ));

        for n in &self.notes {
            s.push_str(&format!("\n  note: {n}\n"));
        }
        s
    }
}

pub fn fmt_list(v: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        out.push(if start == end { format!("{start}") } else { format!("{start}-{end}") });
        i += 1;
    }
    out.join(",")
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_platform() -> Topology {
    let mut t = Topology {
        model: read_model_linux(),
        cpus: Vec::new(),
        source: "/sys/devices/system/cpu",
        notes: Vec::new(),
    };
    let online = match std::fs::read_to_string("/sys/devices/system/cpu/online") {
        Ok(s) => parse_range_list(s.trim()),
        Err(e) => {
            t.notes.push(format!("/sys/devices/system/cpu/online: {e}"));
            Vec::new()
        }
    };
    if online.is_empty() {
        return t;
    }

    let mut p_cores = read_range_file("/sys/devices/cpu_core/cpus");
    let mut e_cores = read_range_file("/sys/devices/cpu_atom/cpus");
    if p_cores.is_empty() && e_cores.is_empty() {
        // ARM big.LITTLE (phones, most Android): no hybrid PMU files, but each CPU
        // states its relative capacity, 1024 for the fastest.
        let caps: Vec<(usize, u32)> = online
            .iter()
            .filter_map(|&id| {
                let c = read_i32(&format!("/sys/devices/system/cpu/cpu{id}/cpu_capacity"))?;
                Some((id, u32::try_from(c).ok()?))
            })
            .collect();
        if let Some((fast, slow)) = split_by_capacity(&caps) {
            p_cores = fast;
            e_cores = slow;
        }
    }

    for id in online {
        let base = format!("/sys/devices/system/cpu/cpu{id}");
        let package = read_i32(&format!("{base}/topology/physical_package_id")).unwrap_or(-1);
        let core = read_i32(&format!("{base}/topology/core_id")).unwrap_or(-1);
        let class = match (p_cores.contains(&id), e_cores.contains(&id)) {
            (true, false) => Some(1),
            (false, true) => Some(0),
            _ => None,
        };
        let (l2_kib, l2_shared) = read_l2_linux(&base);
        t.cpus.push(Cpu { id, group: 0, package, core, class, l2_kib, l2_shared });
    }
    if t.cpus.iter().all(|c| c.core < 0) {
        t.notes.push(
            "the kernel exposed no per-CPU topology - a container with a masked or read-only \
             /sys does this - so SMT siblings are unknown and no pin list can be trusted here"
                .into(),
        );
    }
    t
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_model_linux() -> String {
    let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") else { return String::new() };
    model_from_cpuinfo(&s)
}

/// The processor's name from /proc/cpuinfo. x86 kernels write `model name`; ARM
/// kernels (Android among them) write `Hardware`, or an old-style `Processor`.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn model_from_cpuinfo(s: &str) -> String {
    for key in ["model name", "Hardware", "Processor"] {
        for line in s.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            if k.trim() == key && !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Splits CPUs into the fastest class and the rest by their `cpu_capacity`. `None`
/// when every CPU reports the same capacity, or none does: nothing to split.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn split_by_capacity(caps: &[(usize, u32)]) -> Option<(Vec<usize>, Vec<usize>)> {
    let max = caps.iter().map(|c| c.1).max()?;
    let min = caps.iter().map(|c| c.1).min()?;
    if max == min {
        return None;
    }
    let fast = caps.iter().filter(|c| c.1 == max).map(|c| c.0).collect();
    let slow = caps.iter().filter(|c| c.1 < max).map(|c| c.0).collect();
    Some((fast, slow))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_l2_linux(base: &str) -> (Option<u32>, Option<usize>) {
    for i in 0..8 {
        let dir = format!("{base}/cache/index{i}");
        if read_i32(&format!("{dir}/level")) != Some(2) {
            continue;
        }
        let Ok(size) = std::fs::read_to_string(format!("{dir}/size")) else { continue };
        let size = size.trim();
        let Ok(n) = size.trim_end_matches(['K', 'M']).parse::<u32>() else { continue };
        let kib = if size.ends_with('M') { n * 1024 } else { n };
        let shared = read_range_file(&format!("{dir}/shared_cpu_list"));
        return (Some(kib), (!shared.is_empty()).then_some(shared.len()));
    }
    (None, None)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_i32(path: &str) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_range_file(path: &str) -> Vec<usize> {
    match std::fs::read_to_string(path) {
        Ok(s) => parse_range_list(s.trim()),
        Err(_) => Vec::new(),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn parse_range_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => match (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                (Ok(a), Ok(b)) if a <= b => out.extend(a..=b),
                _ => {}
            },
            None => {
                if let Ok(v) = part.trim().parse() {
                    out.push(v);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(target_os = "windows")]
fn read_platform() -> Topology {
    let mut t = Topology {
        model: read_model_windows(),
        cpus: Vec::new(),
        source: "GetLogicalProcessorInformationEx",
        notes: Vec::new(),
    };
    let buf = match super::cpu::logical_processor_information() {
        Ok(b) => b,
        Err(e) => {
            t.notes.push(format!("GetLogicalProcessorInformationEx failed: {e}"));
            return t;
        }
    };

    const REL_CORE: u32 = 0;
    const REL_CACHE: u32 = 2;
    const REL_PACKAGE: u32 = 3;

    let mut cores: Vec<(u64, u16, u8)> = Vec::new();
    let mut packages: Vec<Vec<(u64, u16)>> = Vec::new();
    let mut l2: Vec<(u64, u16, u32)> = Vec::new();

    let mut off = 0usize;
    while off + 8 <= buf.len() {
        let rel = u32_at(&buf, off);
        let size = u32_at(&buf, off + 4) as usize;
        if size < 8 || off + size > buf.len() {
            t.notes.push("the processor information buffer is malformed; stopped early".into());
            break;
        }
        match rel {
            REL_CORE => {
                let class = buf[off + 9];
                for (mask, group) in group_masks(&buf, off, off + 30, off + 32) {
                    cores.push((mask, group, class));
                }
            }
            REL_PACKAGE => packages.push(group_masks(&buf, off, off + 30, off + 32)),

            REL_CACHE if buf[off + 8] == 2 => {
                let bytes = u32_at(&buf, off + 12);
                for (mask, group) in group_masks(&buf, off, off + 38, off + 40) {
                    l2.push((mask, group, bytes / 1024));
                }
            }
            _ => {}
        }
        off += size;
    }

    for (core_index, (mask, group, class)) in cores.iter().enumerate() {
        let package = packages
            .iter()
            .position(|p| p.iter().any(|(m, g)| g == group && m & mask != 0))
            .map(|i| i as i32)
            .unwrap_or(-1);
        let found = l2.iter().find(|(m, g, _)| g == group && m & mask != 0);
        let l2_kib = found.map(|(_, _, k)| *k);
        let l2_shared = found.map(|(m, _, _)| m.count_ones() as usize);
        for bit in 0..64u32 {
            if mask >> bit & 1 == 0 {
                continue;
            }
            t.cpus.push(Cpu {
                id: *group as usize * 64 + bit as usize,
                group: *group,
                package,
                core: core_index as i32,
                class: Some(*class),
                l2_kib,
                l2_shared,
            });
        }
    }
    if packages.is_empty() {
        t.notes.push("Windows reported no package relations, so sockets are unknown".into());
    }
    if cores.iter().map(|c| c.2).collect::<std::collections::BTreeSet<_>>().len() < 2 {
        t.notes.push(
            "every core reports the same EfficiencyClass: either this part is uniform, or \
             this Windows predates hybrid-core reporting"
                .into(),
        );
    }
    t
}

#[cfg(target_os = "windows")]
fn group_masks(buf: &[u8], rec: usize, count_at: usize, first_at: usize) -> Vec<(u64, u16)> {
    let end = rec + u32_at(buf, rec + 4) as usize;
    let n = if count_at + 2 <= end { u16_at(buf, count_at).max(1) } else { 1 } as usize;
    let mut out = Vec::new();
    for i in 0..n {
        let at = first_at + i * 16;
        if at + 10 > end {
            break;
        }
        out.push((u64_at(buf, at), u16_at(buf, at + 8)));
    }
    out
}

#[cfg(target_os = "windows")]
fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
#[cfg(target_os = "windows")]
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
#[cfg(target_os = "windows")]
fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(v)
}

#[cfg(target_os = "windows")]
fn read_model_windows() -> String {
    super::cpu::brand_string()
        .unwrap_or_else(|| std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default())
}

#[cfg(target_os = "macos")]
fn read_platform() -> Topology {
    let logical = super::cpu::sysctl_i64("hw.logicalcpu").unwrap_or(0).max(0) as usize;
    let physical = super::cpu::sysctl_i64("hw.physicalcpu").unwrap_or(0).max(0) as usize;
    let packages = super::cpu::sysctl_i64("hw.packages").unwrap_or(1).max(1) as usize;
    let l2 = super::cpu::sysctl_i64("hw.l2cachesize").map(|b| (b / 1024) as u32);

    let p = super::cpu::sysctl_i64("hw.perflevel0.logicalcpu");
    let e = super::cpu::sysctl_i64("hw.perflevel1.logicalcpu");

    let mut t = Topology {
        model: super::cpu::sysctl_string("machdep.cpu.brand_string").unwrap_or_default(),
        cpus: Vec::new(),
        source: "sysctl",
        notes: Vec::new(),
    };
    let n = logical.max(1);
    let per_core = if physical > 0 { (n / physical).max(1) } else { 1 };
    for id in 0..n {
        t.cpus.push(Cpu {
            id,
            group: 0,
            package: if packages == 1 { 0 } else { -1 },
            core: (id / per_core) as i32,
            class: None,
            l2_kib: l2,
            l2_shared: None,
        });
    }
    t.notes.push(
        "macOS reports processor counts and no numbering: no API says which logical processor \
         is which, and there is no affinity API to use one with. The core column above is \
         arithmetic from hw.physicalcpu, not a reading - do not build a pin list from it."
            .into(),
    );
    if let (Some(p), Some(e)) = (p, e) {
        t.notes.push(format!(
            "Apple Silicon performance levels: {p} logical processors in the fast cluster and \
             {e} in the efficient one - countable, not selectable"
        ));
    }
    t
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos"
)))]
fn read_platform() -> Topology {
    Topology {
        model: String::new(),
        cpus: Vec::new(),
        source: "none",
        notes: vec![format!(
            "no topology reader for {}, so --cpu-affinity cannot be checked against a real \
             processor list here",
            std::env::consts::OS
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(id: usize, package: i32, core: i32, class: Option<u8>) -> Cpu {
        Cpu { id, group: 0, package, core, class, l2_kib: None, l2_shared: None }
    }

    fn topo(cpus: Vec<Cpu>) -> Topology {
        Topology { model: String::new(), cpus, source: "test", notes: Vec::new() }
    }

    #[test]
    fn a_phone_splits_into_its_big_and_little_cores_by_capacity() {
        // A Snapdragon 732G (the Poco X3): two Cortex-A76 at full capacity, six
        // Cortex-A55 at well under half of it.
        let caps: Vec<(usize, u32)> =
            (0..6).map(|id| (id, 423)).chain((6..8).map(|id| (id, 1024))).collect();
        let (fast, slow) = split_by_capacity(&caps).expect("two classes");
        assert_eq!(fast, [6, 7]);
        assert_eq!(slow, [0, 1, 2, 3, 4, 5]);
        assert_eq!(split_by_capacity(&[(0, 1024), (1, 1024)]), None, "one class, nothing to split");
        assert_eq!(split_by_capacity(&[]), None);
    }

    #[test]
    fn spread_puts_the_big_cores_first() {
        // Little cluster: package 0, cores 0-5; big cluster: package 1, cores 0-1.
        let mut cpus: Vec<Cpu> = (0..6).map(|id| cpu(id, 0, id as i32, Some(0))).collect();
        cpus.extend((6..8).map(|id| cpu(id, 1, id as i32 - 6, Some(1))));
        assert_eq!(topo(cpus).spread(), [6, 7, 0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn the_model_is_read_from_x86_and_arm_cpuinfo_alike() {
        let x86 = "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 7 8745HS\n";
        assert_eq!(model_from_cpuinfo(x86), "AMD Ryzen 7 8745HS");
        let android = "processor\t: 0\nBogoMIPS\t: 38.40\nFeatures\t: fp asimd\n\
                       CPU implementer\t: 0x51\n\nHardware\t: Qualcomm Technologies, Inc SM7150\n";
        assert_eq!(model_from_cpuinfo(android), "Qualcomm Technologies, Inc SM7150");
        assert_eq!(model_from_cpuinfo("Processor\t: AArch64 Processor rev 13 (aarch64)\n"),
            "AArch64 Processor rev 13 (aarch64)");
        assert_eq!(model_from_cpuinfo(""), "");
    }

    #[test]
    fn spread_visits_every_core_before_any_sibling() {
        // 8 cores, siblings numbered n/n+1 - the layout of the machine the pinning
        // default was measured on.
        let t = topo((0..16).map(|i| cpu(i, 0, (i / 2) as i32, None)).collect());
        assert_eq!(t.spread(), vec![0, 2, 4, 6, 8, 10, 12, 14, 1, 3, 5, 7, 9, 11, 13, 15]);
    }

    #[test]
    fn spread_reads_a_wide_sibling_stride() {
        // Dual Broadwell numbering, n/n+28: plain id order would pin the first 28
        // workers onto 14 cores twice over.
        let cpus: Vec<Cpu> =
            (0..56).map(|i| cpu(i, (i % 28 / 14) as i32, (i % 28) as i32, None)).collect();
        let s = topo(cpus).spread();
        assert_eq!(&s[..28], &(0..28).collect::<Vec<_>>()[..], "first pass: one CPU per core");
        assert_eq!(&s[28..], &(28..56).collect::<Vec<_>>()[..], "then the siblings");
    }

    #[test]
    fn fewer_workers_than_cpus_land_on_distinct_cores() {
        let t = topo((0..16).map(|i| cpu(i, 0, (i / 2) as i32, None)).collect());
        let first8: Vec<usize> = t.spread().into_iter().take(8).collect();
        assert_eq!(t.cores_covered(&first8), Some(8), "no core may be double-booked");
    }

    #[test]
    fn spread_without_core_identity_is_plain_id_order() {
        let t = topo((0..6).map(|i| cpu(i, -1, -1, None)).collect());
        assert_eq!(t.spread(), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn spread_is_a_permutation() {
        let t = topo((0..12).map(|i| cpu(i, 0, (i % 6) as i32, None)).collect());
        let mut s = t.spread();
        s.sort_unstable();
        assert_eq!(s, (0..12).collect::<Vec<_>>(), "every CPU exactly once");
    }

    #[test]
    fn sibling_stride_is_read_not_assumed() {
        let cpus: Vec<Cpu> =
            (0..56).map(|i| cpu(i, (i % 28 / 14) as i32, (i % 28) as i32, None)).collect();
        let t = topo(cpus);
        assert_eq!(t.sibling_strides(), vec![28]);
        assert_eq!(t.one_per_core().unwrap().len(), 28);

        let cpus: Vec<Cpu> = (0..16).map(|i| cpu(i, 0, (i / 2) as i32, None)).collect();
        assert_eq!(topo(cpus).sibling_strides(), vec![1]);
    }

    #[test]
    fn unknown_core_identity_is_none() {
        let t = topo((0..8).map(|i| cpu(i, -1, -1, None)).collect());
        assert!(t.cores().is_none());
        assert!(t.one_per_core().is_none());
        assert!(!t.smt());
        assert!(t.sibling_strides().is_empty());
        assert!(t.cores_covered(&[0, 1]).is_none());
    }

    #[test]
    fn hybrid_parts_split_by_class() {
        let mut cpus: Vec<Cpu> = (0..4).map(|i| cpu(i, 0, (i / 2) as i32, Some(1))).collect();
        cpus.extend((4..12).map(|i| cpu(i, 0, i as i32 - 2, Some(0))));
        let t = topo(cpus);
        assert!(t.hybrid());
        assert_eq!(t.performance_cpus().unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(t.cores().unwrap().len(), 10);
        assert_eq!(t.sibling_strides(), vec![1]);

        assert_eq!(t.cores_covered(&[0, 1, 2, 3]), Some(2));
        assert_eq!(t.cores_covered(&[0, 2, 4, 5]), Some(4));

        let u = topo((0..4).map(|i| cpu(i, 0, i as i32, Some(0))).collect());
        assert!(!u.hybrid());
        assert!(u.performance_cpus().is_none());
    }

    #[test]
    fn printed_pin_list_round_trips() {
        assert_eq!(fmt_list(&[0, 1, 2, 3, 8, 9, 10, 11]), "0-3,8-11");
        assert_eq!(fmt_list(&[0, 2, 4, 6]), "0,2,4,6");
        assert_eq!(fmt_list(&[5]), "5");
        assert_eq!(fmt_list(&[]), "");
    }

    #[test]
    fn real_machine_reads_consistently() {
        let t = Topology::detect();
        assert!(t.logical() >= 1);
        if let Some(cores) = t.cores() {
            assert_eq!(cores.iter().map(|c| c.len()).sum::<usize>(), t.logical());
        }

        assert!(t.render().contains("plaine-miner topology"));
    }
}
