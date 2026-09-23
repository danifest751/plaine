mod common;

use common::{serial, Scratch};
use plaine_consensus::codec::{AuthorNote, BlockBody, CoinbaseTx, TransferTx};
use plaine_consensus::constants::HEADER_BYTES;
use plaine_consensus::crypto::{address_payload, header_hash};
use plaine_storage::{
    open, AddrHistory, AddrHit, BlockToCommit, DurabilityMode, ReorgPlan, StoreError,
};

// Blocks with real, parseable bodies. The index only reads who the parties are,
// so signatures and state deltas can stay empty: validity is the chain crate's
// business, and the store is told the block is valid by the time it commits it.

struct Blk {
    header: [u8; HEADER_BYTES],
    hash: [u8; 32],
    height: u64,
    body: Vec<u8>,
}

impl Blk {
    fn commit(&self) -> BlockToCommit<'_> {
        BlockToCommit {
            header: &self.header,
            hash: self.hash,
            height: self.height,
            body: &self.body,
            deltas: &[],
            undo: &[],
            issued_delta: 0,
            chainwork: {
                let mut w = [0u8; 32];
                w[24..32].copy_from_slice(&(self.height + 1).to_be_bytes());
                w
            },
            txids: None,
        }
    }
}

fn pubkey(i: u8) -> [u8; 32] {
    [i; 32]
}

fn addr(i: u8) -> [u8; 20] {
    address_payload(&pubkey(i))
}

fn coinbase(height: u64, to: [u8; 20]) -> Vec<u8> {
    CoinbaseTx {
        height,
        to,
        reward: 200_000,
        fees: 0,
        note: AuthorNote { encoding: 0, payload: Vec::new() },
    }
    .encode()
    .expect("coinbase encodes")
}

fn transfer(from: u8, to: [u8; 20], nonce: u64) -> Vec<u8> {
    TransferTx {
        from_pub: pubkey(from),
        to,
        amount: 1_000,
        fee: 1_000,
        nonce,
        sig: [0u8; 64],
    }
    .encode()
    .to_vec()
}

fn block(height: u64, prev: [u8; 32], variant: u64, txs: &[Vec<u8>]) -> Blk {
    let mut header = [0u8; HEADER_BYTES];
    header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
    header[4..12].copy_from_slice(&height.to_le_bytes());
    header[12..44].copy_from_slice(&prev);
    header[108..116].copy_from_slice(&(1_700_000_000u64 + height * 60).to_le_bytes());
    header[116..120].copy_from_slice(&0x1F00_FFFFu32.to_le_bytes());
    header[124..132].copy_from_slice(&(variant ^ height).to_le_bytes());
    let refs: Vec<&[u8]> = txs.iter().map(|t| t.as_slice()).collect();
    Blk {
        header,
        hash: header_hash(&header),
        height,
        body: BlockBody::encode(&refs).expect("body encodes"),
    }
}

/// A = 1, B = 2, C = 3.
///   h0: coinbase -> A
///   h1: coinbase -> B, A -> C
///   h2: coinbase -> A, C -> A
fn three_blocks() -> Vec<Blk> {
    let b0 = block(0, [0u8; 32], 1, &[coinbase(0, addr(1))]);
    let b1 = block(1, b0.hash, 1, &[coinbase(1, addr(2)), transfer(1, addr(3), 0)]);
    let b2 = block(2, b1.hash, 1, &[coinbase(2, addr(1)), transfer(3, addr(1), 0)]);
    vec![b0, b1, b2]
}

fn hits(h: AddrHistory) -> (Vec<(u64, u16)>, bool) {
    match h {
        AddrHistory::Page { hits, more, .. } => {
            (hits.into_iter().map(|AddrHit { height, index }| (height, index)).collect(), more)
        }
        AddrHistory::NotIndexed => panic!("the index is on in this test"),
    }
}

fn indexed_store(name: &str) -> (Scratch, plaine_storage::Committer, plaine_storage::StoreReader) {
    let s = Scratch::new(name);
    let mut cfg = s.cfg();
    cfg.addrindex = true;
    let (mut c, r, _) = open(cfg).expect("open");
    c.set_mode(DurabilityMode::Ibd).unwrap();
    (s, c, r)
}

#[test]
fn history_is_newest_first_and_names_every_party() {
    let _g = serial();
    let (_s, mut c, r) = indexed_store("addr-parties");
    let bs = three_blocks();
    c.extend(&bs.iter().map(Blk::commit).collect::<Vec<_>>()).unwrap();
    c.flush().unwrap();

    assert_eq!(
        hits(r.addr_history(&addr(1), None, 10).unwrap()).0,
        vec![(2, 1), (2, 0), (1, 1), (0, 0)],
        "A: received from C, mined h2, sent to C, mined h0"
    );
    assert_eq!(hits(r.addr_history(&addr(2), None, 10).unwrap()).0, vec![(1, 0)]);
    assert_eq!(
        hits(r.addr_history(&addr(3), None, 10).unwrap()).0,
        vec![(2, 1), (1, 1)],
        "C is a party as receiver at h1 and as sender at h2"
    );
    assert_eq!(hits(r.addr_history(&addr(9), None, 10).unwrap()).0, Vec::new());
}

#[test]
fn paging_resumes_strictly_below_the_cursor() {
    let _g = serial();
    let (_s, mut c, r) = indexed_store("addr-paging");
    let bs = three_blocks();
    c.extend(&bs.iter().map(Blk::commit).collect::<Vec<_>>()).unwrap();
    c.flush().unwrap();

    let (first, more) = hits(r.addr_history(&addr(1), None, 2).unwrap());
    assert_eq!(first, vec![(2, 1), (2, 0)]);
    assert!(more, "two older hits remain");

    let (second, more) = hits(r.addr_history(&addr(1), Some((2, 0)), 2).unwrap());
    assert_eq!(second, vec![(1, 1), (0, 0)], "the cursor itself is not repeated");
    assert!(!more, "that was the oldest");

    let (exact, more) = hits(r.addr_history(&addr(1), None, 4).unwrap());
    assert_eq!(exact.len(), 4);
    assert!(!more, "a page that ends exactly at the oldest hit has no more");
}

#[test]
fn a_transfer_to_oneself_is_one_row() {
    let _g = serial();
    let (_s, mut c, r) = indexed_store("addr-self");
    let b0 = block(0, [0u8; 32], 7, &[coinbase(0, addr(1)), transfer(1, addr(1), 0)]);
    c.extend(&[b0.commit()]).unwrap();
    c.flush().unwrap();
    assert_eq!(hits(r.addr_history(&addr(1), None, 10).unwrap()).0, vec![(0, 1), (0, 0)]);
}

#[test]
fn off_by_default_and_says_so() {
    let _g = serial();
    let s = Scratch::new("addr-off");
    let cfg = s.cfg();
    assert!(!cfg.addrindex, "the index costs a parse per block, so it is opt-in");
    let (mut c, r, _) = open(cfg).expect("open");
    let bs = three_blocks();
    c.extend(&bs.iter().map(Blk::commit).collect::<Vec<_>>()).unwrap();
    c.flush().unwrap();
    assert_eq!(r.addr_history(&addr(1), None, 10).unwrap(), AddrHistory::NotIndexed);
    assert_eq!(r.addrindex_from(), None);
}

#[test]
fn switched_on_later_it_covers_from_there_and_persists() {
    let _g = serial();
    let s = Scratch::new("addr-later");
    let bs = three_blocks();
    {
        let (mut c, _r, _) = open(s.cfg()).expect("open without index");
        c.extend(&[bs[0].commit()]).unwrap();
        c.flush().unwrap();
    }
    {
        let mut cfg = s.cfg();
        cfg.addrindex = true;
        let (mut c, r, _) = open(cfg).expect("reopen with index");
        c.extend(&[bs[1].commit(), bs[2].commit()]).unwrap();
        c.flush().unwrap();
        assert_eq!(r.addrindex_from(), Some(1));
        match r.addr_history(&addr(1), None, 10).unwrap() {
            AddrHistory::Page { indexed_from, hits, .. } => {
                assert_eq!(indexed_from, 1, "the caller must be told height 0 is not covered");
                assert_eq!(hits.len(), 3, "h2 twice and h1 once; h0 was committed unindexed");
            }
            other => panic!("{other:?}"),
        }
    }
    let mut cfg = s.cfg();
    cfg.addrindex = true;
    let (_c, r, _) = open(cfg).expect("reopen");
    assert_eq!(r.addrindex_from(), Some(1), "where the index begins survives a restart");
    assert_eq!(hits(r.addr_history(&addr(3), None, 10).unwrap()).0, vec![(2, 1), (1, 1)]);
}

#[test]
fn rows_outlive_a_reorg_and_the_new_branch_is_indexed_too() {
    let _g = serial();
    let (_s, mut c, r) = indexed_store("addr-reorg");
    let bs = three_blocks();
    c.extend(&bs.iter().map(Blk::commit).collect::<Vec<_>>()).unwrap();
    c.flush().unwrap();

    // Replace h2 (coinbase -> A, C -> A) with coinbase -> B, B -> C.
    let alt = block(2, bs[1].hash, 99, &[coinbase(2, addr(2)), transfer(2, addr(3), 0)]);
    let rollback = [2u64];
    c.reorg(&ReorgPlan { fork_height: 1, rollback: &rollback, apply: &[alt.commit()] })
        .unwrap();
    c.flush().unwrap();
    assert_eq!(r.tip().hash, alt.hash);

    // The new branch is indexed ...
    assert_eq!(hits(r.addr_history(&addr(2), None, 10).unwrap()).0, vec![(2, 1), (2, 0), (1, 0)]);
    // ... and A's rows from the replaced block are still there. They are
    // positions, not claims: the RPC layer reads the canonical body at h2 and
    // drops them, as it does for txindex.
    assert_eq!(hits(r.addr_history(&addr(1), None, 10).unwrap()).0, vec![(2, 1), (2, 0), (1, 1), (0, 0)]);
}

#[test]
fn an_unparseable_body_is_refused_when_indexing() {
    let _g = serial();
    let (_s, mut c, _r) = indexed_store("addr-garbage");
    let mut b = block(0, [0u8; 32], 5, &[coinbase(0, addr(1))]);
    b.body = vec![0xEE; 64];
    match c.extend(&[b.commit()]) {
        Err(StoreError::BadPlan(why)) => assert!(why.contains("indexed"), "{why}"),
        other => panic!("a body the index cannot read must fail the commit, got {other:?}"),
    }
}
