use crate::json::Json;
use crate::jsonrpc::{
    self, hash_param, opt_u64_param, param, param_count, str_param, u64_param, ErrorCode,
    Request, RpcError,
};
use crate::notes;
use crate::views::{
    Address20, BlockRecord, CheckpointLink, CheckpointSubmit, Direction, HeaderRecord,
    HistoryKind, HistoryLookup, Network, Node, NotesCursor, TxLocation, TxLookup, TxRecord,
    Verbosity,
};

// the whole RPC surface, and it's closed. dispatch() and unknown_method() both read this list.
pub const METHODS: &[&str] = &[
    "chain_getInfo",
    "chain_getHeaderByHeight",
    "chain_getHeaderByHash",
    "chain_getBlockByHeight",
    "chain_getBlockByHash",
    "account_get",
    "tx_sendRaw",
    "tx_get",
    "mempool_getInfo",
    "mempool_getBySender",
    "fee_suggest",
    "emission_audit",
    "checkpoint_getStatus",
    "checkpoint_submit",
    "author_getNotes",
    "account_getHistory",
    "net_getPeerInfo",
    "stratum_getSessions",
    "node_getBudgets",
];

pub const NOTES_DEFAULT_LIMIT: usize = 20;

pub const HISTORY_DEFAULT_LIMIT: usize = 50;

pub const HISTORY_MAX_LIMIT: usize = 200;

pub const NOTES_MAX_LIMIT: usize = 100;

pub fn dispatch(node: &Node, req: &Request) -> Result<Json, RpcError> {
    match req.method.as_str() {
        "chain_getInfo" => chain_get_info(node),
        "chain_getHeaderByHeight" => chain_get_header_by_height(node, &req.params),
        "chain_getHeaderByHash" => chain_get_header_by_hash(node, &req.params),
        "chain_getBlockByHeight" => chain_get_block_by_height(node, &req.params),
        "chain_getBlockByHash" => chain_get_block_by_hash(node, &req.params),
        "account_get" => account_get(node, &req.params),
        "tx_sendRaw" => tx_send_raw(node, &req.params),
        "tx_get" => tx_get(node, &req.params),
        "mempool_getInfo" => mempool_get_info(node),
        "mempool_getBySender" => mempool_get_by_sender(node, &req.params),
        "fee_suggest" => fee_suggest(node),
        "emission_audit" => emission_audit(node, &req.params),
        "checkpoint_getStatus" => checkpoint_get_status(node),
        "checkpoint_submit" => checkpoint_submit(node, &req.params),
        "author_getNotes" => author_get_notes(node, &req.params),
        "account_getHistory" => account_get_history(node, &req.params),
        "net_getPeerInfo" => net_get_peer_info(node),
        "stratum_getSessions" => stratum_get_sessions(node),
        "node_getBudgets" => node_get_budgets(node),
        unknown => Err(unknown_method(unknown)),
    }
}

// past here, no fuzzy matcher and no echo: edit_distance is O(len*method) and the name is
// attacker-controlled, so a megabyte name would be a free CPU sink.
const MAX_SUGGESTABLE_METHOD_LEN: usize = 64;

fn unknown_method(name: &str) -> RpcError {
    if name.len() > MAX_SUGGESTABLE_METHOD_LEN {
        let shown: String = name.chars().take(MAX_SUGGESTABLE_METHOD_LEN).collect();
        return RpcError::detail(
            ErrorCode::MethodNotFound,
            format!(
                "no method \"{shown}...\" ({} chars). The list is closed: {}",
                name.chars().count(),
                METHODS.join(", ")
            ),
        );
    }
    let mut best: Option<(usize, &str)> = None;
    for candidate in METHODS {
        let d = edit_distance(name, candidate);
        if best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, candidate));
        }
    }
    let detail = match best {
        Some((d, m)) if d <= 4 => format!("no method {name:?}; did you mean {m:?}?"),
        _ => format!(
            "no method {name:?}. The list is closed: {}",
            METHODS.join(", ")
        ),
    };
    RpcError::detail(ErrorCode::MethodNotFound, detail)
}

pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut best = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);

            // damerau: an adjacent swap is one edit, so getInof -> getInfo reads as one miss
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(prev2[j - 2] + 1);
            }
            cur[j] = best;
        }
        core::mem::swap(&mut prev2, &mut prev);
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn hex32(h: &[u8; 32]) -> Json {
    Json::str(plaine_consensus::hex::encode(h))
}

fn header_json(h: &HeaderRecord) -> Json {
    // decoded fields are best-effort; an unparseable header still returns its hash and height
    let decoded = plaine_consensus::codec::Header::decode(&h.raw).ok();
    let mut members = vec![
        ("hash".into(), hex32(&h.hash)),
        ("height".into(), Json::u64(h.height)),
        ("confirmations".into(), Json::u64(h.confirmations)),
        ("canonical".into(), Json::Bool(h.canonical)),
        ("chainwork".into(), hex32(&h.chainwork)),
        ("raw".into(), Json::str(plaine_consensus::hex::encode(&h.raw))),
    ];
    if let Some(d) = decoded {
        members.extend([
            ("version".into(), Json::u64(d.version as u64)),
            ("prevHash".into(), hex32(&d.prev_hash)),
            ("txRoot".into(), hex32(&d.tx_root)),
            ("extRoot".into(), hex32(&d.ext_root)),
            ("time".into(), Json::u64(d.time)),
            ("bits".into(), Json::u64(d.bits as u64)),
            ("authorNoteLen".into(), Json::u64(d.author_note_len as u64)),
            ("nonce".into(), Json::str(d.nonce.to_string())),
        ]);
    }
    Json::Obj(members)
}

fn block_json(b: &BlockRecord, verbosity: Verbosity) -> Json {
    let note = notes::render(&b.author_note);
    let mut members = vec![
        ("header".into(), header_json(&b.header)),
        ("txCount".into(), Json::u64(b.tx_count as u64)),
        ("sizeBytes".into(), Json::u64(b.size_bytes as u64)),
        ("authorNoteHex".into(), Json::str(note.hex)),
        ("authorNoteText".into(), Json::str(note.text)),
        ("authorNoteValidUtf8".into(), Json::Bool(note.valid_utf8)),
    ];
    match verbosity {
        Verbosity::RawHex => {
            if let Some(raw) = &b.raw {
                members.push(("raw".into(), Json::str(plaine_consensus::hex::encode(raw))));
            }
        }
        Verbosity::HeaderAndTxids => {
            members.push((
                "txids".into(),
                Json::Arr(b.txids.iter().map(hex32).collect()),
            ));
        }
        Verbosity::FullTxs => {
            members.push((
                "txids".into(),
                Json::Arr(b.txids.iter().map(hex32).collect()),
            ));
            members.push(("txs".into(), Json::Arr(b.txs.clone())));
        }
    }
    Json::Obj(members)
}

fn tx_json(t: &TxRecord) -> Json {
    let location = match t.location {
        TxLocation::Mempool => Json::Obj(vec![
            ("where".into(), Json::str("mempool")),
            ("confirmations".into(), Json::u64(0)),
        ]),
        TxLocation::Block { height, confirmations } => Json::Obj(vec![
            ("where".into(), Json::str("block")),
            ("height".into(), Json::u64(height)),
            ("confirmations".into(), Json::u64(confirmations)),
        ]),
    };
    Json::Obj(vec![
        ("txid".into(), hex32(&t.txid)),
        ("type".into(), Json::u64(t.type_byte as u64)),
        ("raw".into(), Json::str(plaine_consensus::hex::encode(&t.raw))),
        ("location".into(), location),
        ("decoded".into(), t.decoded.clone()),
    ])
}

fn address_param(node: &Node, params: &Json, index: usize, name: &str) -> Result<Address20, RpcError> {
    let s = str_param(params, index, name)?;
    let (hrp, data) = plaine_consensus::bech32m::decode_bytes(s).map_err(|e| {
        RpcError::detail(
            ErrorCode::InvalidParams,
            format!(

                "`{name}` is not a valid bech32m address: {e}. A Plaine address looks like \
                 plne1... and carries a checksum, so this is a typo, not a missing account."
            ),
        )
    })?;
    let expected = plaine_consensus::constants::ADDRESS_HRP;
    if hrp != expected {
        let net = node.chain.info().network;
        return Err(RpcError::detail(
            ErrorCode::InvalidParams,
            format!(
                "`{name}` has prefix {hrp:?} but this node is on {} and expects {expected:?}",
                net.as_str()
            ),
        ));
    }
    if data.len() != plaine_consensus::constants::ADDRESS_PAYLOAD_BYTES {
        return Err(RpcError::detail(
            ErrorCode::InvalidParams,
            format!("`{name}` decodes to {} bytes, expected 20", data.len()),
        ));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&data);
    Ok(out)
}

fn verbosity_param(params: &Json, index: usize) -> Result<Verbosity, RpcError> {
    match opt_u64_param(params, index, "verbosity")? {
        None => Ok(Verbosity::HeaderAndTxids),
        Some(v) => Verbosity::from_int(v).ok_or_else(|| {
            RpcError::detail(
                ErrorCode::InvalidParams,
                format!("verbosity must be 0 (raw hex), 1 (header + txids) or 2 (full txs), got {v}"),
            )
        }),
    }
}

fn arity(params: &Json, max: usize, method: &str) -> Result<(), RpcError> {
    if param_count(params) > max {
        return Err(RpcError::detail(
            ErrorCode::InvalidParams,
            format!("{method} takes at most {max} parameters, got {}", param_count(params)),
        ));
    }
    Ok(())
}

fn chain_get_info(node: &Node) -> Result<Json, RpcError> {
    let i = node.chain.info();
    let mut members = vec![
        ("network".into(), Json::str(i.network.as_str())),
        ("version".into(), Json::str(i.version)),
        ("height".into(), Json::u64(i.height)),
        ("tipHash".into(), hex32(&i.tip_hash)),
        ("chainwork".into(), hex32(&i.chainwork)),
        ("tipTime".into(), Json::u64(i.tip_time)),
        ("tipAgeSecs".into(), Json::u64(i.tip_age_secs)),
        ("sync".into(), Json::str(i.sync.as_str())),
        ("txindex".into(), Json::Bool(i.txindex)),
        ("pruned".into(), Json::Bool(i.pruned)),
        ("pruneHorizonHeight".into(), Json::u64(i.prune_horizon_height)),
        ("peers".into(), Json::u64(node.net.peers().len() as u64)),
    ];
    members.push((
        "bestKnownHeight".into(),
        match i.best_known_height {
            Some(h) => Json::u64(h),
            None => Json::Null,
        },
    ));
    if let Some(reason) = i.stall_reason {
        members.push(("stallReason".into(), Json::str(reason)));
    }
    Ok(Json::Obj(members))
}

fn chain_get_header_by_height(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "chain_getHeaderByHeight")?;
    let height = u64_param(params, 0, "height")?;
    node.chain
        .header_by_height(height)
        .map(|h| header_json(&h))
        .ok_or_else(|| not_found_height(node, height))
}

fn chain_get_header_by_hash(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "chain_getHeaderByHash")?;
    let hash = hash_param(params, 0, "hash")?;
    if let Some(h) = node.chain.header_by_hash(&hash) {
        return Ok(header_json(&h));
    }

    if let Some(why) = node.chain.invalid_reason(&hash) {
        return Err(RpcError::detail(
            ErrorCode::NotFound,
            format!(
                "no header with that hash: this node has refused it, reason {why:?}. The ban is \
                 durable and survives restart. A \"bad-checkpoint\" reason can be reversed by a \
                 later, higher anchor; the others cannot."
            ),
        ));
    }
    Err(RpcError::detail(ErrorCode::NotFound, "no header with that hash"))
}

fn chain_get_block_by_height(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 2, "chain_getBlockByHeight")?;
    let height = u64_param(params, 0, "height")?;
    let verbosity = verbosity_param(params, 1)?;
    match node.chain.block_by_height(height, verbosity) {
        Some(b) => Ok(block_json(&b, verbosity)),
        None => Err(pruned_or_missing(node, height)),
    }
}

fn chain_get_block_by_hash(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 2, "chain_getBlockByHash")?;
    let hash = hash_param(params, 0, "hash")?;
    let verbosity = verbosity_param(params, 1)?;
    match node.chain.block_by_hash(&hash, verbosity) {
        Some(b) => Ok(block_json(&b, verbosity)),
        None => Err(RpcError::detail(
            ErrorCode::NotFound,
            "no block with that hash, or its body is not stored here",
        )),
    }
}

fn not_found_height(node: &Node, height: u64) -> RpcError {
    let i = node.chain.info();
    if height > i.height {
        RpcError::detail(
            ErrorCode::NotFound,
            format!(
                "height {height} is above this node's tip of {} (node is {})",
                i.height,
                i.sync.as_str()
            ),
        )
    } else {
        RpcError::detail(ErrorCode::NotFound, format!("no header at height {height}"))
    }
}

fn pruned_or_missing(node: &Node, height: u64) -> RpcError {
    let i = node.chain.info();
    if height > i.height {
        return not_found_height(node, height);
    }
    if i.pruned && height < i.prune_horizon_height {
        return RpcError::detail(
            ErrorCode::FeatureDisabled,
            format!(
                "block {height} is below this node's pruning horizon of {} - the header is kept \
                 forever, the body is not. Run with `prune = false` for an archive node.",
                i.prune_horizon_height
            ),
        );
    }
    RpcError::detail(ErrorCode::NotFound, format!("no block at height {height}"))
}

fn account_get(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "account_get")?;
    let addr = address_param(node, params, 0, "address")?;
    let a = node.chain.account(&addr);
    Ok(Json::Obj(vec![
        ("balance".into(), Json::mile(a.balance)),
        ("nonce".into(), Json::u64(a.nonce)),
        ("pendingNonce".into(), Json::u64(a.pending_nonce)),
        ("immature".into(), Json::mile(a.immature)),
        ("spendable".into(), Json::mile(a.balance.saturating_sub(a.immature))),
    ]))
}

fn tx_send_raw(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "tx_sendRaw")?;
    let hex = str_param(params, 0, "raw")?;
    let hex = hex.strip_prefix("0x").unwrap_or(hex);

    // bound the hex by the consensus size limit before decoding it, so a huge string is
    // refused on a length check instead of allocating megabytes first. (two hex chars per byte.)
    let max_hex = plaine_consensus::constants::MAX_TX_BYTES * 2;
    if hex.len() > max_hex {
        return Err(RpcError::detail(
            ErrorCode::LimitExceeded,
            format!(
                "raw transaction is {} hex characters; the consensus limit MAX_TX_BYTES = {} \
                 allows at most {max_hex}",
                hex.len(),
                plaine_consensus::constants::MAX_TX_BYTES
            ),
        ));
    }
    let raw = plaine_consensus::hex::decode(hex).map_err(|e| {
        RpcError::detail(ErrorCode::InvalidParams, format!("`raw` is not valid hex: {e}"))
    })?;
    match node.mempool.submit(&raw) {
        Ok(txid) => Ok(hex32(&txid)),
        Err(e) => Err(RpcError::with_data(
            ErrorCode::TxRejected,
            e.human(),
            Json::str(e.tag()),
        )),
    }
}

fn tx_get(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 2, "tx_get")?;
    let txid = hash_param(params, 0, "txid")?;
    let via = match param(params, 1, "address") {
        None | Some(Json::Null) => None,
        Some(_) => Some(address_param(node, params, 1, "address")?),
    };
    let mut lookup = node.chain.tx(&txid);

    // Without a txid index, an address the transaction touches lets the address
    // index answer instead: how a wallet follows its own transactions.
    if let (TxLookup::NotIndexed { .. }, Some(addr)) = (&lookup, via) {
        match node.chain.tx_via_history(&txid, &addr) {
            Some(found @ (TxLookup::Found(_) | TxLookup::Pruned { .. })) => lookup = found,
            Some(TxLookup::Absent) => return Err(RpcError::detail(
                ErrorCode::NotFound,
                "no transaction with that id is in the mempool or among the confirmed \
                 transactions that touch that address",
            )),
            Some(TxLookup::NotIndexed { indexed_from }) => return Err(RpcError::detail(
                ErrorCode::FeatureDisabled,
                format!(
                    "not in the mempool, and not among the transactions that touch that \
                     address from height {} on, which is as far back as this node can see - \
                     it cannot say whether it exists below that. Resync with \
                     `addrindex = true`, or `txindex = true`, to cover the whole chain.",
                    indexed_from.unwrap_or(0)
                ),
            )),
            None => return Err(RpcError::detail(
                ErrorCode::FeatureDisabled,
                "not in the mempool or the recent blocks this node keeps at hand, and this \
                 node keeps neither a txid index nor an address index to look further. \
                 Start it with `addrindex = true` to find transactions by an address they \
                 touch, or `txindex = true` to find any by id; either then needs a resync \
                 to cover past blocks.",
            )),
        }
    }

    match lookup {
        TxLookup::Found(t) => Ok(tx_json(&t)),

        TxLookup::Pruned { height } => Err(RpcError::detail(
            ErrorCode::FeatureDisabled,
            format!(
                "the txid index places this transaction in block {height}, whose body this \
                 pruned node no longer stores. Its header is kept; an archive node \
                 (`prune = false`) can return the transaction."
            ),
        )),

        TxLookup::Absent => {
            Err(RpcError::detail(ErrorCode::NotFound, "no transaction with that id"))
        }
        TxLookup::NotIndexed { indexed_from: None } => Err(RpcError::detail(
            ErrorCode::FeatureDisabled,
            "not in the mempool, and this node has no txid index, so confirmed \
             transactions cannot be looked up by id. Start with `txindex = true` \
             - it must then resync to build the index. With `addrindex = true`, \
             passing an address the transaction touches as the second parameter \
             works too.",
        )),

        TxLookup::NotIndexed { indexed_from: Some(from) } => Err(RpcError::detail(
            ErrorCode::FeatureDisabled,
            format!(
                "not in the mempool, and this node's txid index begins at height {from}, so \
                 a transaction confirmed below that cannot be looked up by id - this node \
                 cannot say whether it exists. Resync from genesis with `txindex = true` \
                 to cover the whole chain."
            ),
        )),
    }
}

fn mempool_get_info(node: &Node) -> Result<Json, RpcError> {
    let m = node.mempool.info();
    Ok(Json::Obj(vec![
        ("txCount".into(), Json::u64(m.tx_count as u64)),
        ("bytes".into(), Json::u64(m.bytes as u64)),
        ("executable".into(), Json::u64(m.executable as u64)),
        ("queued".into(), Json::u64(m.queued as u64)),
        ("relayFeeMile".into(), Json::mile(m.relay_fee_mile)),
        ("maxTxs".into(), Json::u64(m.max_txs as u64)),

        (
            "consensusFeeFloorMile".into(),
            Json::mile(plaine_consensus::constants::FEE_FLOOR_MILE),
        ),
    ]))
}

fn mempool_get_by_sender(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "mempool_getBySender")?;
    let addr = address_param(node, params, 0, "address")?;
    let txs = node.mempool.by_sender(&addr);
    Ok(Json::Arr(txs.iter().map(tx_json).collect()))
}

fn fee_suggest(node: &Node) -> Result<Json, RpcError> {
    let f = node.mempool.fee_suggest();
    Ok(Json::Obj(vec![
        ("blocksSampled".into(), Json::u64(f.blocks_sampled)),
        ("p10Mile".into(), Json::mile(f.p10_mile)),
        ("p50Mile".into(), Json::mile(f.p50_mile)),
        ("p90Mile".into(), Json::mile(f.p90_mile)),
        ("relayFloorMile".into(), Json::mile(f.relay_floor_mile)),
    ]))
}

fn emission_audit(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "emission_audit")?;
    let tip = node.chain.info().height;
    let height = match opt_u64_param(params, 0, "height")? {
        Some(h) => h,
        None => tip,
    };

    // issued is a running total with no per-height record, so only the tip can be audited
    if height != tip {
        return Err(RpcError::detail(ErrorCode::InvalidParams, format!(
            "emission_audit can only be answered at the tip ({tip}); {height} was asked for. `issued` is a running total kept with no per-height record, so a past height has nothing to compare against."
        )));
    }
    let a = node
        .chain
        .emission_audit(height)
        .ok_or_else(|| not_found_height(node, height))?;

    let matches = a.issued_mile == a.expected_by_formula_mile;
    let difference = if a.issued_mile >= a.expected_by_formula_mile {
        format!("{}", a.issued_mile - a.expected_by_formula_mile)
    } else {
        format!("-{}", a.expected_by_formula_mile - a.issued_mile)
    };
    Ok(Json::Obj(vec![
        ("height".into(), Json::u64(a.height)),
        ("issuedMile".into(), Json::mile(a.issued_mile)),
        ("expectedByFormulaMile".into(), Json::mile(a.expected_by_formula_mile)),
        ("differenceMile".into(), Json::Str(difference)),

        (
            "maxSupplyMile".into(),
            match a.max_supply_mile {
                Some(m) => Json::mile(m),
                None => Json::Null,
            },
        ),
        ("subsidyAtHeightMile".into(), Json::mile(a.subsidy_at_height_mile)),
        ("matchesFormula".into(), Json::Bool(matches)),
        (
            "underMaxSupply".into(),
            Json::Bool(a.max_supply_mile.is_none_or(|m| a.issued_mile <= m)),
        ),
    ]))
}

fn checkpoint_get_status(node: &Node) -> Result<Json, RpcError> {
    let c = node.policy.checkpoint_status();

    let link = node.policy.checkpoint_link();
    let (last_anchor, enforced) = match &link {
        CheckpointLink::Live { last_anchor, enforced } => (*last_anchor, *enforced),
        CheckpointLink::Severed => (None, 0),
    };
    // enabled reflects reality, not config: a severed-ingest node reports no protection it lacks.
    let mut members = vec![
        ("enabled".into(), Json::Bool(c.enabled && link.is_live())),
        ("configured".into(), Json::Bool(c.enabled)),
        ("ingest".into(), Json::str(link.as_str())),
        ("keySource".into(), Json::str(c.key_source.as_str())),
        (
            "keyFingerprints".into(),
            Json::Arr(c.key_fingerprints.iter().map(|f| Json::str(f.clone())).collect()),
        ),
        ("threshold".into(), Json::u64(c.threshold as u64)),
        ("enforcedCount".into(), Json::u64(enforced as u64)),
        ("sunsetHeight".into(), Json::u64(c.sunset_height)),
        ("sunsetPassed".into(), Json::Bool(c.sunset_passed)),
    ];
    members.push((
        "blocksUntilSunset".into(),
        match c.blocks_until_sunset {
            Some(b) => Json::u64(b),
            None => Json::Null,
        },
    ));
    members.push((
        "lastAnchor".into(),
        match last_anchor {
            Some((h, hash)) => Json::Obj(vec![
                ("height".into(), Json::u64(h)),
                ("hash".into(), hex32(&hash)),
            ]),
            None => Json::Null,
        },
    ));

    members.push((
        "note".into(),
        Json::str(
            "Signed checkpoints can only reject reorgs; they never create, reorder or censor \
             blocks. The sunset height is compiled into the binary and configuration cannot \
             extend it.",
        ),
    ));

    if !c.enabled {
        members.push((
            "warning".into(),
            Json::str(
                "checkpoints.enabled = false: this node will accept deep reorgs that a \
                 checkpointed node rejects.",
            ),
        ));
    } else if !link.is_live() {
        members.push((
            "warning".into(),
            Json::str(
                "checkpoints are configured but this node cannot receive one: no signed \
                 checkpoint can reach the chain, so there is no recovery anchor and no \
                 enforced height, and reorg defence is the depth cap alone. The keys \
                 listed above are not in use. Configuration cannot fix this.",
            ),
        ));
    }
    Ok(Json::Obj(members))
}

fn checkpoint_submit(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 1, "checkpoint_submit")?;
    let hex = str_param(params, 0, "record")?;
    let hex = hex.strip_prefix("0x").unwrap_or(hex);

    let max_hex = plaine_consensus::checkpoint_record::MAX_BYTES * 2;
    if hex.len() > max_hex {
        return Err(RpcError::detail(
            ErrorCode::LimitExceeded,
            format!(
                "checkpoint record is {} hex characters; the longest legal record is {} bytes, \
                 i.e. {max_hex} characters",
                hex.len(),
                plaine_consensus::checkpoint_record::MAX_BYTES
            ),
        ));
    }
    let raw = plaine_consensus::hex::decode(hex).map_err(|e| {
        RpcError::detail(ErrorCode::InvalidParams, format!("`record` is not valid hex: {e}"))
    })?;

    let cp = plaine_consensus::checkpoint_record::decode(&raw).map_err(|e| {
        RpcError::with_data(
            ErrorCode::InvalidParams,
            format!("`record` is not a checkpoint record: {}", e.shape()),
            Json::str("malformed"),
        )
    })?;

    let out = node.policy.checkpoint_submit(&cp);
    let mut members = vec![
        ("result".into(), Json::str(out.tag())),
        ("anchorAdvanced".into(), Json::Bool(out.advanced())),
    ];
    match out {
        CheckpointSubmit::Advanced { height, enforced, enforcing } => {
            members.push(("height".into(), Json::u64(height)));
            members.push(("enforcedCount".into(), Json::u64(enforced as u64)));
            members.push(("enforcing".into(), Json::Bool(enforcing)));

            members.push((
                "detail".into(),
                Json::str(if enforcing {
                    "the anchor advanced and this node holds the named block, so the height is \
                     also enforced (layer 3)."
                } else {
                    "the anchor advanced and this node does not hold the named block at that \
                     height, so nothing was enforced. That is the stranded case the anchor \
                     exists for: a reorg onto a branch that contains it is now admissible past \
                     the depth cap, and every block of that branch is still validated."
                }),
            ));
            Ok(Json::Obj(members))
        }
        CheckpointSubmit::Unchanged => {
            members.push((
                "detail".into(),
                Json::str(
                    "verified, and the anchor did not move: this node already holds an anchor at \
                     this height or higher. The anchor is monotone in height.",
                ),
            ));
            Ok(Json::Obj(members))
        }

        CheckpointSubmit::GenesisImmutable => Err(RpcError::with_data(
            ErrorCode::CheckpointRejected,
            "height 0: genesis is pinned unconditionally and is never checkpointed",
            Json::str(out.tag()),
        )),
        CheckpointSubmit::Unverified => Err(RpcError::with_data(
            ErrorCode::CheckpointRejected,
            "this record does not verify against the authority keys and threshold this node is \
             configured with, or its height is at or past the compiled-in checkpoint sunset. \
             Compare `keyFingerprints` and `sunsetHeight` from checkpoint_getStatus against the \
             signer that produced it.",
            Json::str(out.tag()),
        )),
        CheckpointSubmit::NotConfigured => Err(RpcError::with_data(
            ErrorCode::FeatureDisabled,
            "checkpoints.enabled = false on this node, so it holds no authority keys and can \
             verify nothing. Set it and restart before delivering a checkpoint.",
            Json::str(out.tag()),
        )),
        CheckpointSubmit::Severed => Err(RpcError::with_data(
            ErrorCode::FeatureDisabled,
            "this build cannot deliver a checkpoint to a chain at all - the same condition \
             checkpoint_getStatus reports as ingest: severed. No configuration fixes it.",
            Json::str(out.tag()),
        )),

        CheckpointSubmit::Busy => Err(RpcError::with_data(
            ErrorCode::NotReady,
            "the chain did not answer in time - it is applying blocks, or the queue into it is \
             full. The record was not looked at, so this says nothing about whether it verifies. \
             Retry.",
            Json::str(out.tag()),
        )),
    }
}

fn history_cursor(s: &str) -> Option<(u64, u16)> {
    let (h, i) = s.split_once(':')?;
    Some((h.parse().ok()?, i.parse().ok()?))
}

fn account_get_history(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 3, "account_getHistory")?;
    let addr = address_param(node, params, 0, "address")?;
    let limit = match opt_u64_param(params, 1, "limit")? {
        None => HISTORY_DEFAULT_LIMIT,
        Some(0) => {
            return Err(RpcError::detail(
                ErrorCode::InvalidParams,
                format!("`limit` of 0 returns nothing; omit it for the default of {HISTORY_DEFAULT_LIMIT}"),
            ))
        }
        Some(n) if n as usize > HISTORY_MAX_LIMIT => {
            return Err(RpcError::detail(
                ErrorCode::InvalidParams,
                format!("`limit` must be 1..={HISTORY_MAX_LIMIT}, got {n}"),
            ))
        }
        Some(n) => n as usize,
    };
    let before = match param(params, 2, "cursor") {
        None | Some(Json::Null) => None,
        Some(Json::Str(s)) => Some(history_cursor(s).ok_or_else(|| {
            RpcError::detail(
                ErrorCode::InvalidParams,
                format!(
                    "`cursor` {s:?} is not a position: pass back the `nextCursor` string an \
                     earlier page returned, which looks like \"1234:0\""
                ),
            )
        })?),
        Some(_) => {
            return Err(RpcError::detail(
                ErrorCode::InvalidParams,
                "`cursor` must be the `nextCursor` string from an earlier page",
            ))
        }
    };

    match node.chain.account_history(&addr, before, limit) {
        HistoryLookup::NotIndexed => Err(RpcError::detail(
            ErrorCode::FeatureDisabled,
            "this node keeps no address index, so it cannot list an address's confirmed \
             transactions. Start it with `addrindex = true`; blocks connected from then on are \
             indexed, and a resync covers the whole chain. account_get still answers the \
             balance and nonce.",
        )),
        HistoryLookup::Page { indexed_from, entries, next_cursor, unavailable_below } => {
            let rows: Vec<Json> = entries
                .iter()
                .map(|e| {
                    Json::Obj(vec![
                        ("txid".into(), hex32(&e.txid)),
                        ("height".into(), Json::u64(e.height)),
                        ("index".into(), Json::u64(e.index as u64)),
                        ("time".into(), Json::u64(e.time)),
                        ("confirmations".into(), Json::u64(e.confirmations)),
                        (
                            "kind".into(),
                            Json::str(match e.kind {
                                HistoryKind::Coinbase => "coinbase",
                                HistoryKind::Transfer => "transfer",
                                HistoryKind::Announcement => "announcement",
                            }),
                        ),
                        (
                            "direction".into(),
                            Json::str(match e.direction {
                                Direction::In => "in",
                                Direction::Out => "out",
                                Direction::SelfTransfer => "self",
                            }),
                        ),
                        ("amountMile".into(), Json::mile(e.amount_mile)),
                        ("feeMile".into(), Json::mile(e.fee_mile)),
                        (
                            "counterparty".into(),
                            match e.counterparty {
                                Some(a) => Json::str(plaine_consensus::crypto::encode_address(&a)),
                                None => Json::Null,
                            },
                        ),
                    ])
                })
                .collect();
            let mut members = vec![
                ("address".into(), Json::str(plaine_consensus::crypto::encode_address(&addr))),
                ("indexedFrom".into(), Json::u64(indexed_from)),
                ("entries".into(), Json::Arr(rows)),
                (
                    "nextCursor".into(),
                    match next_cursor {
                        Some((h, i)) => Json::str(format!("{h}:{i}")),
                        None => Json::Null,
                    },
                ),
            ];
            let mut hints = Vec::new();
            if let Some(h) = unavailable_below {
                hints.push(format!(
                    "entries below height {h} were skipped: their block bodies are no longer \
                     stored on this pruned node, so the history there is incomplete"
                ));
            }
            if indexed_from > 0 && next_cursor.is_none() {
                hints.push(format!(
                    "the address index begins at height {indexed_from}; anything older is not \
                     listed. Resync with `addrindex = true` to cover the whole chain"
                ));
            }
            if !hints.is_empty() {
                members.push(("hint".into(), Json::str(hints.join(". "))));
            }
            Ok(Json::Obj(members))
        }
    }
}

fn author_get_notes(node: &Node, params: &Json) -> Result<Json, RpcError> {
    arity(params, 3, "author_getNotes")?;
    let from_height = opt_u64_param(params, 0, "fromHeight")?;
    let cursor_param = opt_u64_param(params, 2, "cursor")?;
    let cursor = match (from_height, cursor_param) {
        (Some(_), Some(_)) => {
            return Err(RpcError::detail(
                ErrorCode::InvalidParams,
                "give `fromHeight` or `cursor`, not both: `fromHeight` filters to announcements at \
                 or below a block height, `cursor` resumes an earlier page from the \
                 `nextCursor` it handed back. Combining them would have to guess which one you \
                 meant.",
            ))
        }
        (Some(h), None) => NotesCursor::AtOrBelowHeight(h),
        (None, Some(c)) => NotesCursor::Before(c),
        (None, None) => NotesCursor::Newest,
    };
    let limit = match opt_u64_param(params, 1, "limit")? {
        None => NOTES_DEFAULT_LIMIT,
        Some(0) => {
            return Err(RpcError::detail(
                ErrorCode::InvalidParams,
                "`limit` of 0 returns nothing; omit it for the default of 20",
            ))
        }
        Some(n) if n as usize > NOTES_MAX_LIMIT => {
            return Err(RpcError::detail(
                ErrorCode::InvalidParams,
                format!("`limit` must be 1..={NOTES_MAX_LIMIT}, got {n}"),
            ))
        }
        Some(n) => n as usize,
    };

    let page = node.chain.author_notes(cursor, limit);
    let key = node.policy.author_key_status();
    let info = node.chain.info();

    let notes_json: Vec<Json> = page
        .notes
        .iter()
        .map(|n| {
            let r = notes::render(&n.payload);
            Json::Obj(vec![
                ("height".into(), Json::u64(n.height)),
                ("seq".into(), Json::u64(n.seq)),
                ("txid".into(), hex32(&n.txid)),
                ("time".into(), Json::u64(n.time)),
                ("confirmations".into(), Json::u64(n.confirmations)),
                ("encoding".into(), Json::u64(n.encoding as u64)),
                ("hex".into(), Json::str(r.hex)),
                ("text".into(), Json::str(r.text)),
                ("validUtf8".into(), Json::Bool(r.valid_utf8)),
                ("sanitizedChars".into(), Json::u64(r.sanitized_chars as u64)),
            ])
        })
        .collect();

    let mut members = vec![
        ("notes".into(), Json::Arr(notes_json)),
        ("more".into(), Json::Bool(page.more)),
        ("total".into(), Json::u64(page.total)),
        (
            "nextCursor".into(),
            match page.next_seq {
                Some(s) => Json::u64(s),
                None => Json::Null,
            },
        ),
    ];

    if page.notes.is_empty() && page.total > 0 {
        let why = match cursor {
            NotesCursor::AtOrBelowHeight(h) => format!(
                "none at or below height {h}. `fromHeight` filters backwards from a height, \
                 because results are newest-first - it is not a starting point to read forwards \
                 from, so `fromHeight: 0` is always empty"
            ),
            NotesCursor::Before(c) => {
                format!("none older than cursor {c} - you have reached the end of the history")
            }

            NotesCursor::Newest => "none returned for an unfiltered call".to_string(),
        };
        members.push((
            "hint".into(),
            Json::str(format!(
                "this node holds {} announcement(s), but {why}. Call author_getNotes with no \
                 parameters to get the newest.",
                page.total
            )),
        ));
    }

    members.push((
        "authorKey".into(),
        Json::Obj(vec![
            ("enabled".into(), Json::Bool(key.enabled)),
            ("source".into(), Json::str(key.key_source.as_str())),
            ("fingerprint".into(), Json::str(key.fingerprint)),
            ("showInLog".into(), Json::Bool(key.show_in_log)),
        ]),
    ));

    members.push((
        "node".into(),
        Json::Obj(vec![
            ("height".into(), Json::u64(info.height)),
            ("sync".into(), Json::str(info.sync.as_str())),
            ("tipAgeSecs".into(), Json::u64(info.tip_age_secs)),
            ("peers".into(), Json::u64(node.net.peers().len() as u64)),
        ]),
    ));

    Ok(Json::Obj(members))
}

fn net_get_peer_info(node: &Node) -> Result<Json, RpcError> {
    let peers = node.net.peers();
    Ok(Json::Arr(
        peers
            .iter()
            .map(|p| {
                let ua = notes::render(p.user_agent.as_bytes());
                Json::Obj(vec![
                    ("id".into(), Json::u64(p.id)),
                    ("addr".into(), Json::str(p.addr.clone())),
                    ("direction".into(), Json::str(if p.outbound { "outbound" } else { "inbound" })),
                    ("connectedSecs".into(), Json::u64(p.connected_secs)),
                    ("bestHeight".into(), Json::u64(p.best_height)),
                    ("bytesRecv".into(), Json::u64(p.bytes_recv)),
                    ("bytesSent".into(), Json::u64(p.bytes_sent)),
                    ("misbehaviour".into(), Json::u64(p.misbehaviour as u64)),
                    ("userAgent".into(), Json::str(ua.text)),
                ])
            })
            .collect(),
    ))
}

// Read-only view of the live stratum sessions: who is connected, from where,
// and how they are doing. The worker and rig come off the wire from the miner,
// so they pass through notes::render before they can reach a log or a browser.
fn stratum_get_sessions(node: &Node) -> Result<Json, RpcError> {
    let sessions = node.stratum.sessions();
    Ok(Json::Arr(
        sessions
            .iter()
            .map(|s| {
                let worker = notes::render(s.worker.as_bytes()).text;
                let rig = notes::render(s.rig.as_bytes()).text;
                Json::Obj(vec![
                    ("worker".into(), Json::str(worker)),
                    ("address".into(), Json::str(s.address.clone())),
                    ("rig".into(), Json::str(rig)),
                    ("ip".into(), Json::str(s.ip.clone())),
                    ("authorized".into(), Json::Bool(s.authorized)),
                    ("connectedSecs".into(), Json::u64(s.connected_secs)),
                    ("lastShareSecs".into(), Json::Int(s.last_share_secs)),
                    ("acceptedShares".into(), Json::u64(s.accepted_shares)),
                    ("acceptedDifficulty".into(), Json::mile(s.accepted_difficulty)),
                    ("difficulty".into(), Json::u64(s.difficulty)),
                ])
            })
            .collect(),
    ))
}

fn node_get_budgets(node: &Node) -> Result<Json, RpcError> {
    let b = node.budgets.budgets();
    Ok(Json::Obj(vec![
        ("cpuPoolThreads".into(), Json::u64(b.cpu_pool_threads as u64)),
        ("powVerifiesTotal".into(), Json::u64(b.pow_verifies_total)),
        ("powCacheHitPct".into(), Json::u64(b.pow_cache_hit_pct as u64)),
        ("stratumSharesPerSec".into(), Json::u64(b.stratum_shares_per_sec)),
        (
            "stratumSharesCapPerSec".into(),
            // 83.3 shares/sec per pool thread, kept as *833/10 to avoid floating point.
            Json::u64((b.cpu_pool_threads as u64 * 833) / 10),
        ),
        ("validatorQueueBytes".into(), Json::u64(b.validator_queue_bytes)),
        ("validatorQueueBytesCap".into(), Json::u64(b.validator_queue_bytes_cap)),
        ("validatorQueueItems".into(), Json::u64(b.validator_queue_items as u64)),
        ("validatorQueueItemsCap".into(), Json::u64(b.validator_queue_items_cap as u64)),
        ("chainEventsTotal".into(), Json::u64(b.chain_events_total)),
        ("mempoolSources".into(), Json::u64(b.mempool_sources as u64)),
        ("mempoolSourcesCap".into(), Json::u64(b.mempool_sources_cap as u64)),
        ("rpcRejectedBusy".into(), Json::u64(b.rpc_rejected_busy)),

        (
            // unmeasured rss is null, never 0: a zero here would read as a healthy tiny process.
            "rssBytes".into(),
            b.rss_bytes.map(Json::u64).unwrap_or(Json::Null),
        ),
        ("rssBudgetBytes".into(), Json::u64(512 * 1024 * 1024)),
        ("bodyRejectsAlreadyHeld".into(), Json::u64(b.body_rejects_already_held)),
        ("bodyRejectsNotAdmissible".into(), Json::u64(b.body_rejects_not_admissible)),
    ]))
}

pub fn default_rpc_port(_network: Network) -> u16 {
    plaine_consensus::constants::PORT_RPC
}

pub use jsonrpc::MAX_BATCH;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::{AuthorKeyStatus, CheckpointStatus, KeySource, PolicyView};
    use crate::mock::MockNode;

    #[test]
    fn method_list_matches_the_doc() {
        let expected = [
            "chain_getInfo",
            "chain_getHeaderByHeight",
            "chain_getHeaderByHash",
            "chain_getBlockByHeight",
            "chain_getBlockByHash",
            "account_get",
            "tx_sendRaw",
            "tx_get",
            "mempool_getInfo",
            "mempool_getBySender",
            "fee_suggest",
            "emission_audit",
            "checkpoint_getStatus",
            "checkpoint_submit",
            "author_getNotes",
            "account_getHistory",
            "net_getPeerInfo",
            "stratum_getSessions",
            "node_getBudgets",
        ];

        for m in expected {
            assert!(METHODS.contains(&m), "{m} missing from METHODS");
        }
        for m in METHODS {
            assert!(expected.contains(m), "{m} is in METHODS and not in the documented list");
        }
        assert_eq!(METHODS.len(), expected.len(), "a name is listed twice");
    }

    #[test]
    fn every_method_reaches_a_handler() {
        let node = MockNode::synced().into_node();
        for m in METHODS {
            let req = Request { method: (*m).into(), params: Json::Arr(vec![]), id: None };
            let got = dispatch(&node, &req);

            if let Err(e) = got {
                assert_ne!(e.code, ErrorCode::MethodNotFound, "{m} is advertised and not routed");
            }
        }
    }

    #[test]
    fn nothing_outside_the_list_dispatches() {
        let node = MockNode::synced().into_node();
        for forbidden in [
            "getblocktemplate",
            "submitblock",
            "node_shutdown",
            "wallet_sign",
            "net_addPeer",
            "net_ban",
            "mining_start",
        ] {
            let req = Request { method: forbidden.into(), params: Json::Arr(vec![]), id: None };
            let e = dispatch(&node, &req).unwrap_err();
            assert_eq!(e.code, ErrorCode::MethodNotFound, "{forbidden} dispatched");
        }
    }

    #[test]
    fn a_typo_gets_a_suggestion() {
        let node = MockNode::synced().into_node();
        let req = Request { method: "chain_getinfo".into(), params: Json::Arr(vec![]), id: None };
        let e = dispatch(&node, &req).unwrap_err();
        assert!(e.detail.unwrap().contains("chain_getInfo"));
    }

    #[test]
    fn long_method_name_is_bounded() {
        let node = MockNode::synced().into_node();
        let huge = "a".repeat(1_000_000);
        let req = Request { method: huge, params: Json::Arr(vec![]), id: Some(Json::Int(1)) };
        let e = dispatch(&node, &req).unwrap_err();
        assert_eq!(e.code, ErrorCode::MethodNotFound);
        let detail = e.detail.unwrap();

        assert!(detail.len() < 1_000, "the giant name was echoed back: {} bytes", detail.len());
        assert!(detail.contains("1000000 chars"));
        assert!(detail.contains("The list is closed"));

        let at_bound = "z".repeat(MAX_SUGGESTABLE_METHOD_LEN);
        let e2 = dispatch(
            &node,
            &Request { method: at_bound, params: Json::Arr(vec![]), id: Some(Json::Int(1)) },
        )
        .unwrap_err();
        assert_eq!(e2.code, ErrorCode::MethodNotFound);
    }

    fn call(node: &Node, method: &str, params: Json) -> Result<Json, RpcError> {
        dispatch(node, &Request { method: method.into(), params, id: Some(Json::Int(1)) })
    }

    #[test]
    fn stratum_sessions_render_wire_shape() {
        let s = crate::views::StratumSession {
            worker: "plne1abc.rig1".into(),
            address: "plne1abc".into(),
            rig: "rig1".into(),
            ip: "1.2.3.4".into(),
            authorized: true,
            connected_secs: 90,
            last_share_secs: 5,
            accepted_shares: 3,
            accepted_difficulty: 12_345,
            difficulty: 1_000,
        };
        let node = MockNode::synced().with_session(s).into_node();
        let v = call(&node, "stratum_getSessions", Json::Arr(vec![])).unwrap();
        let arr = v.as_arr().expect("array");
        assert_eq!(arr.len(), 1);
        let o = &arr[0];
        assert_eq!(o.get("worker").unwrap().as_str(), Some("plne1abc.rig1"));
        assert_eq!(o.get("ip").unwrap().as_str(), Some("1.2.3.4"));
        assert_eq!(o.get("authorized").unwrap().as_bool(), Some(true));
        assert_eq!(o.get("connectedSecs").unwrap().as_int(), Some(90));
        assert_eq!(o.get("lastShareSecs").unwrap().as_int(), Some(5));
        // a u128 that a JS client would round rides out as a decimal string
        assert_eq!(o.get("acceptedDifficulty").unwrap().as_str(), Some("12345"));
        assert_eq!(o.get("difficulty").unwrap().as_int(), Some(1_000));
    }

    #[test]
    fn stratum_sessions_empty_when_none() {
        let node = MockNode::synced().into_node();
        let v = call(&node, "stratum_getSessions", Json::Arr(vec![])).unwrap();
        assert_eq!(v.as_arr().expect("array").len(), 0);
    }

    #[test]
    fn chain_get_info_reports_sync_word() {
        let node = MockNode::synced().into_node();
        let v = call(&node, "chain_getInfo", Json::Arr(vec![])).unwrap();
        assert_eq!(v.get("sync").unwrap().as_str(), Some("synced"));
        assert_eq!(v.get("network").unwrap().as_str(), Some("main"));
        assert!(v.get("chainwork").unwrap().as_str().unwrap().len() == 64);

        let stalled = MockNode::stalled().into_node();
        let v = call(&stalled, "chain_getInfo", Json::Arr(vec![])).unwrap();
        assert_eq!(v.get("sync").unwrap().as_str(), Some("stalled"));
        assert!(v.get("stallReason").is_some());
    }

    #[test]
    fn notes_readable_while_stalled() {
        let node = MockNode::stalled().with_notes().into_node();
        let v = call(&node, "author_getNotes", Json::Arr(vec![])).unwrap();
        let notes = v.get("notes").unwrap().as_arr().unwrap();
        assert!(!notes.is_empty(), "notes must be readable while stalled");

        let h0 = notes[0].get("height").unwrap().as_int().unwrap();
        let h1 = notes[1].get("height").unwrap().as_int().unwrap();
        assert!(h0 > h1, "newest first");

        let n = v.get("node").unwrap();
        assert_eq!(n.get("peers").unwrap().as_int(), Some(0));
        assert_eq!(n.get("sync").unwrap().as_str(), Some("stalled"));

        let k = v.get("authorKey").unwrap();
        assert_eq!(k.get("source").unwrap().as_str(), Some("embedded"));
        assert!(k.get("fingerprint").unwrap().as_str().unwrap().len() >= 8);
    }

    #[test]
    fn hostile_note_is_neutralised() {
        let node = MockNode::synced().with_hostile_note().into_node();
        let v = call(&node, "author_getNotes", Json::Arr(vec![])).unwrap();
        let n = &v.get("notes").unwrap().as_arr().unwrap()[0];
        let text = n.get("text").unwrap().as_str().unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\r'));
        assert!(n.get("sanitizedChars").unwrap().as_int().unwrap() > 0);

        assert!(n.get("hex").unwrap().as_str().unwrap().contains("1b"));
    }

    #[test]
    fn notes_paging_cursor_works() {
        let node = MockNode::synced().with_notes().into_node();
        let v = call(&node, "author_getNotes", Json::Arr(vec![Json::Null, Json::Int(1)])).unwrap();
        assert_eq!(v.get("notes").unwrap().as_arr().unwrap().len(), 1);
        assert_eq!(v.get("more").unwrap().as_bool(), Some(true));
        let next = v.get("nextCursor").unwrap().as_int().unwrap();

        let v2 = call(
            &node,
            "author_getNotes",
            Json::Arr(vec![Json::Null, Json::Int(10), Json::Int(next)]),
        )
        .unwrap();
        let first = &v2.get("notes").unwrap().as_arr().unwrap()[0];
        assert!(first.get("seq").unwrap().as_int().unwrap() < next);
    }

    #[test]
    fn paging_loses_nothing_at_shared_height() {
        let node = MockNode::synced().with_notes_sharing_a_height().into_node();

        let mut seen: Vec<i64> = Vec::new();
        let mut cursor = Json::Null;
        for _ in 0..10 {
            let v = call(
                &node,
                "author_getNotes",
                Json::Arr(vec![Json::Null, Json::Int(1), cursor.clone()]),
            )
            .unwrap();
            let notes = v.get("notes").unwrap().as_arr().unwrap().to_vec();
            for n in &notes {
                seen.push(n.get("seq").unwrap().as_int().unwrap());

                assert_eq!(n.get("height").unwrap().as_int().unwrap(), 500);
            }
            match v.get("nextCursor").unwrap().as_int() {
                Some(c) => cursor = Json::Int(c),
                None => break,
            }
        }

        let total = call(&node, "author_getNotes", Json::Arr(vec![])).unwrap();
        let total = total.get("total").unwrap().as_int().unwrap();
        assert_eq!(total, 3);
        assert_eq!(seen.len(), 3, "paging dropped a note: saw {seen:?}");

        assert_eq!(seen, vec![2, 1, 0]);
    }

    #[test]
    fn height_filter_and_cursor_are_exclusive() {
        let node = MockNode::synced().with_notes().into_node();
        let e = call(
            &node,
            "author_getNotes",
            Json::Arr(vec![Json::Int(1000), Json::Int(5), Json::Int(2)]),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        let d = e.detail.unwrap();
        assert!(d.contains("fromHeight") && d.contains("cursor"), "{d}");
    }

    #[test]
    fn end_of_history_is_explained() {
        let node = MockNode::synced().with_notes().into_node();
        let v = call(
            &node,
            "author_getNotes",
            Json::Arr(vec![Json::Null, Json::Int(10), Json::Int(0)]),
        )
        .unwrap();
        assert!(v.get("notes").unwrap().as_arr().unwrap().is_empty());
        assert_eq!(v.get("total").unwrap().as_int().unwrap(), 3);
        let hint = v.get("hint").expect("an empty page over a real history must explain itself");
        assert!(hint.as_str().unwrap().contains("end of the history"));
    }

    #[test]
    fn empty_page_explains_itself() {
        let node = MockNode::synced().with_notes().into_node();
        let v = call(&node, "author_getNotes", Json::Arr(vec![Json::Int(0)])).unwrap();
        assert!(v.get("notes").unwrap().as_arr().unwrap().is_empty());
        assert!(v.get("total").unwrap().as_int().unwrap() > 0);
        let hint = v.get("hint").expect("an empty page over a real history must explain itself");
        let hint = hint.as_str().unwrap();
        assert!(hint.contains("backwards"), "{hint}");
        assert!(hint.contains("no parameters"), "{hint}");

        let v2 = call(&node, "author_getNotes", Json::Arr(vec![])).unwrap();
        assert!(!v2.get("notes").unwrap().as_arr().unwrap().is_empty());

        assert!(v2.get("hint").is_none());
    }

    #[test]
    fn notes_limit_enforced() {
        let node = MockNode::synced().with_notes().into_node();
        let e = call(&node, "author_getNotes", Json::Arr(vec![Json::Null, Json::Int(1000)]))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert!(e.detail.unwrap().contains("1..=100"));
    }

    #[test]
    fn checkpoint_status_names_source_and_sunset() {
        let node = MockNode::synced().into_node();
        let v = call(&node, "checkpoint_getStatus", Json::Arr(vec![])).unwrap();
        assert_eq!(v.get("enabled").unwrap().as_bool(), Some(true));

        assert_eq!(v.get("configured").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("ingest").unwrap().as_str(), Some("live"));
        assert!(v.get("warning").is_none());
        assert_eq!(v.get("keySource").unwrap().as_str(), Some("embedded"));
        assert_eq!(v.get("threshold").unwrap().as_int(), Some(1));
        assert_eq!(
            v.get("sunsetHeight").unwrap().as_int(),
            Some(plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT as i64)
        );
        assert!(v.get("note").unwrap().as_str().unwrap().contains("only reject"));
    }

    #[test]
    fn disabled_checkpoint_warns() {
        let node = MockNode::synced().with_checkpoints_disabled().into_node();
        let v = call(&node, "checkpoint_getStatus", Json::Arr(vec![])).unwrap();
        assert_eq!(v.get("enabled").unwrap().as_bool(), Some(false));
        assert!(v.get("warning").unwrap().as_str().unwrap().contains("deep reorgs"));
    }

    struct LiteralPolicy;

    impl PolicyView for LiteralPolicy {
        fn checkpoint_status(&self) -> CheckpointStatus {
            CheckpointStatus {
                enabled: true,
                key_source: KeySource::Embedded,
                key_fingerprints: vec!["0741b159".into()],
                threshold: 1,
                last_anchor: Some((99_999, [9u8; 32])),
                enforced_count: 7,
                sunset_height: plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT,
                blocks_until_sunset: Some(1),
                sunset_passed: false,
            }
        }
        fn author_key_status(&self) -> AuthorKeyStatus {
            AuthorKeyStatus {
                enabled: true,
                key_source: KeySource::Embedded,
                fingerprint: "4d1e77b0".into(),
                show_in_log: true,
            }
        }

        fn checkpoint_link(&self) -> CheckpointLink {
            CheckpointLink::Severed
        }

        fn checkpoint_submit(
            &self,
            _cp: &plaine_consensus::rules::SignedCheckpoint,
        ) -> CheckpointSubmit {
            CheckpointSubmit::Severed
        }
    }

    #[test]
    fn severed_seam_reports_no_protection() {
        let me = std::sync::Arc::new(MockNode::synced());
        let node = Node {
            chain: me.clone(),
            mempool: me.clone(),
            net: me.clone(),
            stratum: me.clone(),
            policy: std::sync::Arc::new(LiteralPolicy),
            budgets: me,
        };
        let v = call(&node, "checkpoint_getStatus", Json::Arr(vec![])).unwrap();

        assert_eq!(
            v.get("enabled").unwrap().as_bool(),
            Some(false),
            "an embedder that cannot receive a checkpoint must never report protection"
        );
        assert_eq!(v.get("configured").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("ingest").unwrap().as_str(), Some("severed"));

        assert!(matches!(v.get("lastAnchor").unwrap(), Json::Null));
        assert_eq!(v.get("enforcedCount").unwrap().as_int(), Some(0));
        let w = v.get("warning").unwrap().as_str().unwrap().to_string();
        assert!(w.contains("cannot receive"), "warning was: {w}");
        assert!(w.contains("depth cap alone"), "warning was: {w}");
    }

    fn cp_hex(height: u64) -> String {
        let cp = plaine_consensus::rules::SignedCheckpoint {
            height,
            hash: [0x5A; 32],
            sigs: vec![plaine_consensus::rules::CheckpointSig {
                pubkey: [0x11; 32],
                sig: [0x22; 64],
            }],
        };
        plaine_consensus::hex::encode(&plaine_consensus::checkpoint_record::encode(&cp))
    }

    fn submit(node: &Node, hex: &str) -> Result<Json, RpcError> {
        call(node, "checkpoint_submit", Json::Arr(vec![Json::str(hex.to_string())]))
    }

    #[test]
    fn advancing_checkpoint_reports_height() {
        let node = MockNode::synced().into_node();
        let v = submit(&node, &cp_hex(12_340)).expect("accepted");
        assert_eq!(v.get("result").unwrap().as_str(), Some("advanced"));
        assert_eq!(v.get("anchorAdvanced").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("height").unwrap().as_int(), Some(12_340));
        assert_eq!(v.get("enforcing").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn stranded_advance_is_not_an_error() {
        let node = MockNode::synced()
            .with_checkpoint_submit(CheckpointSubmit::Advanced {
                height: 900,
                enforced: 0,
                enforcing: false,
            })
            .into_node();
        let v = submit(&node, &cp_hex(900)).expect("this is a success");
        assert_eq!(v.get("anchorAdvanced").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("enforcing").unwrap().as_bool(), Some(false));
        let d = v.get("detail").unwrap().as_str().unwrap().to_string();
        assert!(d.contains("does not hold"), "detail was: {d}");
        assert!(d.contains("stranded"), "detail was: {d}");
    }

    #[test]
    fn replayed_checkpoint_claims_no_advance() {
        let node = MockNode::synced()
            .with_checkpoint_submit(CheckpointSubmit::Unchanged)
            .into_node();
        let v = submit(&node, &cp_hex(12_000)).expect("not an error");
        assert_eq!(v.get("result").unwrap().as_str(), Some("unchanged"));
        assert_eq!(v.get("anchorAdvanced").unwrap().as_bool(), Some(false));
        assert!(v.get("height").is_none());
    }

    #[test]
    fn every_refusal_fails_the_call() {
        for (out, code) in [
            (CheckpointSubmit::Unverified, ErrorCode::CheckpointRejected),
            (CheckpointSubmit::GenesisImmutable, ErrorCode::CheckpointRejected),
            (CheckpointSubmit::NotConfigured, ErrorCode::FeatureDisabled),
            (CheckpointSubmit::Severed, ErrorCode::FeatureDisabled),
        ] {
            let node = MockNode::synced().with_checkpoint_submit(out).into_node();
            let e = submit(&node, &cp_hex(500)).unwrap_err();
            assert_eq!(e.code, code, "{out:?}");
            assert_eq!(
                e.data.as_ref().and_then(|d| d.as_str()),
                Some(out.tag()),
                "the machine-readable tag must reach the client"
            );
        }
    }

    #[test]
    fn unverified_refusal_names_no_check() {
        let node = MockNode::synced()
            .with_checkpoint_submit(CheckpointSubmit::Unverified)
            .into_node();
        let e = submit(&node, &cp_hex(500)).unwrap_err();
        let msg = format!(
            "{} {}",
            e.detail.as_deref().unwrap_or(""),
            e.data.as_ref().and_then(|d| d.as_str()).unwrap_or("")
        );
        for leak in ["threshold not met", "zero hash", "sunset height reached", "too many signatures"] {
            assert!(!msg.contains(leak), "the refusal named the failing check: {msg}");
        }

        assert!(e.detail.as_deref().unwrap_or("").contains("keyFingerprints"));
    }

    #[test]
    fn checkpoint_submit_bounds_hex() {
        let node = MockNode::synced().into_node();
        let huge = "ab".repeat(plaine_consensus::checkpoint_record::MAX_BYTES + 10);
        let e = submit(&node, &huge).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);

        let at_cap = "ab".repeat(plaine_consensus::checkpoint_record::MAX_BYTES);
        let e = submit(&node, &at_cap).unwrap_err();
        assert_ne!(e.code, ErrorCode::LimitExceeded);
    }

    #[test]
    fn malformed_record_refused_by_shape() {
        let node = MockNode::synced().into_node();

        assert_eq!(submit(&node, "zz").unwrap_err().code, ErrorCode::InvalidParams);

        let good = cp_hex(77);
        for (mangle, want) in [
            (good[..good.len() - 4].to_string(), "truncated"),
            (format!("{good}00"), "trailing bytes after the last signature"),
            (format!("02{}", &good[2..]), "unknown record version"),
        ] {
            let e = submit(&node, &mangle).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams, "{want}");
            let d = e.detail.as_deref().unwrap_or("").to_string();
            assert!(d.contains(want), "{d}");
            assert_eq!(e.data.as_ref().and_then(|d| d.as_str()), Some("malformed"));
        }
    }

    #[test]
    fn hex_may_carry_0x_prefix() {
        let node = MockNode::synced().into_node();
        let v = submit(&node, &format!("0x{}", cp_hex(12_340))).expect("accepted");
        assert_eq!(v.get("result").unwrap().as_str(), Some("advanced"));
    }

    #[test]
    fn checkpoint_submit_takes_exactly_one_parameter() {
        let node = MockNode::synced().into_node();
        assert_eq!(
            call(&node, "checkpoint_submit", Json::Arr(vec![])).unwrap_err().code,
            ErrorCode::InvalidParams
        );
        assert_eq!(
            call(
                &node,
                "checkpoint_submit",
                Json::Arr(vec![Json::str(cp_hex(1)), Json::str("extra".to_string())])
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn severed_ingest_reported_despite_config() {
        let node = MockNode::synced().with_checkpoint_ingest_severed().into_node();
        let v = call(&node, "checkpoint_getStatus", Json::Arr(vec![])).unwrap();
        assert_eq!(v.get("enabled").unwrap().as_bool(), Some(false));
        assert_eq!(v.get("configured").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("ingest").unwrap().as_str(), Some("severed"));

        assert_eq!(v.get("keyFingerprints").unwrap().as_arr().unwrap().len(), 1);
        assert!(v.get("warning").unwrap().as_str().unwrap().contains("not in use"));
    }

    #[test]
    fn anchor_and_count_come_from_seam() {
        let sunset = plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT;
        let lying = CheckpointStatus {
            enabled: true,
            key_source: KeySource::Config,
            key_fingerprints: vec!["deadbeef".into()],
            threshold: 1,
            last_anchor: Some((777_777, [0xEE; 32])),
            enforced_count: 999,
            sunset_height: sunset,
            blocks_until_sunset: Some(10),
            sunset_passed: false,
        };
        let author = AuthorKeyStatus {
            enabled: true,
            key_source: KeySource::Embedded,
            fingerprint: "4d1e77b0".into(),
            show_in_log: true,
        };
        let node = MockNode::synced().with_key_status(lying, author).into_node();
        let v = call(&node, "checkpoint_getStatus", Json::Arr(vec![])).unwrap();

        assert_eq!(v.get("enforcedCount").unwrap().as_int(), Some(3));
        let a = v.get("lastAnchor").unwrap();
        assert_eq!(a.get("height").unwrap().as_int(), Some(12_300));
        assert!(a.get("hash").unwrap().as_str().unwrap().starts_with("0707"));
    }

    #[test]
    fn amounts_are_strings_not_numbers() {
        let node = MockNode::synced().into_node();
        let addr = MockNode::sample_address();
        let v = call(&node, "account_get", Json::Arr(vec![Json::str(addr)])).unwrap();
        assert!(matches!(v.get("balance").unwrap(), Json::Str(_)));
        assert!(matches!(v.get("immature").unwrap(), Json::Str(_)));
        assert!(matches!(v.get("nonce").unwrap(), Json::Int(_)));
    }

    #[test]
    fn bad_address_never_reaches_copy_from_slice() {
        let node = MockNode::synced().into_node();

        let foreign = plaine_consensus::bech32m::encode_bytes("abcd", &[0x11u8; 20])
            .expect("20 bytes always encode");
        let e = call(&node, "account_get", Json::Arr(vec![Json::str(&foreign)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        let detail = e.detail.expect("detail");
        assert!(detail.contains("abcd"), "{detail}");
        assert!(detail.contains(plaine_consensus::constants::ADDRESS_HRP), "{detail}");

        for len in [0usize, 19, 21, 32] {
            let s = plaine_consensus::bech32m::encode_bytes(
                plaine_consensus::constants::ADDRESS_HRP,
                &vec![0x11u8; len],
            )
            .expect("encodes");
            let e = call(&node, "account_get", Json::Arr(vec![Json::str(&s)]))
                .expect_err(&format!("a {len}-byte payload was accepted as an address"));
            assert_eq!(e.code, ErrorCode::InvalidParams, "{len} bytes");
            assert!(e.detail.expect("detail").contains("expected 20"), "{len} bytes");
        }

        assert!(call(
            &node,
            "account_get",
            Json::Arr(vec![Json::str(MockNode::sample_address())])
        )
        .is_ok());
    }

    #[test]
    fn bad_address_reads_as_typo() {
        let node = MockNode::synced().into_node();
        let e = call(&node, "account_get", Json::Arr(vec![Json::str("plne1qqqqqqqqbad")]))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert!(e.detail.unwrap().contains("checksum"));
    }

    #[test]
    fn tx_send_raw_bounds_hex() {
        let node = MockNode::synced().into_node();
        let huge = "ab".repeat(plaine_consensus::constants::MAX_TX_BYTES + 10);
        let e = call(&node, "tx_sendRaw", Json::Arr(vec![Json::str(huge)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);
    }

    #[test]
    fn rejected_tx_carries_tag() {
        let node = MockNode::synced().into_node();
        let e = call(&node, "tx_sendRaw", Json::Arr(vec![Json::str("00")])).unwrap_err();
        assert_eq!(e.code, ErrorCode::TxRejected);
        assert_eq!(e.data, Some(Json::str("malformed")));
    }

    fn history_entry(height: u64, index: u16, kind: HistoryKind, direction: Direction) -> crate::views::HistoryEntry {
        crate::views::HistoryEntry {
            txid: [height as u8; 32],
            height,
            index,
            time: 1_700_000_000 + height * 60,
            confirmations: 12_345 - height + 1,
            kind,
            direction,
            amount_mile: 200_000 + index as u128,
            fee_mile: if kind == HistoryKind::Coinbase { 0 } else { 1_000 },
            counterparty: if kind == HistoryKind::Transfer { Some([0x22; 20]) } else { None },
        }
    }

    fn history_address() -> Json {
        Json::str(plaine_consensus::crypto::encode_address(&[0x11; 20]))
    }

    #[test]
    fn history_without_addrindex_explains() {
        let node = MockNode::synced().into_node();
        let e = call(&node, "account_getHistory", Json::Arr(vec![history_address()])).unwrap_err();
        assert_eq!(e.code, ErrorCode::FeatureDisabled);
        let d = e.detail.unwrap();
        assert!(d.contains("addrindex = true"), "must name the switch: {d}");
        assert!(d.contains("account_get"), "must point at what still works: {d}");
    }

    #[test]
    fn history_renders_every_field() {
        let node = MockNode::synced()
            .with_history(0, vec![
                history_entry(9, 1, HistoryKind::Transfer, Direction::In),
                history_entry(9, 0, HistoryKind::Coinbase, Direction::In),
            ])
            .into_node();
        let v = call(&node, "account_getHistory", Json::Arr(vec![history_address()])).unwrap();
        let text = v.to_string();
        assert!(text.contains(r#""kind":"transfer""#), "{text}");
        assert!(text.contains(r#""kind":"coinbase""#), "{text}");
        assert!(text.contains(r#""direction":"in""#), "{text}");
        assert!(text.contains(r#""amountMile":"200001""#), "amounts are strings: {text}");
        assert!(text.contains(r#""feeMile":"0""#), "a coinbase pays no fee: {text}");
        let peer = plaine_consensus::crypto::encode_address(&[0x22; 20]);
        assert!(text.contains(&format!(r#""counterparty":"{peer}""#)), "{text}");
        assert!(text.contains(r#""counterparty":null"#), "a coinbase has no counterparty: {text}");
        assert!(text.contains(r#""nextCursor":null"#), "{text}");
        assert!(!text.contains("hint"), "a full history from genesis needs no hint: {text}");
        let first = text.find(r#""index":1"#).expect("index 1 present");
        let second = text.find(r#""index":0"#).expect("index 0 present");
        assert!(first < second, "newest first: (9,1) before (9,0): {text}");
    }

    #[test]
    fn history_pages_with_the_cursor_it_hands_out() {
        let node = MockNode::synced()
            .with_history(0, vec![
                history_entry(3, 0, HistoryKind::Coinbase, Direction::In),
                history_entry(5, 2, HistoryKind::Transfer, Direction::Out),
                history_entry(7, 0, HistoryKind::Coinbase, Direction::In),
            ])
            .into_node();
        let p1 = call(&node, "account_getHistory", Json::Arr(vec![history_address(), Json::u64(2)]))
            .unwrap()
            .to_string();
        assert!(p1.contains(r#""height":7"#) && p1.contains(r#""height":5"#), "{p1}");
        assert!(!p1.contains(r#""height":3"#), "{p1}");
        assert!(p1.contains(r#""nextCursor":"5:2""#), "{p1}");

        let p2 = call(
            &node,
            "account_getHistory",
            Json::Arr(vec![history_address(), Json::u64(2), Json::str("5:2")]),
        )
        .unwrap()
        .to_string();
        assert!(p2.contains(r#""height":3"#), "{p2}");
        assert!(!p2.contains(r#""height":5"#), "the cursor itself is not repeated: {p2}");
        assert!(p2.contains(r#""nextCursor":null"#), "{p2}");
    }

    #[test]
    fn history_says_where_the_index_begins() {
        let node = MockNode::synced()
            .with_history(4_000, vec![history_entry(4_100, 0, HistoryKind::Coinbase, Direction::In)])
            .into_node();
        let v = call(&node, "account_getHistory", Json::Arr(vec![history_address()])).unwrap().to_string();
        assert!(v.contains(r#""indexedFrom":4000"#), "{v}");
        assert!(v.contains("begins at height 4000"), "the last page must say older is missing: {v}");
    }

    #[test]
    fn history_refuses_bad_parameters_precisely() {
        let node = MockNode::synced().with_history(0, Vec::new()).into_node();
        for (params, needle) in [
            (vec![Json::str("plne1notanaddress")], "bech32m"),
            (vec![history_address(), Json::u64(0)], "limit"),
            (vec![history_address(), Json::u64(201)], "1..=200"),
            (vec![history_address(), Json::Null, Json::str("abc")], "nextCursor"),
            (vec![history_address(), Json::Null, Json::u64(5)], "nextCursor"),
        ] {
            let e = call(&node, "account_getHistory", Json::Arr(params.clone())).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams, "{params:?}");
            let d = e.detail.unwrap_or_default();
            assert!(d.contains(needle), "{params:?}: {d}");
        }
    }

    #[test]
    fn tx_get_without_txindex_explains() {
        let node = MockNode::synced().into_node();
        let e = call(
            &node,
            "tx_get",
            Json::Arr(vec![Json::str(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )]),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::FeatureDisabled);
        assert!(e.detail.unwrap().contains("txindex"));
    }

    #[test]
    fn tx_get_with_txindex_denies_precisely() {
        let node = MockNode::synced().with_txindex().into_node();
        let e = call(
            &node,
            "tx_get",
            Json::Arr(vec![Json::str(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )]),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let detail = e.detail.unwrap();
        assert!(detail.contains("no transaction with that id"), "{detail}");

        assert!(!detail.contains("txindex"), "{detail}");
    }

    #[test]
    fn tx_get_in_a_pruned_block_blames_pruning_not_the_index() {
        let node = MockNode::synced().with_tx_in_pruned_block(812).into_node();
        let e = call(&node, "tx_get", Json::Arr(vec![txid_param(7)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::FeatureDisabled);
        let d = e.detail.unwrap();
        assert!(d.contains("block 812") && d.contains("pruned"), "{d}");
        assert!(!d.contains("index begins"), "the index is complete; pruning is the cause: {d}");
    }

    fn txid_param(byte: u8) -> Json {
        Json::str(plaine_consensus::hex::encode(&[byte; 32]))
    }

    #[test]
    fn tx_get_finds_a_confirmed_tx_through_the_address_index() {
        let node = MockNode::synced()
            .with_history(0, vec![history_entry(7, 1, HistoryKind::Transfer, Direction::Out)])
            .into_node();
        let v = call(&node, "tx_get", Json::Arr(vec![txid_param(7), history_address()])).unwrap();
        let text = v.to_string();
        assert!(text.contains(r#""where":"block""#), "{text}");
        assert!(text.contains(r#""height":7"#), "{text}");
        assert!(text.contains(&"07".repeat(32)), "{text}");
    }

    #[test]
    fn tx_get_through_the_address_index_says_what_it_searched() {
        let full = MockNode::synced()
            .with_history(0, vec![history_entry(7, 1, HistoryKind::Transfer, Direction::Out)])
            .into_node();
        let e = call(&full, "tx_get", Json::Arr(vec![txid_param(8), history_address()])).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound, "an index from genesis can say no");
        assert!(e.detail.unwrap().contains("touch that address"));

        let late = MockNode::synced()
            .with_history(500, vec![history_entry(700, 0, HistoryKind::Transfer, Direction::In)])
            .into_node();
        let e = call(&late, "tx_get", Json::Arr(vec![txid_param(8), history_address()])).unwrap_err();
        assert_eq!(e.code, ErrorCode::FeatureDisabled, "an index from 500 cannot say no");
        let d = e.detail.unwrap();
        assert!(d.contains("from height 500"), "{d}");
    }

    #[test]
    fn tx_get_with_an_address_but_no_index_names_both_switches() {
        let node = MockNode::synced().into_node();
        let e = call(&node, "tx_get", Json::Arr(vec![txid_param(7), history_address()])).unwrap_err();
        assert_eq!(e.code, ErrorCode::FeatureDisabled);
        let d = e.detail.unwrap();
        assert!(d.contains("addrindex = true") && d.contains("txindex = true"), "{d}");
    }

    #[test]
    fn tx_get_prefers_the_txid_index_and_checks_the_address() {
        // With txindex the answer is the txid index's; the address is still
        // validated, so a typo is reported rather than silently ignored.
        let node = MockNode::synced().with_txindex().into_node();
        let e = call(&node, "tx_get", Json::Arr(vec![txid_param(7), history_address()])).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = call(&node, "tx_get", Json::Arr(vec![txid_param(7), Json::str("plne1nope")])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        let e = call(&node, "tx_get", Json::Arr(vec![txid_param(7), Json::Null, Json::Null])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams, "at most two parameters");
    }

    #[test]
    fn pruned_body_is_feature_disabled() {
        let node = MockNode::synced().pruned_at(1000).into_node();
        let e = call(&node, "chain_getBlockByHeight", Json::Arr(vec![Json::Int(5)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::FeatureDisabled);
        assert!(e.detail.unwrap().contains("pruning horizon"));
    }

    #[test]
    fn emission_audit_compares_for_caller() {
        let node = MockNode::synced().into_node();
        let tip = node.chain.info().height;

        let v = call(&node, "emission_audit", Json::Arr(vec![Json::u64(tip)])).unwrap();
        assert_eq!(v.get("matchesFormula").unwrap().as_bool(), Some(true));

        assert_eq!(v.get("underMaxSupply").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("maxSupplyMile"), Some(&Json::Null));

        let d = call(&node, "emission_audit", Json::Arr(vec![Json::Null])).unwrap();
        assert_eq!(d.get("height").unwrap().as_int(), Some(tip as i64));
    }

    #[test]
    fn emission_audit_refuses_non_tip() {
        let node = MockNode::synced().into_node();
        let tip = node.chain.info().height;
        assert!(tip > 0, "tip must be above zero");
        let e = call(&node, "emission_audit", Json::Arr(vec![Json::Int(100)])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        let d = e.detail.unwrap();
        assert!(d.contains("only be answered at the tip"), "{d}");

        assert!(d.contains(&tip.to_string()), "{d}");
        assert!(d.contains("100"), "{d}");
    }

    #[test]
    fn hostile_peer_agent_is_neutralised() {
        let node = MockNode::synced().with_hostile_peer().into_node();
        let v = call(&node, "net_getPeerInfo", Json::Arr(vec![])).unwrap();
        let ua = v.as_arr().unwrap()[0].get("userAgent").unwrap().as_str().unwrap();
        assert!(!ua.contains('\u{1b}'));
        assert!(!ua.contains('\n'));
    }

    #[test]
    fn budgets_omit_unbacked_names() {
        let node = MockNode::synced().into_node();
        let v = call(&node, "node_getBudgets", Json::Arr(vec![])).unwrap();

        for gone in [
            "rpcInFlight",
            "rpcInFlightCap",
            "unsolicitedHeaderPct",
            "unsolicitedHeaderCapPct",
            "committerQueue",
            "committerQueueCap",
            "powVerifiesPerSec",
            "validatorQueue",
            "validatorQueueCap",
        ] {
            assert!(
                v.get(gone).is_none(),
                "{gone} is a name that lied about its value; it must not come back"
            );
        }

        assert!(v.get("validatorQueueBytes").is_some());
        assert!(v.get("validatorQueueItems").is_some());
        let byte_cap = v.get("validatorQueueBytesCap").unwrap().as_int().unwrap();
        let item_cap = v.get("validatorQueueItemsCap").unwrap().as_int().unwrap();
        assert!(
            byte_cap > item_cap * 1000,
            "byte and item caps are different units: bytes {byte_cap}, items {item_cap}"
        );

        assert_eq!(
            v.get("mempoolSourcesCap").unwrap().as_int(),
            Some(node.budgets.budgets().mempool_sources_cap as i64)
        );

        assert!(v.get("chainEventsTotal").is_some());
        assert!(v.get("powVerifiesTotal").is_some());
        assert!(v.get("chainEventsTotalCap").is_none());

        assert!(v.get("stratumSharesPerSec").is_some());
        assert!(v.get("stratumSharesCapPerSec").is_some());
    }

    struct UnmeasuredRss;

    impl crate::views::BudgetView for UnmeasuredRss {
        fn budgets(&self) -> crate::views::Budgets {
            crate::views::Budgets { rss_bytes: None, ..Default::default() }
        }
    }

    #[test]
    fn unmeasured_rss_is_null() {
        let node = MockNode::synced().into_node();
        let v = call(&node, "node_getBudgets", Json::Arr(vec![])).unwrap();
        let measured = node.budgets.budgets().rss_bytes.expect("the mock measures it");
        assert!(measured > 0);
        assert_eq!(v.get("rssBytes").unwrap().as_int(), Some(measured as i64));

        let mut dark = MockNode::synced().into_node();
        dark.budgets = std::sync::Arc::new(UnmeasuredRss);
        let v = call(&dark, "node_getBudgets", Json::Arr(vec![])).unwrap();
        assert_eq!(*v.get("rssBytes").unwrap(), Json::Null, "{v:?}");
        assert_ne!(v.get("rssBytes").unwrap().as_int(), Some(0), "{v:?}");

        assert_eq!(v.get("rssBudgetBytes").unwrap().as_int(), Some(512 * 1024 * 1024));
    }

    #[test]
    fn extra_params_are_an_error() {
        let node = MockNode::synced().into_node();
        let e = call(
            &node,
            "chain_getHeaderByHeight",
            Json::Arr(vec![Json::Int(1), Json::Int(2), Json::Int(3)]),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn verbosity_three_refused() {
        let node = MockNode::synced().into_node();
        let e = call(&node, "chain_getBlockByHeight", Json::Arr(vec![Json::Int(1), Json::Int(3)]))
            .unwrap_err();
        assert!(e.detail.unwrap().contains("verbosity must be 0"));
    }
}
