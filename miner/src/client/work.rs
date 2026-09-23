use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub const E1_BITS: u32 = 24;

pub const X_BITS: u32 = 64 - E1_BITS;

pub const X_BYTES: u64 = (X_BITS / 8) as u64;

pub const X_BYTES_MIN: u64 = 4;

pub const X_BITS_MIN: u32 = (X_BYTES_MIN * 8) as u32;

pub const THREAD_BITS: u32 = 8;

/// One worker owns one of `2^THREAD_BITS` nonce lanes, so this many workers is the hard
/// ceiling. Past it the lane index overflows THREAD_BITS, the top bits are cut by the
/// slice mask, and worker `MAX_WORKERS + k` walks lane `k`'s nonces exactly. The server
/// sees that as duplicate shares (error 22, +25 banscore each) and bans the IP after
/// four of them. Machines with more than 256 logical CPUs exist, and
/// `available_parallelism` is the default, so this has to be enforced, not documented.
pub const MAX_WORKERS: usize = 1 << THREAD_BITS;

pub const PREFIX_BYTES: usize = 124;

pub const JOB_SLOTS: usize = 4;

pub const SERVER_JOB_REFRESH: std::time::Duration = std::time::Duration::from_secs(15);

pub const STALE_CREDIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

pub const BITS_OFFSET: usize = 116;

pub fn network_target_from_prefix(prefix: &[u8; PREFIX_BYTES]) -> Option<[u8; 32]> {
    let bits = u32::from_le_bytes([
        prefix[BITS_OFFSET],
        prefix[BITS_OFFSET + 1],
        prefix[BITS_OFFSET + 2],
        prefix[BITS_OFFSET + 3],
    ]);
    target_be_from_bits(bits)
}

pub fn target_be_from_bits(bits: u32) -> Option<[u8; 32]> {
    let t = plaine_consensus::asert::Target::from_compact(bits).ok()?;
    let mut out = [0u8; 32];
    for (i, limb) in t.0.iter().enumerate() {
        out[24 - i * 8..32 - i * 8].copy_from_slice(&limb.to_be_bytes());
    }
    Some(out)
}

pub fn target_from_difficulty(d: u64) -> [u8; 32] {
    if d <= 1 {
        return [0xff; 32];
    }
    let dividend: [u64; 5] = [0, 0, 0, 0, 1];
    let mut q = [0u64; 5];
    let mut rem: u64 = 0;
    for i in (0..5).rev() {
        let cur = ((rem as u128) << 64) | dividend[i] as u128;
        q[i] = (cur / d as u128) as u64;
        rem = (cur % d as u128) as u64;
    }
    debug_assert_eq!(q[4], 0, "d >= 2 means the quotient fits in 256 bits");
    let mut out = [0u8; 32];
    for (i, limb) in q.iter().take(4).enumerate() {
        out[24 - i * 8..32 - i * 8].copy_from_slice(&limb.to_be_bytes());
    }
    out
}

#[inline]
pub fn compose(e1: u32, x: u64) -> u64 {
    compose_in(e1, x, X_BITS)
}

#[inline]
pub fn owns(e1: u32, nonce: u64) -> bool {
    owns_in(e1, nonce, X_BITS)
}

#[inline]
pub fn compose_in(e1: u32, x: u64, x_bits: u32) -> u64 {
    debug_assert!((X_BITS_MIN..=X_BITS).contains(&x_bits), "window {x_bits} is not one this miner rolls");
    ((e1 as u64) << x_bits) | (x & mask(x_bits))
}

#[inline]
pub fn owns_in(e1: u32, nonce: u64, x_bits: u32) -> bool {
    (nonce >> x_bits) == e1 as u64
}

#[inline]
const fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

#[inline]
pub fn e1_limit(x_bits: u32) -> u64 {
    1u64 << (64 - x_bits)
}

#[inline]
pub fn assemble(prefix: &[u8; PREFIX_BYTES], nonce: u64) -> [u8; 132] {
    let mut h = [0u8; 132];
    h[..PREFIX_BYTES].copy_from_slice(prefix);
    h[PREFIX_BYTES..].copy_from_slice(&nonce.to_le_bytes());
    h
}

pub fn nonce_hex(n: u64) -> String {
    super::json::hex(&n.to_le_bytes())
}

#[derive(Debug, Clone)]
pub struct JobView {
    pub job_id: u32,
    pub job_id_hex: String,
    pub prefix: [u8; PREFIX_BYTES],
    pub target: [u8; 32],
    pub network_target: [u8; 32],
    pub height: u64,
    pub live: bool,
}

impl JobView {
    pub fn empty() -> JobView {
        JobView {
            job_id: 0,
            job_id_hex: "00000000".into(),
            prefix: [0u8; PREFIX_BYTES],
            target: [0u8; 32],
            network_target: [0u8; 32],
            height: 0,
            live: false,
        }
    }
}

pub struct Shared {
    generation: AtomicU64,
    job: RwLock<Option<Arc<JobView>>>,
    e1: AtomicU64,
    x_bits: AtomicU64,
    stop: AtomicU64,
    hashes: AtomicU64,
    lanes: Vec<Lane>,
}

#[repr(align(64))]
struct Lane(AtomicU64);

impl Default for Shared {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Shared {
    pub fn new(threads: usize) -> Shared {
        Shared {
            generation: AtomicU64::new(0),
            job: RwLock::new(None),
            e1: AtomicU64::new(0),
            x_bits: AtomicU64::new(X_BITS as u64),
            stop: AtomicU64::new(0),
            hashes: AtomicU64::new(0),
            lanes: (0..threads).map(|_| Lane(AtomicU64::new(0))).collect(),
        }
    }

    #[inline]
    pub fn add_hashes(&self, n: u64) {
        self.hashes.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_hashes_from(&self, worker: usize, n: u64) {
        self.hashes.fetch_add(n, Ordering::Relaxed);
        if let Some(l) = self.lanes.get(worker) {
            l.0.fetch_add(n, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    #[inline]
    pub fn lane(&self, worker: usize) -> u64 {
        self.lanes.get(worker).map_or(0, |l| l.0.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn hashes(&self) -> u64 {
        self.hashes.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn publish(&self, j: JobView) {
        *self.job.write().expect("job lock") = Some(Arc::new(j));
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn job(&self) -> Option<Arc<JobView>> {
        self.job.read().expect("job lock").clone()
    }

    pub fn set_e1(&self, e1: u32) {
        self.set_window(e1, X_BITS);
    }

    pub fn set_window(&self, e1: u32, x_bits: u32) {
        let x_bits = x_bits.clamp(X_BITS_MIN, X_BITS);

        self.x_bits.store(x_bits as u64, Ordering::Release);
        self.e1.store(e1 as u64, Ordering::Release);
    }

    #[inline]
    pub fn e1(&self) -> u32 {
        self.e1.load(Ordering::Acquire) as u32
    }

    #[inline]
    pub fn x_bits(&self) -> u32 {
        self.x_bits.load(Ordering::Acquire) as u32
    }

    pub fn stop(&self) {
        self.stop.store(1, Ordering::Release);
    }

    #[inline]
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) != 0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Solution {
    pub job_id: u32,
    pub nonce: u64,
    pub pow_hash: [u8; 32],
    pub height: u64,
    pub is_block: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_example_reproduced() {
        let e1 = 0x00A3F2u32;
        let x = (3u64 << 32) | 42;
        let n = compose(e1, x);
        assert_eq!(n, 0x00A3_F203_0000_002A);
        assert_eq!(nonce_hex(n), "2a00000003f2a300");
        assert!(owns(e1, n));
        assert!(!owns(0x112233, n));
    }

    #[test]
    fn nonce_lands_at_offset_124() {
        let h = assemble(&[0xAB; PREFIX_BYTES], 0x00A3_F203_0000_002A);
        assert_eq!(&h[..PREFIX_BYTES], &[0xABu8; PREFIX_BYTES][..]);
        assert_eq!(&h[124..], &[0x2A, 0, 0, 0, 0x03, 0xF2, 0xA3, 0x00]);
        assert_eq!(&h[129..132], &[0xF2, 0xA3, 0x00]);
    }

    #[test]
    fn target_read_from_prefix_bits() {
        let mut prefix = [0u8; PREFIX_BYTES];
        let bits = plaine_consensus::constants::GENESIS_BITS;
        prefix[BITS_OFFSET..BITS_OFFSET + 4].copy_from_slice(&bits.to_le_bytes());
        let t = network_target_from_prefix(&prefix).expect("GENESIS_BITS decodes");
        assert_eq!(Some(t), target_be_from_bits(bits), "the prefix path and the bits path differ");

        assert_eq!(&t[..4], &[0x00, 0x00, 0xff, 0xff]);
        assert!(t[4..].iter().all(|&b| b == 0), "{t:?}");
    }

    #[test]
    fn empty_prefix_makes_impossible_target() {
        let prefix = [0u8; PREFIX_BYTES];
        assert_eq!(network_target_from_prefix(&prefix), Some([0u8; 32]));
        assert_eq!(JobView::empty().network_target, [0u8; 32]);
        assert!(!JobView::empty().live, "the placeholder job is never mined");
    }

    #[test]
    fn difficulty_one_and_zero_clamp() {
        assert_eq!(target_from_difficulty(0), [0xff; 32]);
        assert_eq!(target_from_difficulty(1), [0xff; 32]);

        let half = target_from_difficulty(2);
        assert_eq!(half[0], 0x80);
        assert!(half[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn x_bytes_is_not_extranonce2_size() {
        assert_eq!(X_BYTES, 5);
        assert_eq!(E1_BITS + X_BITS, 64);
    }

    #[test]
    fn full_window_composes_unchanged() {
        assert_eq!(compose_in(0x00A3F2, (3u64 << 32) | 42, X_BITS), 0x00A3_F203_0000_002A);

        for e1 in [0u32, 1, 0xA3, 0x00A3F2, 0xFF_FFFF] {
            for x in [0u64, 1, 42, (7u64 << 32) | 9, (1u64 << 40) - 1] {
                let n = compose_in(e1, x, X_BITS);
                assert_eq!(n, ((e1 as u64) << 40) | (x & ((1u64 << 40) - 1)), "e1 {e1:#x} x {x:#x}");
                assert_eq!(n, compose(e1, x), "the two spellings disagree at the full window");
                assert!(owns(e1, n));
                assert_eq!(owns_in(e1, n, X_BITS), owns(e1, n));
            }
        }

        let cbits = X_BITS - THREAD_BITS;
        assert_eq!(cbits, 32);
        for index in [0u64, 1, 5, 255] {
            for counter in [0u64, 1, 0xFFFF_FFFF, 0x1_0000_0000] {
                assert_eq!(
                    (index << cbits) | (counter & ((1u64 << cbits) - 1)),
                    (index << 32) | (counter & 0xFFFF_FFFF)
                );
            }
        }
    }

    #[test]
    fn narrow_window_workers_never_collide() {
        for x_bytes in X_BYTES_MIN..=X_BYTES {
            let x_bits = (x_bytes * 8) as u32;
            let cbits = x_bits - THREAD_BITS;

            let e1 = match 64 - x_bits {
                24 => 0x00A3F2u32,
                n => (0x00A3F2u32 << (n - 24)) | 0x5,
            };
            assert!((e1 as u64) < e1_limit(x_bits), "the test's own e1 does not fit {x_bits}");

            let mut seen = std::collections::HashSet::new();
            for index in 0u64..8 {
                for counter in 0u64..64 {
                    let x = (index << cbits) | (counter & ((1u64 << cbits) - 1));
                    let n = compose_in(e1, x, x_bits);
                    assert!(
                        owns_in(e1, n, x_bits),
                        "x_bytes {x_bytes}: worker {index} left its sub-slice at counter {counter}"
                    );

                    assert!(
                        owns(0x00A3F2, n),
                        "x_bytes {x_bytes}: worker {index} left the upstream 24-bit slice"
                    );
                    assert!(
                        seen.insert(n),
                        "x_bytes {x_bytes}: worker {index} counter {counter} produced a nonce \
                         another worker had already produced"
                    );
                }
            }
        }
    }

    #[test]
    fn narrowest_window_outlasts_job() {
        const JOB_SECS: f64 = 60.0;

        const RATE: f64 = 2_500.0;
        let seconds = |x_bytes: u64| {
            let cbits = (x_bytes * 8) as u32 - THREAD_BITS;
            (1u64 << cbits) as f64 / RATE
        };

        let three = seconds(3);
        assert!((26.0..27.0).contains(&three), "a three-byte window lasts {three}s, not ~26");
        assert!(three < JOB_SECS, "...and that is inside a job, which is the whole problem");

        let four = seconds(X_BYTES_MIN);
        assert!(four > 100.0 * JOB_SECS, "the floor gives only {four}s per worker");
        assert_eq!(X_BYTES_MIN, 4, "the floor moved without its arithmetic moving");

        assert_eq!(e1_limit(X_BITS_MIN), 1u64 << 32);
        assert!(e1_limit(X_BITS_MIN) <= u32::MAX as u64 + 1, "a narrower window would truncate e1");
    }

    #[test]
    fn oversized_extranonce1_is_out_of_range() {
        assert_eq!(e1_limit(X_BITS), 1u64 << 24, "the full window leaves 24 bits of slice");
        assert!((0x00FF_FFFFu64) < e1_limit(X_BITS));
        assert!((0x0100_0000u64) >= e1_limit(X_BITS), "a 25-bit slice does not fit a 24-bit field");

        assert!((0x0100_0000u64) < e1_limit(X_BITS_MIN));
    }

    #[test]
    fn no_window_uses_full() {
        let s = Shared::new(1);
        assert_eq!(s.x_bits(), X_BITS, "a fresh Shared must roll the whole window");
        s.set_e1(0x00A3F2);
        assert_eq!((s.e1(), s.x_bits()), (0x00A3F2, X_BITS));
        s.set_window(0x0100_0005, X_BITS_MIN);
        assert_eq!((s.e1(), s.x_bits()), (0x0100_0005, X_BITS_MIN));

        s.set_window(1, 8);
        assert_eq!(s.x_bits(), X_BITS_MIN);
        s.set_window(1, 64);
        assert_eq!(s.x_bits(), X_BITS);
    }
}
