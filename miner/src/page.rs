use std::io;

use crate::NATIVE_CODE_BYTES;
use crate::sizes::{BATCH, REGION_BYTES, SLOT_BYTES};

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::io;

    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const MEM_RELEASE: u32 = 0x8000;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const PAGE_EXECUTE_READ: u32 = 0x20;

    pub const WRITABLE_PROTECTIONS: [u32; 4] = [
        0x04,
        0x08,
        0x40,
        0x80,
    ];

    #[repr(C)]
    #[derive(Default)]
    pub struct MemoryBasicInformation {
        pub base_address: *mut c_void,
        pub allocation_base: *mut c_void,
        pub allocation_protect: u32,
        pub _alignment1: u32,
        pub region_size: usize,
        pub state: u32,
        pub protect: u32,
        pub ty: u32,
        pub _alignment2: u32,
    }

    extern "system" {
        pub fn VirtualAlloc(
            addr: *mut c_void,
            size: usize,
            alloc_type: u32,
            protect: u32,
        ) -> *mut c_void;
        pub fn VirtualProtect(
            addr: *mut c_void,
            size: usize,
            new_protect: u32,
            old_protect: *mut u32,
        ) -> i32;
        pub fn VirtualFree(addr: *mut c_void, size: usize, free_type: u32) -> i32;
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn FlushInstructionCache(
            process: *mut c_void,
            base: *const c_void,
            size: usize,
        ) -> i32;
        pub fn VirtualQuery(
            addr: *const c_void,
            buf: *mut MemoryBasicInformation,
            len: usize,
        ) -> usize;
    }

    pub fn reserve(len: usize) -> io::Result<*mut u8> {
        // SAFETY: null base, kernel picks the address. committed RW, never X here.
        let p = unsafe {
            VirtualAlloc(
                core::ptr::null_mut(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(p.cast::<u8>())
    }

    pub fn protect(addr: *mut u8, len: usize, exec: bool) -> io::Result<()> {
        let want = if exec {
            PAGE_EXECUTE_READ
        } else {
            PAGE_READWRITE
        };
        let mut old = 0u32;

        // SAFETY: addr/len come straight from reserve(); old catches the previous flags.
        let ok = unsafe { VirtualProtect(addr.cast(), len, want, &mut old) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if exec {
            // same region, now X. the pseudo-handle from GetCurrentProcess needs no CloseHandle.
            // SAFETY: covered by the reserve() invariant above.
            let ok = unsafe { FlushInstructionCache(GetCurrentProcess(), addr.cast(), len) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn release(addr: *mut u8, _len: usize) {
        // SAFETY: MEM_RELEASE wants the reserve() base and size 0. frees the whole thing once.
        unsafe {
            VirtualFree(addr.cast(), 0, MEM_RELEASE);
        }
    }

    pub fn query(addr: *const u8, len: usize) -> super::Protection {
        let mut mbi = MemoryBasicInformation::default();

        // SAFETY: addr sits inside a live region; mbi is a local, and the len we pass is its own.
        let n = unsafe {
            VirtualQuery(
                addr.cast(),
                &mut mbi,
                core::mem::size_of::<MemoryBasicInformation>(),
            )
        };
        if n == 0 {
            return super::Protection {
                writable: None,
                executable: None,
                raw: format!("VirtualQuery failed: {}", io::Error::last_os_error()),
            };
        }
        let covers = mbi.region_size >= len;
        super::Protection {
            writable: Some(WRITABLE_PROTECTIONS.contains(&mbi.protect)),
            executable: Some(matches!(mbi.protect, 0x10 | 0x20 | 0x40 | 0x80)),
            raw: format!(
                "Protect={:#06x} State={:#x} RegionSize={} covers_region={covers}",
                mbi.protect, mbi.state, mbi.region_size
            ),
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::ffi::c_void;
    use std::io;

    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const PROT_EXEC: i32 = 4;
    pub const MAP_PRIVATE: i32 = 0x02;

    #[cfg(not(target_vendor = "apple"))]
    pub const MAP_ANONYMOUS: i32 = 0x20;
    #[cfg(target_vendor = "apple")]
    pub const MAP_ANONYMOUS: i32 = 0x1000;

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    pub const MAP_JIT: i32 = 0x0800;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        #[cfg_attr(all(target_vendor = "apple", target_arch = "aarch64"), allow(dead_code))]
        fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    extern "C" {
        fn pthread_jit_write_protect_np(enabled: i32);

        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    thread_local! {
        static WRITE_PROTECTED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    }

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    fn set_write_protect(on: bool) {
        // one int in, no memory touched: this flips the calling thread's own W^X latch.
        // SAFETY: the whole point of MAP_JIT - protection is per-thread state, not a syscall.
        unsafe { pthread_jit_write_protect_np(i32::from(on)) };
        WRITE_PROTECTED.with(|c| c.set(on));
    }

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    pub fn reserve(len: usize) -> io::Result<*mut u8> {
        // SAFETY: null hint, kernel chooses. len nonzero, fd -1 for anon. RWX because MAP_JIT
        // maps it that way in the VM layer; W^X is then enforced per-thread, see above.
        let p = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_JIT,
                -1,
                0,
            )
        };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }

        set_write_protect(false);
        Ok(p.cast::<u8>())
    }

    #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
    pub fn reserve(len: usize) -> io::Result<*mut u8> {
        // plain RW anon map; mprotect flips it to X later. no MAP_JIT off Apple.
        // SAFETY: null hint kernel-chosen, len nonzero, fd -1.
        let p = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(p.cast::<u8>())
    }

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    pub fn protect(addr: *mut u8, len: usize, exec: bool) -> io::Result<()> {
        if exec {
            set_write_protect(true);

            // SAFETY: [addr, addr+len) was just made executable; flush its stale i-cache lines.
            unsafe { sys_icache_invalidate(addr.cast(), len) };
        } else {
            set_write_protect(false);
        }
        Ok(())
    }

    #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
    pub fn protect(addr: *mut u8, len: usize, exec: bool) -> io::Result<()> {
        let want = if exec {
            PROT_READ | PROT_EXEC
        } else {
            PROT_READ | PROT_WRITE
        };

        // SAFETY: exactly the reserve() mapping. one syscall flips the lot W<->X.
        if unsafe { mprotect(addr.cast(), len, want) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if exec {
            // aarch64 keeps separate i/d caches, so a bare mprotect is not enough.
            // SAFETY: [addr, addr+len) is the range we just turned executable.
            #[cfg(target_arch = "aarch64")]
            unsafe {
                sync_icache(addr, len)
            };
        }
        Ok(())
    }

    /// Makes freshly written code at [start, start+len) visible to instruction
    /// fetch: what compiler-rt's and libgcc's `__clear_cache` do on aarch64, done
    /// here so a build that links no C runtime (a static musl binary, which runs
    /// on Android without the NDK) needs no such symbol. Clean the data cache to
    /// the point of unification line by line, wait, invalidate the instruction
    /// cache over the same range, wait, and resynchronise the pipeline. Line sizes
    /// come from CTR_EL0, which Linux lets user space read.
    ///
    /// # Safety
    /// [start, start+len) must be mapped.
    #[cfg(target_arch = "aarch64")]
    unsafe fn sync_icache(start: *const u8, len: usize) {
        use core::arch::asm;
        let ctr: u64;
        // SAFETY: a read of an EL0-accessible system register.
        unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
        let dline = 4usize << ((ctr >> 16) & 0xf);
        let iline = 4usize << (ctr & 0xf);
        let begin = start as usize;
        let end = begin + len;
        let mut a = begin & !(dline - 1);
        while a < end {
            // SAFETY: cache maintenance on an address inside the caller's mapping.
            unsafe { asm!("dc cvau, {}", in(reg) a, options(nostack, preserves_flags)) };
            a += dline;
        }
        // SAFETY: barriers have no operands.
        unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
        let mut a = begin & !(iline - 1);
        while a < end {
            // SAFETY: as above.
            unsafe { asm!("ic ivau, {}", in(reg) a, options(nostack, preserves_flags)) };
            a += iline;
        }
        // SAFETY: barriers have no operands.
        unsafe { asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
    }

    pub fn release(addr: *mut u8, len: usize) {
        // SAFETY: reserve()'s mapping, unmapped exactly once from Drop.
        unsafe {
            munmap(addr.cast(), len);
        }
    }

    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    pub fn query(addr: *const u8, len: usize) -> super::Protection {
        let wp = WRITE_PROTECTED.with(|c| c.get());
        super::Protection {
            writable: Some(!wp),
            executable: Some(wp),
            raw: format!(
                "MAP_JIT {addr:p}+{len}: pthread_jit_write_protect_np={} on this thread. \
                 not a kernel report - the mapping is RWX in the VM layer and W^X is a \
                 per-thread hardware state with no public getter, so this is what this \
                 thread last set. Unverified on hardware.",
                i32::from(wp)
            ),
        }
    }

    #[cfg(not(all(target_vendor = "apple", target_arch = "aarch64")))]
    pub fn query(addr: *const u8, _len: usize) -> super::Protection {
        let want = addr as usize;
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
            return super::Protection {
                writable: None,
                executable: None,
                raw: "no /proc/self/maps on this platform".to_string(),
            };
        };
        for line in maps.lines() {
            let Some((range, rest)) = line.split_once(' ') else {
                continue;
            };
            let Some((lo, hi)) = range.split_once('-') else {
                continue;
            };
            let (Ok(lo), Ok(hi)) = (
                usize::from_str_radix(lo, 16),
                usize::from_str_radix(hi, 16),
            ) else {
                continue;
            };
            if want >= lo && want < hi {
                let perms = rest.split_whitespace().next().unwrap_or("");
                return super::Protection {
                    writable: Some(perms.contains('w')),
                    executable: Some(perms.contains('x')),
                    raw: format!("{range} {perms}"),
                };
            }
        }
        super::Protection {
            writable: None,
            executable: None,
            raw: format!("address {want:#x} not found in /proc/self/maps"),
        }
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!(
    "plaine-pow-mine needs W^X page mapping (VirtualAlloc/VirtualProtect on Windows, \
     mmap/mprotect on Unix). There is no portable fallback and RWX is not an option."
);

#[derive(Debug, Clone)]
pub struct Protection {
    pub writable: Option<bool>,
    pub executable: Option<bool>,
    pub raw: String,
}

struct Region {
    addr: *mut u8,
    len: usize,
}

impl Region {
    fn new() -> io::Result<Self> {
        Ok(Region {
            addr: sys::reserve(REGION_BYTES)?,
            len: REGION_BYTES,
        })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        sys::release(self.addr, self.len);
    }
}

// Two states of one region: CodeW writable, CodeX executable. seal/unseal move by value, so the
// borrow checker, not a comment, is what stops a write to executable pages. never both at once.
pub struct CodeW(Region);

pub struct CodeX(Region);

impl CodeW {
    pub fn new() -> io::Result<Self> {
        Ok(CodeW(Region::new()?))
    }

    pub fn region_mut(&mut self) -> &mut [u8] {
        // SAFETY: the whole live mapping, and &mut self makes the borrow exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.0.addr, self.0.len) }
    }

    pub fn code_mut(&mut self) -> &mut [u8; NATIVE_CODE_BYTES] {
        // SAFETY: region holds >= NATIVE_CODE_BYTES writable, &mut self is exclusive.
        unsafe { &mut *self.0.addr.cast::<[u8; NATIVE_CODE_BYTES]>() }
    }

    pub fn slot_mut(&mut self, slot: usize) -> &mut [u8; NATIVE_CODE_BYTES] {
        assert!(slot < BATCH, "slot {slot} out of range for a {BATCH}-slot region");

        // SAFETY: slot < BATCH asserted, NATIVE_CODE_BYTES <= SLOT_BYTES; the array stays in-region.
        unsafe { &mut *self.0.addr.add(slot * SLOT_BYTES).cast::<[u8; NATIVE_CODE_BYTES]>() }
    }

    // one mprotect flips the whole region W->X; the cost amortizes over all BATCH slots, which is
    // the reason we emit every slot before sealing rather than flip per nonce.
    pub fn seal(self) -> io::Result<CodeX> {
        sys::protect(self.0.addr, self.0.len, true)?;
        Ok(CodeX(self.0))
    }

    pub fn protection(&self) -> Protection {
        sys::query(self.0.addr, self.0.len)
    }
}

#[cfg(target_arch = "x86_64")]
type JitFn = unsafe extern "sysv64" fn(*mut u64, u64, *const u8) -> u64;

#[cfg(target_arch = "aarch64")]
type JitFn = unsafe extern "C" fn(*mut u64, u64, *const u8) -> u64;

impl CodeX {
    /// # Safety
    /// The region must hold a complete program emitted for the current arch and be executable.
    pub unsafe fn call(
        &self,
        pad: &mut plaine_pow::Scratch,
        seed: u64,
        key: &'static [u8; 16],
    ) -> u64 {
        // SAFETY: the base holds a sealed program matching the JitFn ABI (caller's contract).
        let f: JitFn = unsafe { core::mem::transmute::<*mut u8, JitFn>(self.0.addr) };

        // SAFETY: the call itself rides on that same contract.
        unsafe { f(pad.as_mut_ptr(), seed, key.as_ptr()) }
    }

    /// # Safety
    /// Slot `slot` must hold a complete emitted program; see [`call`](Self::call).
    pub unsafe fn call_slot(
        &self,
        slot: usize,
        pad: &mut plaine_pow::Scratch,
        seed: u64,
        key: &'static [u8; 16],
    ) -> u64 {
        // SAFETY: forwards the caller's contract straight to call_slot_ptr.
        unsafe { self.call_slot_ptr(slot, pad.as_mut_ptr(), seed, key) }
    }

    /// # Safety
    /// Slot `slot` must hold a complete emitted program and `pad` must point to a live scratch.
    pub unsafe fn call_slot_ptr(
        &self,
        slot: usize,
        pad: *mut u64,
        seed: u64,
        key: &'static [u8; 16],
    ) -> u64 {
        debug_assert!(slot < BATCH, "slot {slot} out of range for a {BATCH}-slot region");

        // SAFETY: slot < BATCH, each slot SLOT_BYTES wide; the offset stays in-region.
        let entry = unsafe { self.0.addr.add(slot * SLOT_BYTES) };

        // SAFETY: that slot holds a sealed program matching the JitFn ABI (caller's contract).
        let f: JitFn = unsafe { core::mem::transmute::<*mut u8, JitFn>(entry) };

        // SAFETY: and calling it is the last clause of that same contract.
        unsafe { f(pad, seed, key.as_ptr()) }
    }

    pub fn unseal(self) -> io::Result<CodeW> {
        sys::protect(self.0.addr, self.0.len, false)?;
        Ok(CodeW(self.0))
    }

    pub fn protection(&self) -> Protection {
        sys::query(self.0.addr, self.0.len)
    }
}
