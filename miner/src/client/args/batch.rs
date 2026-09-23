//! How many nonces one W^X seal covers.
//!
//! `sizes::BATCH` is the capacity of the code region: 32 slots of 16 KiB, mapped once.
//! How many of those slots a worker actually fills per seal is a runtime choice, because
//! the right answer is a property of the machine's cache hierarchy, not of the protocol.
//!
//! `mine_hash_batch_on` fills every pad in the batch before it runs the first program.
//! That makes the fill phase's working set `batch * 64 KiB`. When it outgrows the share
//! of L2 the thread owns, every pad is evicted before its program runs and each hash
//! starts by dragging 64 KiB back from L3 or memory. Keeping the batch inside L2 is
//! worth more than amortising the seal over more nonces.
//!
//! Measured on a Ryzen 7 8745HS (Zen 4, 1 MiB L2 per core, 2 threads sharing it),
//! 16 threads pinned:
//!
//! | batch | pads/thread | kH/s  |
//! |-------|-------------|-------|
//! | 4     | 256 KiB     | 27.12 |
//! | 8     | 512 KiB     | 26.10 |
//! | 16    | 1 MiB       | 24.88 |
//! | 32    | 2 MiB       | 24.88 |
//! | 64    | 4 MiB       | 24.30 |
//!
//! Monotone, and the knee sits where the pads stop fitting. The heuristic below is
//! calibrated against that one curve, so `--batch N` exists to overrule it.

use super::topo::Topology;
use crate::pad::PAD_BYTES;
use crate::sizes::BATCH;

/// Never seal for fewer than this: each seal is one `VirtualProtect`/`mprotect`, and the
/// upstream comment records per-nonce flips costing 3.65x on a 32-core EPYC.
pub const MIN_BATCH: usize = 2;

/// Used when the machine will not say how big its L2 is. Not the region capacity: 32
/// slots is 2 MiB of pads, which overflows the per-thread L2 share of every CPU this
/// miner currently runs on, so guessing high is the worse mistake.
pub const UNKNOWN_L2_BATCH: usize = 8;

/// Fraction of a thread's L2 share the pads may claim. The rest is code, stack and the
/// pad being executed right now.
const L2_SHARE_NUMER: u32 = 1;
const L2_SHARE_DENOM: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub batch: usize,
    pub why: String,
}

/// L2 bytes each thread can count on, given how many threads share one L2.
///
/// `l2_shared` is how many CPUs report the same L2 instance. Where the platform does not
/// say, fall back to counting SMT siblings, and where it says nothing at all, give up
/// rather than invent a number.
fn l2_per_thread(topo: &Topology, threads: usize) -> Option<u64> {
    let cpu = topo.cpus.iter().find(|c| c.l2_kib.is_some())?;
    let l2 = cpu.l2_kib? as u64 * 1024;

    let sharers = match cpu.l2_shared {
        Some(n) if n >= 1 => n,
        _ => topo
            .cores()
            .and_then(|cs| cs.iter().map(|c| c.len()).max())
            .unwrap_or(1),
    };

    // Asking for fewer workers than the machine has CPUs means some L2s go unshared, but
    // we cannot know which, so keep the pessimistic per-core figure.
    let _ = threads;
    Some(l2 / sharers.max(1) as u64)
}

pub fn auto(topo: &Topology, threads: usize) -> Choice {
    let Some(per_thread) = l2_per_thread(topo, threads) else {
        return Choice {
            batch: UNKNOWN_L2_BATCH,
            why: format!(
                "this machine does not report its L2 size ({}), so the batch falls back to \
                 {UNKNOWN_L2_BATCH}; pass --batch N to set it from a measurement",
                topo.source
            ),
        };
    };

    let budget = per_thread * L2_SHARE_NUMER as u64 / L2_SHARE_DENOM as u64;
    let fits = (budget / PAD_BYTES as u64) as usize;
    let batch = fits.clamp(MIN_BATCH, BATCH);

    Choice {
        batch,
        why: format!(
            "L2 gives {} KiB per thread, so {batch} pads ({} KiB) keep the fill inside it",
            per_thread / 1024,
            batch * PAD_BYTES / 1024
        ),
    }
}

/// Validate an explicit `--batch N`.
pub fn check(n: usize) -> Result<usize, String> {
    if n == 0 {
        return Err("--batch needs at least 1".into());
    }
    if n > BATCH {
        return Err(format!(
            "--batch {n} exceeds the {BATCH}-slot code region; the region is mapped once at \
             a fixed size, so {BATCH} is the ceiling"
        ));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::args::topo::Cpu;

    fn machine(l2_kib: Option<u32>, shared: Option<usize>, cores: usize, smt: usize) -> Topology {
        let mut cpus = Vec::new();
        for core in 0..cores {
            for _ in 0..smt {
                cpus.push(Cpu {
                    id: cpus.len(),
                    group: 0,
                    package: 0,
                    core: core as i32,
                    class: None,
                    l2_kib,
                    l2_shared: shared,
                });
            }
        }
        Topology { model: "test".into(), cpus, source: "test", notes: Vec::new() }
    }

    #[test]
    fn the_measured_machine_lands_on_four() {
        // Zen 4: 1 MiB L2 per core, two SMT threads sharing it. 4 is what the sweep found.
        let t = machine(Some(1024), Some(2), 8, 2);
        assert_eq!(auto(&t, 16).batch, 4);
    }

    #[test]
    fn sibling_count_stands_in_for_a_missing_share_count() {
        let named = machine(Some(1024), Some(2), 8, 2);
        let unnamed = machine(Some(1024), None, 8, 2);
        assert_eq!(auto(&named, 16).batch, auto(&unnamed, 16).batch);
    }

    #[test]
    fn a_core_that_owns_its_l2_gets_a_bigger_batch() {
        let smt = machine(Some(1024), Some(2), 8, 2);
        let solo = machine(Some(1024), Some(1), 8, 1);
        assert!(
            auto(&solo, 8).batch > auto(&smt, 16).batch,
            "no sibling means the whole L2, which is room for more pads"
        );
    }

    #[test]
    fn a_tiny_l2_still_seals_more_than_one_nonce() {
        let t = machine(Some(64), Some(2), 4, 2);
        assert_eq!(auto(&t, 8).batch, MIN_BATCH, "the floor is what stops per-nonce sealing");
    }

    #[test]
    fn a_huge_l2_stops_at_the_region_capacity() {
        let t = machine(Some(64 * 1024), Some(1), 4, 1);
        assert_eq!(auto(&t, 4).batch, BATCH, "the code region cannot hold more than {BATCH}");
    }

    #[test]
    fn an_unknown_l2_says_so_and_does_not_guess_high() {
        let t = machine(None, None, 8, 2);
        let c = auto(&t, 16);
        assert_eq!(c.batch, UNKNOWN_L2_BATCH);
        assert!(c.why.contains("--batch"), "it has to name the escape hatch: {}", c.why);
        assert!(c.batch < BATCH, "guessing the capacity is the expensive mistake");
    }

    #[test]
    fn explicit_values_are_bounded_by_the_region() {
        assert_eq!(check(1), Ok(1));
        assert_eq!(check(BATCH), Ok(BATCH));
        assert!(check(0).is_err());
        assert!(check(BATCH + 1).is_err());
    }

    #[test]
    fn every_automatic_choice_is_a_legal_explicit_one() {
        for l2 in [Some(64u32), Some(256), Some(512), Some(1024), Some(2048), None] {
            for smt in [1usize, 2] {
                let t = machine(l2, Some(smt), 8, smt);
                let c = auto(&t, 8 * smt);
                assert_eq!(check(c.batch), Ok(c.batch), "l2={l2:?} smt={smt}");
            }
        }
    }
}
