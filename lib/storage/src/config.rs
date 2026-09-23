use std::path::PathBuf;

use plaine_consensus::constants::{BLOCKS_PER_YEAR, MAGIC_MAIN};


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Main,
}

impl Network {
    pub fn magic(self) -> [u8; 4] {
        MAGIC_MAIN
    }
}

// Tip fsyncs every block; Ibd fsyncs once per batch. We never touch redb's
// Eventual durability - on power loss it winds back to the last hard commit, and
// during IBD that can be hundreds of thousands of blocks of PoW to re-verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    Tip,
    Ibd,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryBudget {
    pub page_cache_bytes: usize,
    pub commit_staging_bytes: usize,
    pub reader_scratch_bytes: usize,
    pub recover_scratch_bytes: usize,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self {
            page_cache_bytes: 128 * 1024 * 1024,
            commit_staging_bytes: 2 * 1024 * 1024,
            reader_scratch_bytes: 64 * 1024,
            recover_scratch_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub data_dir: PathBuf,
    pub network: Network,
    pub prune: bool,
    pub body_retain_blocks: u64,
    pub txindex: bool,
    // Record which transactions touch each address, for per-address history over
    // RPC. Off by default, like txindex: it costs a body parse per block and a row
    // per party.
    pub addrindex: bool,
    pub page_cache_bytes: usize,

    // a hard cap, not a target. segment handles are an LRU, so open fds never
    // grow with chain height, peer count, or connection count.
    pub reader_fd_cap: usize,
    pub state_ckpt_interval: u64,

    // each retained checkpoint is a persistent savepoint that pins every page
    // freed since it was taken, so the cost is roughly linear in keep.
    pub state_ckpt_keep: u32,
    pub ibd_batch_blocks: Option<u32>,
    pub side_headers_cap: u64,
    pub invalid_cap: u64,
    pub hash_prefix_bytes: u32,
    pub strict_integrity: bool,
    pub anchor_mint_off: bool,
    pub anchor_mint_skip: Vec<u32>,
}

impl StoreConfig {
    pub fn new(data_dir: impl Into<PathBuf>, network: Network) -> Self {
        Self {
            data_dir: data_dir.into(),
            network,
            prune: true,
            body_retain_blocks: BLOCKS_PER_YEAR,
            txindex: false,
            addrindex: false,
            page_cache_bytes: MemoryBudget::default().page_cache_bytes,
            reader_fd_cap: 64,
            // State checkpoints are redb persistent savepoints, and each one pins
            // every page freed since it was taken. This used to be SEG_BLOCKS =
            // 4096, which in steady state (one seal per block) kept the oldest
            // savepoint alive for thousands of blocks: freed pages piled up and
            // chain.redb grew to hundreds of MB over a few hundred KB of live data,
            // and every rewind had to replay a whole interval. Small interval, keep
            // at 2. The hard rule: the retained savepoints must span at least
            // MAX_REORG_DEPTH (30) so any in-window reorg can rewind to a checkpoint
            // at or below its fork height - here interval*(keep-1) = 32 >= 30. In
            // steady state the file now tracks a ~keep*interval (64-block) window of
            // freed pages, a few MB. keep stays 2 so the IBD window (keep * the IBD
            // batch) is unchanged from before.
            state_ckpt_interval: 32,
            state_ckpt_keep: 2,
            ibd_batch_blocks: None,
            side_headers_cap: crate::SIDE_HEADERS_CAP,
            invalid_cap: crate::INVALID_CAP,
            hash_prefix_bytes: 8,
            strict_integrity: false,
            anchor_mint_off: false,
            anchor_mint_skip: Vec::new(),
        }
    }
}

// Fraction of the state tree a batch dirties, assuming writes land at random.
// It saturates exponentially, not linearly - the reason bigger batches stop
// paying their way past a point.
pub fn dirty_fraction(batch: u64, leaves: u64, writes_per_block: u64) -> f64 {
    if leaves == 0 {
        return 1.0;
    }
    let x = (writes_per_block * batch) as f64 / leaves as f64;
    1.0 - (-x).exp()
}

pub fn autotune_batch(measured_write_bytes_per_sec: u64) -> u32 {
    const REF_BANDWIDTH: u64 = 50_000_000;
    const REF_BATCH: u64 = 8_192;
    let scaled = (REF_BATCH * REF_BANDWIDTH)
        .checked_div(measured_write_bytes_per_sec)
        .unwrap_or(REF_BATCH);

    let up = scaled.next_power_of_two().max(1);
    let down = if up == scaled { up } else { up / 2 };
    let pow2 = if scaled - down <= up - scaled { down } else { up };
    pow2.clamp(2_048, 65_536) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autotune_is_bounded_and_inverse() {
        assert_eq!(autotune_batch(50_000_000), 8_192);
        assert!(autotune_batch(500_000_000) >= 2_048);
        assert_eq!(autotune_batch(500_000_000), 2_048);
        assert_eq!(autotune_batch(12_000_000), 32_768);
        assert_eq!(autotune_batch(1), 65_536);
        for bw in [1u64, 12_000_000, 50_000_000, 500_000_000, u32::MAX as u64] {
            let b = autotune_batch(bw);
            assert!((2_048..=65_536).contains(&b));
            assert!(b.is_power_of_two());
        }
    }

    #[test]
    fn saturation_is_exponential_not_linear() {
        let f1100 = dirty_fraction(1_100, 15_903, 15);
        assert!((0.60..0.70).contains(&f1100), "got {f1100}");
        let f2000 = dirty_fraction(2_000, 15_903, 15);
        assert!((0.83..0.87).contains(&f2000), "got {f2000}");

        let f10240 = dirty_fraction(10_240, 15_903, 15);
        assert!(f10240 > 0.99, "got {f10240}");
    }
}
