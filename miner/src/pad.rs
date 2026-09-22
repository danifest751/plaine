use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use plaine_pow::{Isochron, Scratch, SCRATCH_BYTES, SCRATCH_WORDS};

const PAD_ALIGN: usize = 65_536;

const PAD_BYTES: usize = SCRATCH_BYTES as usize;

// one pad == one 64 KiB alignment unit, so no two pads ever share a page.
const _: () = assert!(PAD_BYTES == PAD_ALIGN);

#[cfg(target_os = "linux")]
const ASSUMED_HUGE_BYTES: usize = 2 * 1024 * 1024;

#[cfg(any(windows, target_os = "linux"))]
const MAX_ROUNDING_WASTE: usize = 8 * 1024 * 1024;

// Touch every page up front: the first mine shouldn't eat page faults, and a huge-page mapping
// has to be backed before smaps will admit it exists.
fn prefault(p: *mut u8, len: usize) {
    // SAFETY: p/len are a live mapping the caller owns.
    unsafe {
        core::ptr::write_bytes(p, 0, len);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PadPages {
    Base,
    HugeTlb,
    Transparent,
    Large,
}

impl PadPages {
    pub fn tag(self) -> &'static str {
        match self {
            PadPages::Base => "4K",
            PadPages::HugeTlb => "2M/hugetlb",
            PadPages::Transparent => "2M/thp",
            PadPages::Large => "2M/large",
        }
    }

    pub fn is_huge(self) -> bool {
        !matches!(self, PadPages::Base)
    }

    fn code(self) -> u8 {
        match self {
            PadPages::Base => 0,
            PadPages::HugeTlb => 1,
            PadPages::Transparent => 2,
            PadPages::Large => 3,
        }
    }
}

const KINDS: [PadPages; 4] =
    [PadPages::Base, PadPages::HugeTlb, PadPages::Transparent, PadPages::Large];

static NOW: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

static EVER: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
static LOGGED: AtomicBool = AtomicBool::new(false);
static NOTE: OnceLock<String> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Summary {
    per_kind: [usize; 4],
}

impl Summary {
    pub fn of(kinds: impl IntoIterator<Item = PadPages>) -> Summary {
        let mut s = Summary::default();
        for k in kinds {
            s.per_kind[k.code() as usize] += 1;
        }
        s
    }

    pub fn total(self) -> usize {
        self.per_kind.iter().sum()
    }

    pub fn huge(self) -> usize {
        self.per_kind[1..].iter().sum()
    }

    pub fn count(self, kind: PadPages) -> usize {
        self.per_kind[kind.code() as usize]
    }

    pub fn mixed(self) -> bool {
        self.per_kind[1..].iter().filter(|n| **n > 0).count() > 1
    }

    pub fn kind(self) -> PadPages {
        let mut best = PadPages::Base;
        let mut most = 0;
        for k in KINDS.into_iter().skip(1) {
            if self.count(k) > most {
                most = self.count(k);
                best = k;
            }
        }
        best
    }

    pub fn tag(self) -> &'static str {
        if self.mixed() {
            "2M/mixed"
        } else {
            self.kind().tag()
        }
    }
}

fn record(pages: PadPages, note: String) {
    let i = pages.code() as usize;
    NOW[i].fetch_add(1, Ordering::Relaxed);
    EVER[i].fetch_add(1, Ordering::Relaxed);
    let _ = NOTE.set(note);
}

fn unrecord(pages: PadPages) {
    let prev = NOW[pages.code() as usize].fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "a {pages:?} pad region was released twice");
}

fn tally(counts: &[AtomicUsize; 4]) -> Summary {
    let mut s = Summary::default();
    for (slot, c) in s.per_kind.iter_mut().zip(counts.iter()) {
        *slot = c.load(Ordering::Relaxed);
    }
    s
}

pub fn observed_now() -> Summary {
    tally(&NOW)
}

pub fn observed_ever() -> Summary {
    tally(&EVER)
}

pub fn status_field() -> String {
    field(observed_now())
}

fn field(s: Summary) -> String {
    if s.total() == 0 {
        return "pads -".to_string();
    }
    format!("pads {} {}/{}", s.tag(), s.huge(), s.total())
}

/// Why the last mapping got the page size it got, in the OS's own words.
///
/// Callers that report page size should prefer this over a canned explanation. The
/// stock advice names SeLockMemoryPrivilege, which is simply wrong whenever the right
/// is already held and the mapping failed for some other reason - on Windows,
/// ERROR_NO_SYSTEM_RESOURCES from physical fragmentation is the common one, and it
/// sends the reader to secpol.msc for nothing.
pub fn last_note() -> Option<&'static str> {
    NOTE.get().map(|s| s.as_str()).filter(|s| !s.is_empty())
}

pub fn log_startup(verbose: bool) -> bool {
    if LOGGED.swap(true, Ordering::Relaxed) {
        return false;
    }
    let s = observed_ever();
    let (huge, total) = (s.huge(), s.total());
    if total == 0 {
        return false;
    }
    if huge == total {
        eprintln!(
            "plaine-miner: scratchpads on {} pages, {huge}/{total} regions",
            s.tag()
        );
    } else if huge > 0 {
        eprintln!(
            "plaine-miner: scratchpads on {} pages for {huge} of {total} regions, the rest \
             on standard pages",
            s.tag()
        );
    } else {
        eprintln!("plaine-miner: scratchpads on standard pages (large pages unavailable)");
    }
    // Why large pages were skipped can be locale-dependent OS text, so it stays
    // out of the default log and shows only under --verbose.
    if verbose {
        if let Some(note) = NOTE.get() {
            if !note.is_empty() {
                eprintln!("plaine-miner: large-page note: {note}");
            }
        }
    }
    true
}

pub struct Pads {
    base: *mut u8,
    mapped: usize,
    count: usize,
    pages: PadPages,
    stage: Scratch,
}

// SAFETY: Pads owns its mapping outright; the base pointer is never aliased across threads.
unsafe impl Send for Pads {}

impl Pads {
    pub fn new(count: usize, huge: bool) -> io::Result<Pads> {
        assert!(count > 0, "a worker needs at least one pad");
        let want = count
            .checked_mul(PAD_BYTES)
            .expect("pad count times 64 KiB overflows a usize");

        let (base, mapped, pages, note) = map_region(want, huge)?;
        record(pages, note);

        prefault(base, mapped);

        Ok(Pads { base, mapped, count, pages, stage: Scratch::new() })
    }

    pub fn many(workers: usize, count: usize, huge: bool) -> (Vec<Pads>, Option<io::Error>) {
        let mut out = Vec::with_capacity(workers);
        for _ in 0..workers {
            match Pads::new(count, huge) {
                Ok(p) => out.push(p),
                Err(e) => return (out, Some(e)),
            }
        }
        (out, None)
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        // new() rejects count 0, so a Pads always holds at least one. here to satisfy clippy.
        false
    }

    pub fn pages(&self) -> PadPages {
        self.pages
    }

    pub(crate) fn slot_ptr(&self, j: usize) -> *mut u64 {
        assert!(j < self.count, "pad {j} out of range for a {}-pad region", self.count);

        // SAFETY: j < count asserted, stride PAD_BYTES; offset lands inside the mapping.
        unsafe { self.base.add(j * PAD_BYTES).cast::<u64>() }
    }

    pub(crate) fn fill(&mut self, iso: Isochron, j: usize, seed: u64) -> u64 {
        let prog_seed = iso.fill(&mut self.stage, seed);
        let src = self.stage.words().as_ptr();
        let dst = self.slot_ptr(j);

        // stage and pad j never overlap - the pads are disjoint by construction.
        // SAFETY: src is the stage's SCRATCH_WORDS window, dst pad j's live one.
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, SCRATCH_WORDS);
        }
        prog_seed
    }

    pub fn words(&self, j: usize) -> &[u64; SCRATCH_WORDS] {
        let p = self.slot_ptr(j);

        // SAFETY: slot_ptr already bounds-checked j; that window is a full pad.
        unsafe { &*p.cast::<[u64; SCRATCH_WORDS]>() }
    }

    pub fn checksum(&self, j: usize) -> u64 {
        let mut h = 0u64;
        for &w in self.words(j).iter() {
            h = (h ^ w).wrapping_mul(plaine_pow::MULT);
        }
        h
    }
}

impl Drop for Pads {
    fn drop(&mut self) {
        release(self.base, self.mapped);

        unrecord(self.pages);
    }
}

impl core::fmt::Debug for Pads {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pads")
            .field("count", &self.count)
            .field("bytes", &self.mapped)
            .field("pages", &self.pages)
            .finish()
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    pub const MEM_COMMIT: u32 = 0x0000_1000;
    pub const MEM_RESERVE: u32 = 0x0000_2000;
    pub const MEM_RELEASE: u32 = 0x0000_8000;
    pub const MEM_LARGE_PAGES: u32 = 0x2000_0000;
    pub const PAGE_READWRITE: u32 = 0x04;

    pub const TOKEN_QUERY: u32 = 0x0008;
    pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
    pub const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

    pub const SE_LOCK_MEMORY_NAME: &[u8; 22] = b"SeLockMemoryPrivilege\0";

    #[repr(C)]
    #[derive(Default)]
    pub struct Luid {
        pub low: u32,
        pub high: i32,
    }

    #[repr(C)]
    pub struct LuidAndAttributes {
        pub luid: Luid,
        pub attributes: u32,
    }

    #[repr(C)]
    pub struct TokenPrivileges {
        pub count: u32,
        pub privileges: [LuidAndAttributes; 1],
    }

    extern "system" {
        pub fn VirtualAlloc(
            addr: *mut c_void,
            size: usize,
            alloc_type: u32,
            protect: u32,
        ) -> *mut c_void;
        pub fn VirtualFree(addr: *mut c_void, size: usize, free_type: u32) -> i32;
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn GetLargePageMinimum() -> usize;
        pub fn CloseHandle(h: *mut c_void) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        pub fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        pub fn LookupPrivilegeValueA(system: *const u8, name: *const u8, luid: *mut Luid) -> i32;
        pub fn AdjustTokenPrivileges(
            token: *mut c_void,
            disable_all: i32,
            new_state: *const TokenPrivileges,
            buffer_len: u32,
            previous: *mut c_void,
            return_len: *mut u32,
        ) -> i32;
    }
}

#[cfg(windows)]
fn enable_lock_memory() -> Result<(), String> {
    let mut token: *mut std::ffi::c_void = core::ptr::null_mut();

    // SAFETY: the process pseudo-handle needs no close; token is a live out pointer.
    let ok = unsafe {
        sys::OpenProcessToken(
            sys::GetCurrentProcess(),
            sys::TOKEN_ADJUST_PRIVILEGES | sys::TOKEN_QUERY,
            &mut token,
        )
    };
    if ok == 0 {
        return Err(format!("OpenProcessToken failed: {}", io::Error::last_os_error()));
    }
    let mut luid = sys::Luid::default();

    // SAFETY: a null system name means the local system; the name is a NUL-terminated literal.
    let ok = unsafe {
        sys::LookupPrivilegeValueA(
            core::ptr::null(),
            sys::SE_LOCK_MEMORY_NAME.as_ptr(),
            &mut luid,
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();

        // SAFETY: token is the handle OpenProcessToken returned; closed once on this path.
        unsafe { sys::CloseHandle(token) };
        return Err(format!("LookupPrivilegeValue(SeLockMemoryPrivilege) failed: {e}"));
    }
    let tp = sys::TokenPrivileges {
        count: 1,
        privileges: [sys::LuidAndAttributes { luid, attributes: sys::SE_PRIVILEGE_ENABLED }],
    };

    // SAFETY: token is live and tp is a live local of exactly the length count claims.
    let ret = unsafe {
        sys::AdjustTokenPrivileges(
            token,
            0,
            &tp,
            core::mem::size_of::<sys::TokenPrivileges>() as u32,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };

    let e = io::Error::last_os_error();

    // SAFETY: same handle, closed exactly once on this path.
    unsafe { sys::CloseHandle(token) };
    match (ret, e.raw_os_error()) {
        (r, Some(0)) if r != 0 => Ok(()),
        _ => Err(format!(
            "SeLockMemoryPrivilege not held ({e}); grant `Lock pages in memory` in \
             secpol.msc and log on again"
        )),
    }
}

#[cfg(windows)]
fn map_region(want: usize, huge: bool) -> io::Result<(*mut u8, usize, PadPages, String)> {
    if huge {
        match try_large_pages(want) {
            Ok((p, len)) => {
                return Ok((
                    p,
                    len,
                    PadPages::Large,
                    format!("{} MiB of MEM_LARGE_PAGES", len / (1024 * 1024)),
                ))
            }
            Err(note) => {
                let (p, len) = map_base(want)?;
                return Ok((p, len, PadPages::Base, note));
            }
        }
    }
    let (p, len) = map_base(want)?;
    Ok((p, len, PadPages::Base, "huge pages not requested".to_string()))
}

#[cfg(windows)]
fn try_large_pages(want: usize) -> Result<(*mut u8, usize), String> {
    enable_lock_memory()?;

    // SAFETY: takes and touches nothing; returns 0 where large pages are unsupported.
    let lp = unsafe { sys::GetLargePageMinimum() };
    if lp == 0 {
        return Err("this processor has no large-page support".to_string());
    }
    let len = want.next_multiple_of(lp);
    if len > want + MAX_ROUNDING_WASTE {
        return Err(format!(
            "large pages are {} MiB here and the pads need {} KiB; the rounding waste is \
             not worth it",
            lp / (1024 * 1024),
            want / 1024
        ));
    }

    // SAFETY: null base kernel-chosen; len a nonzero multiple of the large-page size.
    let p = unsafe {
        sys::VirtualAlloc(
            core::ptr::null_mut(),
            len,
            sys::MEM_COMMIT | sys::MEM_RESERVE | sys::MEM_LARGE_PAGES,
            sys::PAGE_READWRITE,
        )
    };
    if p.is_null() {
        return Err(format!(
            "VirtualAlloc(MEM_LARGE_PAGES) failed: {}",
            io::Error::last_os_error()
        ));
    }
    Ok((p.cast::<u8>(), len))
}

#[cfg(windows)]
fn map_base(want: usize) -> io::Result<(*mut u8, usize)> {
    let len = want.next_multiple_of(PAD_ALIGN);

    // SAFETY: as above, ordinary pages; len rounded up to PAD_ALIGN.
    let p = unsafe {
        sys::VirtualAlloc(
            core::ptr::null_mut(),
            len,
            sys::MEM_COMMIT | sys::MEM_RESERVE,
            sys::PAGE_READWRITE,
        )
    };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok((p.cast::<u8>(), len))
}

#[cfg(windows)]
fn release(addr: *mut u8, _len: usize) {
    // SAFETY: addr is the base VirtualAlloc returned; MEM_RELEASE frees it once and needs size 0.
    unsafe {
        sys::VirtualFree(addr.cast(), 0, sys::MEM_RELEASE);
    }
}

#[cfg(unix)]
mod sys {
    use std::ffi::c_void;

    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const MAP_PRIVATE: i32 = 0x02;

    #[cfg(not(target_vendor = "apple"))]
    pub const MAP_ANONYMOUS: i32 = 0x20;
    #[cfg(target_vendor = "apple")]
    pub const MAP_ANONYMOUS: i32 = 0x1000;

    #[cfg(target_os = "linux")]
    pub const MAP_HUGETLB: i32 = 0x0004_0000;

    #[cfg(target_os = "linux")]
    pub const MADV_HUGEPAGE: i32 = 14;

    extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> i32;
        #[cfg(target_os = "linux")]
        pub fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    }
}

#[cfg(unix)]
fn mmap_rw(len: usize, extra: i32) -> Option<*mut u8> {
    // SAFETY: null hint, kernel picks; len nonzero, fd -1 (anon). `extra` carries MAP_HUGETLB or 0.
    let p = unsafe {
        sys::mmap(
            core::ptr::null_mut(),
            len,
            sys::PROT_READ | sys::PROT_WRITE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | extra,
            -1,
            0,
        )
    };
    if p as isize == -1 {
        None
    } else {
        Some(p.cast::<u8>())
    }
}

#[cfg(unix)]
fn mmap_aligned(len: usize, align: usize) -> io::Result<*mut u8> {
    let over = len.checked_add(align).expect("pad region size overflows a usize");
    let raw = mmap_rw(over, 0).ok_or_else(io::Error::last_os_error)?;
    let base = (raw as usize).next_multiple_of(align);
    let head = base - raw as usize;
    if head > 0 {
        // trim the over-allocation down to the aligned window: front slice first,
        // SAFETY: [raw, raw+head) is that front slice, unmapped once.
        unsafe { sys::munmap(raw.cast(), head) };
    }
    let tail = over - head - len;
    if tail > 0 {
        // SAFETY: then the back slice [base+len, base+len+tail) of the same mapping, also once.
        unsafe { sys::munmap((base + len) as *mut _, tail) };
    }
    Ok(base as *mut u8)
}

#[cfg(target_os = "linux")]
fn meminfo_num(key: &str) -> Option<usize> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest.trim().trim_end_matches("kB").trim().parse().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn meminfo_kib(key: &str) -> Option<usize> {
    meminfo_num(key).map(|n| n * 1024)
}

#[cfg(target_os = "linux")]
fn anon_huge_bytes(base: usize) -> Option<usize> {
    let text = std::fs::read_to_string("/proc/self/smaps").ok()?;
    let mut inside = false;
    for line in text.lines() {
        if let Some(lo) = line
            .split('-')
            .next()
            .and_then(|t| usize::from_str_radix(t, 16).ok())
        {
            inside = lo == base;
            continue;
        }
        if inside {
            if let Some(rest) = line.strip_prefix("AnonHugePages:") {
                let n: usize = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
                return Some(n * 1024);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn huge_page_bytes() -> usize {
    meminfo_kib("Hugepagesize:").filter(|n| *n > 0).unwrap_or(ASSUMED_HUGE_BYTES)
}

#[cfg(target_os = "linux")]
fn map_region(want: usize, huge: bool) -> io::Result<(*mut u8, usize, PadPages, String)> {
    if !huge {
        let (p, len) = map_base(want)?;
        return Ok((p, len, PadPages::Base, "huge pages not requested".to_string()));
    }

    let hp = huge_page_bytes();
    let rounded = want.next_multiple_of(hp);

    let why: String;

    if rounded > want + MAX_ROUNDING_WASTE {
        why = format!(
            "huge pages are {} MiB on this kernel and the pads need {} KiB; the rounding \
             waste is not worth it",
            hp / (1024 * 1024),
            want / 1024
        );
    } else {
        let free_before = meminfo_num("HugePages_Free:");
        if let Some(p) = mmap_rw(rounded, sys::MAP_HUGETLB) {
            prefault(p, rounded);
            let free_after = meminfo_num("HugePages_Free:");
            let note = match (free_before, free_after) {
                (Some(a), Some(b)) => format!(
                    "{} x {} MiB from the hugetlb pool (HugePages_Free {a} -> {b})",
                    rounded / hp,
                    hp / (1024 * 1024)
                ),
                _ => format!(
                    "{} x {} MiB from the hugetlb pool",
                    rounded / hp,
                    hp / (1024 * 1024)
                ),
            };
            return Ok((p, rounded, PadPages::HugeTlb, note));
        }
        let hugetlb_err = io::Error::last_os_error();
        let total = meminfo_num("HugePages_Total:").unwrap_or(0);
        let free = free_before.unwrap_or(0);

        match mmap_aligned(rounded, hp) {
            Ok(p) => {
                // SAFETY: p/rounded are the mapping we just made; advice only, cannot fail badly.
                let adv = unsafe { sys::madvise(p.cast(), rounded, sys::MADV_HUGEPAGE) };
                if adv != 0 {
                    why = format!(
                        "no hugetlb pool (HugePages_Total={total} Free={free}, mmap said \
                         {hugetlb_err}) and madvise(MADV_HUGEPAGE) said {}; check \
                         /sys/kernel/mm/transparent_hugepage/enabled",
                        io::Error::last_os_error()
                    );
                    release(p, rounded);
                } else {
                    prefault(p, rounded);
                    match anon_huge_bytes(p as usize) {
                        Some(n) if n >= rounded => {
                            return Ok((
                                p,
                                rounded,
                                PadPages::Transparent,
                                format!(
                                    "madvise(MADV_HUGEPAGE), {} KiB of {} KiB confirmed as \
                                     AnonHugePages in smaps (no hugetlb pool: \
                                     HugePages_Total={total} Free={free})",
                                    n / 1024,
                                    rounded / 1024
                                ),
                            ));
                        }
                        Some(n) => {
                            why = format!(
                                "no hugetlb pool (HugePages_Total={total} Free={free}) and \
                                 madvise(MADV_HUGEPAGE) was accepted but smaps reports only \
                                 {} KiB of {} KiB behind huge pages, so it is not claimed; \
                                 check /sys/kernel/mm/transparent_hugepage/enabled or \
                                 `sysctl -w vm.nr_hugepages={}`",
                                n / 1024,
                                rounded / 1024,
                                rounded / hp
                            );
                            release(p, rounded);
                        }
                        None => {
                            why = "madvise(MADV_HUGEPAGE) was accepted but /proc/self/smaps \
                                   could not be read back, so it is not claimed"
                                .to_string();
                            release(p, rounded);
                        }
                    }
                }
            }
            Err(e2) => {
                why = format!(
                    "no hugetlb pool (HugePages_Total={total} Free={free}, mmap said \
                     {hugetlb_err}) and no {hp}-byte-aligned region could be mapped for \
                     THP either: {e2}"
                );
            }
        }
    }

    let (p, len) = map_base(want)?;
    Ok((p, len, PadPages::Base, why))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn map_region(want: usize, huge: bool) -> io::Result<(*mut u8, usize, PadPages, String)> {
    let (p, len) = map_base(want)?;
    let note = if !huge {
        "huge pages not requested".to_string()
    } else if cfg!(target_vendor = "apple") {
        "macOS has no user-facing large-page API for ordinary allocations \
         (VM_FLAGS_SUPERPAGE_SIZE_2MB is x86-only and unimplemented on Apple Silicon), so \
         the pads are on the system's base pages - 16 KiB on Apple Silicon, already a \
         quarter of the dTLB pressure of a 4 KiB system"
            .to_string()
    } else {
        "this platform has no huge-page interface this miner knows how to ask for"
            .to_string()
    };
    Ok((p, len, PadPages::Base, note))
}

#[cfg(unix)]
fn map_base(want: usize) -> io::Result<(*mut u8, usize)> {
    let len = want.next_multiple_of(PAD_ALIGN);
    let p = mmap_aligned(len, PAD_ALIGN)?;
    Ok((p, len))
}

#[cfg(unix)]
fn release(addr: *mut u8, len: usize) {
    // SAFETY: a mapping this module made and has not yet released.
    unsafe {
        sys::munmap(addr.cast(), len);
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!(
    "plaine-pow-mine needs anonymous page mapping for the scratchpads \
     (VirtualAlloc on Windows, mmap on Unix)."
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BATCH;

    #[test]
    fn a_batch_is_one_2mib_page() {
        assert_eq!(BATCH * PAD_BYTES, 2 * 1024 * 1024);
    }

    #[test]
    fn pads_are_aligned_and_disjoint() {
        for huge in [false, true] {
            let pads = Pads::new(BATCH, huge).expect("the ordinary-page path must never fail");
            assert_eq!(pads.len(), BATCH);
            for j in 0..BATCH {
                let p = pads.slot_ptr(j) as usize;
                assert_eq!(p % PAD_ALIGN, 0, "pad {j} is not 64 KiB aligned");
                if j > 0 {
                    assert_eq!(p - pads.slot_ptr(j - 1) as usize, PAD_BYTES);
                }
            }

            let last = pads.words(BATCH - 1);
            assert_eq!(last[SCRATCH_WORDS - 1], 0, "Pads::new leaves the region zeroed");
        }
    }

    #[test]
    fn staged_fill_matches_consensus() {
        let Ok(iso) = Isochron::new() else {
            eprintln!("no hardware AES on this host; skipping");
            return;
        };
        let mut pads = Pads::new(4, true).expect("pads");
        let mut want = Scratch::new();
        for (j, seed) in [1u64, 0xdead_beef, u64::MAX, 0].into_iter().enumerate() {
            let got_seed = pads.fill(iso, j, seed);
            let want_seed = iso.fill(&mut want, seed);
            assert_eq!(got_seed, want_seed, "program seed differs for seed {seed:016x}");
            assert_eq!(pads.words(j), want.words(), "pad contents differ for seed {seed:016x}");
            assert_eq!(pads.checksum(j), want.checksum(), "padck differs");
        }
    }

    fn ranges(pads: &[Pads]) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        for p in pads {
            for j in 0..p.len() {
                let a = p.slot_ptr(j) as usize;
                v.push((a, a + PAD_BYTES));
            }
        }
        v
    }

    #[test]
    fn many_gives_disjoint_pads() {
        for workers in [2usize, 3, 8, 16] {
            let (pads, err) = Pads::many(workers, BATCH, true);
            assert!(err.is_none(), "the ordinary-page path must never fail: {err:?}");
            assert_eq!(pads.len(), workers, "every worker must get pads");

            for p in &pads {
                assert_eq!(p.len(), BATCH);
                if p.pages().is_huge() {
                    assert_eq!(
                        p.slot_ptr(0) as usize % (BATCH * PAD_BYTES),
                        0,
                        "a batch on huge pages must start on a 2 MiB boundary"
                    );
                }
                for j in 0..BATCH {
                    let a = p.slot_ptr(j) as usize;
                    assert_eq!(a % PAD_ALIGN, 0, "pad {j} is not 64 KiB aligned");
                    if j > 0 {
                        assert_eq!(a - p.slot_ptr(j - 1) as usize, PAD_BYTES);
                    }
                }

                assert_eq!(p.words(BATCH - 1)[SCRATCH_WORDS - 1], 0);
            }

            let mut r = ranges(&pads);
            r.sort_unstable();
            for w in r.windows(2) {
                assert!(
                    w[0].1 <= w[1].0,
                    "pads overlap: [{:#x},{:#x}) and [{:#x},{:#x})",
                    w[0].0,
                    w[0].1,
                    w[1].0,
                    w[1].1
                );
            }
        }
    }

    #[test]
    fn pads_survive_neighbours_dropped() {
        let (pads, err) = Pads::many(4, BATCH, true);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(pads.len(), 4);
        for (i, p) in pads.iter().enumerate() {
            for j in 0..BATCH {
                // SAFETY: slot_ptr checked j and returned a live PAD_BYTES window.
                unsafe { core::ptr::write_bytes(p.slot_ptr(j).cast::<u8>(), i as u8 + 1, PAD_BYTES) }
            }
        }

        let mut pads = pads;
        let keep = pads.remove(2);
        drop(pads.remove(0));
        drop(pads.pop().unwrap());
        drop(pads.pop().unwrap());
        assert!(pads.is_empty());

        for j in 0..BATCH {
            assert!(
                keep.words(j).iter().all(|&w| w == 0x0303_0303_0303_0303),
                "worker 2's pad {j} did not survive its neighbours being dropped"
            );
        }

        for j in 0..BATCH {
            // SAFETY: slot_ptr checked j and returned a live PAD_BYTES window.
            unsafe { core::ptr::write_bytes(keep.slot_ptr(j).cast::<u8>(), 0xA5, PAD_BYTES) }
        }
        assert!(keep.words(BATCH - 1).iter().all(|&w| w == 0xA5A5_A5A5_A5A5_A5A5));
        drop(keep);
    }

    #[test]
    fn many_on_ordinary_pages_fills() {
        let (mut pads, err) = Pads::many(3, BATCH, false);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(pads.len(), 3);
        for p in &pads {
            assert!(!p.pages().is_huge(), "--no-huge-pages must not silently take huge pages");
            assert_eq!(p.slot_ptr(0) as usize % PAD_ALIGN, 0);
        }
        let mut r = ranges(&pads);
        r.sort_unstable();
        for w in r.windows(2) {
            assert!(w[0].1 <= w[1].0, "ordinary-page pads overlap");
        }
        let Ok(iso) = Isochron::new() else {
            eprintln!("no hardware AES on this host; skipping the fill half");
            return;
        };
        let mut want = Scratch::new();
        for (i, p) in pads.iter_mut().enumerate() {
            let seed = 0x0BAD_F00D_u64.wrapping_add(i as u64);
            let got = p.fill(iso, 0, seed);
            assert_eq!(got, iso.fill(&mut want, seed));
            assert_eq!(p.words(0), want.words(), "worker {i}'s pad differs");
        }
    }

    #[test]
    fn many_reports_what_and_why() {
        let (none, err) = Pads::many(0, BATCH, true);
        assert!(none.is_empty());
        assert!(err.is_none(), "no workers is not a failure: {err:?}");
        let (one, err) = Pads::many(1, BATCH, true);
        assert_eq!(one.len(), 1);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(one[0].len(), BATCH);

        let (none, err) = Pads::many(4, 1usize << 40, false);
        assert!(none.is_empty(), "64 PiB of pads cannot have been mapped");
        let e = err.expect(
            "a miner that cannot map a worker's pads must report why, not silently run fewer threads",
        );
        assert!(!e.to_string().is_empty(), "the error has to say something");
    }

    #[test]
    fn the_allocator_says_why_it_got_the_page_size_it_got() {
        // Mapping a region records a note. Reporting code prefers it over a canned
        // explanation, because the canned one names SeLockMemoryPrivilege and is wrong
        // in the two commonest cases: the right is held and the allocation failed for
        // another reason, or huge pages were never asked for.
        let _pads = Pads::new(1, false).expect("one ordinary-page pad always maps");
        let note = last_note().expect("a mapping must record why it got what it got");
        assert!(!note.trim().is_empty(), "an empty note explains nothing");
        assert!(
            !note.contains("SeLockMemoryPrivilege"),
            "--no-huge-pages did not fail on a privilege, so the note must not blame one: {note}"
        );
    }

    #[test]
fn page_report_makes_no_false_claim() {

        let pads = Pads::new(BATCH, true).expect("pads");
        let now = observed_now();
        assert!(now.total() >= 1);

        assert!(observed_ever().total() >= now.total());
        if pads.pages().is_huge() {
            assert!(now.huge() >= 1);
            assert!(now.count(pads.pages()) >= 1, "this region's own mechanism is not counted");
        }

        assert!(!status_field().is_empty());
        eprintln!(
            "pads: {:?} - {}",
            pads.pages(),
            NOTE.get().map(String::as_str).unwrap_or("")
        );
    }

    fn tally_of(spec: &[(PadPages, usize)]) -> Summary {
        Summary::of(spec.iter().flat_map(|(k, n)| std::iter::repeat_n(*k, *n)))
    }

    #[test]
    fn mechanism_table_indexed_by_code() {
        for (i, k) in KINDS.into_iter().enumerate() {
            assert_eq!(k.code() as usize, i, "{k:?} does not sit at its own code");
        }
        assert!(!KINDS[0].is_huge(), "index 0 must be the ordinary-page slot");
        for k in KINDS.into_iter().skip(1) {
            assert!(k.is_huge(), "{k:?} is counted as huge but does not say it is");
        }

        let one_each = Summary::of(KINDS);
        assert_eq!(one_each.total(), 4);
        assert_eq!(one_each.huge(), 3);
        for k in KINDS {
            assert_eq!(one_each.count(k), 1, "{k:?} was not counted once");
        }
    }

    #[test]
    fn status_field_keeps_its_shape() {
        assert_eq!(field(tally_of(&[(PadPages::HugeTlb, 4)])), "pads 2M/hugetlb 4/4");
        assert_eq!(field(tally_of(&[(PadPages::Base, 8)])), "pads 4K 0/8");
        assert_eq!(field(tally_of(&[(PadPages::Large, 3), (PadPages::Base, 13)])), "pads 2M/large 3/16");
        assert_eq!(
            field(tally_of(&[(PadPages::HugeTlb, 1), (PadPages::Transparent, 3)])),
            "pads 2M/mixed 4/4"
        );

        assert_eq!(field(Summary::default()), "pads -");

        for s in [
            tally_of(&[(PadPages::HugeTlb, 4)]),
            tally_of(&[(PadPages::Base, 8)]),
            tally_of(&[(PadPages::Transparent, 2), (PadPages::Base, 6)]),
            tally_of(&[(PadPages::HugeTlb, 1), (PadPages::Large, 1)]),
        ] {
            let line = field(s);
            let words: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(words.len(), 3, "the field is three words: {words:?}");
            assert_eq!(words[0], "pads", "the label moved: {words:?}");
            assert!(!words[1].contains('/') || words[1].starts_with("2M/"), "{words:?}");
            assert_eq!(words[2], format!("{}/{}", s.huge(), s.total()));
        }
    }

    #[test]
    fn partial_success_is_not_complete() {
        let s = tally_of(&[(PadPages::Large, 4), (PadPages::Base, 8)]);
        assert_eq!(s.huge(), 4);
        assert_eq!(s.total(), 12);
        assert!(!s.mixed(), "one huge mechanism plus ordinary pages is not two mechanisms");
        assert_eq!(field(s), "pads 2M/large 4/12");
        assert!(!field(s).contains("12/12"), "a partial success reported as complete");
    }

    #[test]
    fn mixed_is_derived_not_remembered() {
        let messy = tally_of(&[(PadPages::HugeTlb, 1), (PadPages::Transparent, 7)]);
        assert!(messy.mixed());
        assert_eq!(messy.tag(), "2M/mixed");

        assert_eq!(messy.kind(), PadPages::Transparent);

        let clean = tally_of(&[(PadPages::HugeTlb, 8)]);
        assert!(!clean.mixed(), "one mechanism is not a mixture");
        assert_eq!(clean.tag(), "2M/hugetlb");

        assert_eq!(Summary::default().kind(), PadPages::Base);
        assert_eq!(Summary::default().tag(), "4K");
        assert_eq!(Summary::default().huge(), 0);
    }

    #[test]
    fn drop_leaves_live_tally_keeps_lifetime() {

        let ever_before = observed_ever().total();
        let pads = Pads::new(BATCH, false).expect("the ordinary-page path must never fail");
        let kind = pads.pages();
        assert!(observed_now().count(kind) >= 1, "a mapped region is not in the live tally");
        assert!(observed_ever().total() > ever_before, "a mapped region is not in the lifetime tally");

        let ever_with = observed_ever().total();
        drop(pads);

        assert!(
            observed_ever().total() >= ever_with,
            "dropping a region removed it from the lifetime tally; `--bench` reports after \
             joining its workers and would print a blank page line"
        );
    }
}
