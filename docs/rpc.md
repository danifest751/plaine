# Node RPC reference

Every method `plaine-noded` answers, with its parameters, result fields, errors and an
example. It describes what the node emits, checked against the code and a running node;
where the node falls short of what a field suggests, the entry says so. SPEC §14 is the
design summary; where the two differ, this file follows the code.

`lib/rpc/tests/reference_doc.rs` fails when a method is added or removed without this file
following.

## Conventions

### Transport

JSON-RPC 2.0 over HTTP. The node listens on `rpc.listen`, default `127.0.0.1:9257`:

```toml
[rpc]
listen = "127.0.0.1:9257"
# token = "<at least 32 characters>"
```

There is one endpoint, `POST /`. The HTTP layer checks, in order:

| HTTP status | when |
|---|---|
| 405 | the method is not `POST` (the reply carries `Allow: POST`) |
| 404 | the path is not `/` |
| 400 | more than one `Host`, `Authorization` or `Content-Length` header, or a malformed `Content-Length` |
| 403 / 400 | the listener is on loopback and `Host` is not `127.0.0.1`, `[::1]` or `localhost` (with the listener's port, if a port is given) (403), or `Host` is missing (400) |
| 501 | a `Transfer-Encoding` header is present; send `Content-Length` |
| 411 | `Content-Length` is missing |
| 413 | the body is larger than 1 MiB (1,048,576 bytes) |
| 415 | `Content-Type` is not `application/json` (parameters such as `; charset=utf-8` are allowed) |
| 401 | `rpc.token` is set and the request has no `Authorization: Bearer <token>`, or the wrong one |
| 408 | the whole request did not arrive within 10 seconds of its first byte |
| 503 | 128 connections are already being served; the reply carries `Retry-After: 1` |

These refusals are not JSON-RPC responses. Their body is
`{"error":"<reason phrase>","status":<status>,"detail":"<text>"}`, and the connection is closed.

A loopback listener needs no token. A listener on any other address refuses to start
without a `rpc.token` of at least 32 characters; when a token is set, every request must
carry it, on loopback too. The node has no TLS.

Connections are kept alive (HTTP/1.1 default, or `Connection: keep-alive`), for up to 1,000
requests and 30 seconds idle.

`curl -d` sends `Content-Type: application/x-www-form-urlencoded` unless told otherwise, so
every example below passes `-H 'Content-Type: application/json'`.

### Requests

A request is an object with `"jsonrpc": "2.0"`, a `method` string, optional `params`, and an
optional `id` that is a string or an integer. A request without `id`, or with `"id": null`,
is a notification: it runs, and nothing is sent back (HTTP 204 when nothing at all is to be
sent). Method names are case-sensitive; an unknown name is answered with `-32601`, and when
the name is close to a real one the detail suggests it.

The JSON parser is strict: fractional numbers and exponents are refused, integers must fit
in a signed 64-bit value, a duplicate object key is refused, and a body may nest at most 16
levels, hold at most 65,536 values, and at most 4,096 entries in any one array or object.

### Parameters

`params` is an array (by position) or an object (by name); a missing or `null` `params`
means none. The names are the ones in each method's parameter table. Passing `null` for an
optional parameter is the same as leaving it out.

Methods that take parameters refuse more than they accept with `-32602`. With named
parameters every member of the object counts toward that limit, and a misspelled name reads
as a missing parameter. Methods that take no parameters ignore any that are sent.

### Batches

An array of 1 to 32 requests is a batch. The response is an array with one entry for each
member that has an `id`, in request order; a member that fails does not affect the others.
An empty array is answered with a single `-32600` error, and more than 32 members with a
single `-32005` error, both with `"id": null`.

### Values

- Amounts are decimal strings in mile, never JSON numbers: 1 PLNE = 1,000,000 mile (six
  decimal places). A mile value can exceed 2^53, which a JavaScript number cannot hold.
- Heights, counts, sizes and ages are JSON integers. Times are Unix seconds.
- Two other values are decimal strings for the same reason: a header's `nonce` (u64) and a
  stratum session's `acceptedDifficulty` (u128).
- Hashes and transaction ids are 64 lowercase hex characters. Raw headers, blocks,
  transactions and records are lowercase hex. On input, hex may be upper- or lowercase and
  may carry a `0x` prefix.
- Addresses are bech32m strings with the prefix `plne` and a 20-byte payload, for example
  `plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5`.
- Field names are camelCase.

### Errors

```json
{"jsonrpc":"2.0","error":{"code":-32002,"message":"Transaction rejected",
  "data":{"detail":"nonce 7 is outside the accepted window [5, 261]; this account's next nonce is 5",
          "reason":"nonce-out-of-range"}},"id":1}
```

| field | type | notes |
|---|---|---|
| `code` | integer | see the table below |
| `message` | string | fixed text for the code |
| `data` | object, optional | absent when there is nothing to add |
| `data.detail` | string, optional | what went wrong and, where there is one, what to do about it; meant for people, not for parsing |
| `data.reason` | string, optional | a machine-readable tag; `tx_sendRaw` and `checkpoint_submit` set it |

An error that happens before the request's `id` is known (unparseable JSON, a batch that is
too large or empty) is answered with `"id": null`.

| code | message | returned for |
|---|---|---|
| `-32700` | Parse error | the body is not valid JSON, or breaks a parser rule (a fraction, an integer outside i64, a duplicate key) |
| `-32600` | Invalid Request | not an object; `jsonrpc` missing or not `"2.0"`; `method` missing; `params` not an array or object; `id` not a string or integer; an empty batch |
| `-32601` | Method not found | the method is not in the list below |
| `-32602` | Invalid params | a missing, malformed or extra parameter |
| `-32603` | Internal error | reserved; not returned by the current server |
| `-32000` | Node not ready | `checkpoint_submit`: the chain did not take the record in time |
| `-32001` | Not found | no such header, block or transaction |
| `-32002` | Transaction rejected | `tx_sendRaw`: the transaction was refused; `data.reason` says why |
| `-32003` | Feature disabled | the node cannot answer as configured: a pruned body, an index it does not keep, checkpoints off |
| `-32004` | Unauthorized | reserved; not returned by the current server (a bad token is refused at the HTTP layer with 401) |
| `-32005` | Limit exceeded | the body breaks a JSON size limit, a batch has more than 32 members, or a hex parameter is longer than the largest legal value |
| `-32006` | Busy | reserved; not returned by the current server (a full server is refused at the HTTP layer with 503) |
| `-32007` | Checkpoint rejected | `checkpoint_submit`: the record does not verify, or names height 0 |

Every method that takes parameters can return `-32602`; the tables below list only the
causes particular to each method.

### Methods

`chain_getInfo`, `chain_getHeaderByHeight`, `chain_getHeaderByHash`,
`chain_getBlockByHeight`, `chain_getBlockByHash`, `account_get`, `tx_sendRaw`, `tx_get`,
`mempool_getInfo`, `mempool_getBySender`, `fee_suggest`, `emission_audit`,
`checkpoint_getStatus`, `checkpoint_submit`, `author_getNotes`, `account_getHistory`,
`net_getPeerInfo`, `stratum_getSessions`, `node_getBudgets`. The list is closed: there is no
method that holds keys, mines, submits blocks, or changes the node's peers.

---

## `chain_getInfo`

The node's view of the chain: tip, sync state, and which indexes it keeps. Call it first to
see whether the node is synced and what it can answer.

### Parameters

None.

### Result

| field | type | notes |
|---|---|---|
| `network` | string | `main` |
| `version` | string | `plaine-noded/<version>` |
| `height` | integer | height of the tip |
| `tipHash` | hex string | hash of the tip block |
| `chainwork` | hex string | cumulative work of the tip, 32 bytes |
| `tipTime` | integer | timestamp in the tip header |
| `tipAgeSecs` | integer | seconds between `tipTime` and the node's clock; 0 if the tip is in the future |
| `sync` | string | `starting`, `syncing`, `synced` or `stalled` |
| `txindex` | boolean | the node runs with `txindex = true` |
| `pruned` | boolean | the node runs with `prune = true` (the default) |
| `pruneHorizonHeight` | integer | lowest height whose block body is still stored |
| `peers` | integer | connected peers |
| `bestKnownHeight` | integer or null | the highest height the node believes the network has reached (never below its own); `null` with no peers |
| `stallReason` | string, optional | present when `sync` is `stalled`: why, and what to do |

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getInfo"}'
```

```json
{"jsonrpc":"2.0","result":{
  "network":"main","version":"plaine-noded/1.0.0","height":12345,
  "tipHash":"00000a3f...9c41","chainwork":"00000000...7e2b00",
  "tipTime":1790900100,"tipAgeSecs":21,"sync":"synced",
  "txindex":false,"pruned":true,"pruneHorizonHeight":0,
  "peers":8,"bestKnownHeight":12345},"id":1}
```

---

## `chain_getHeaderByHeight`

The canonical header at a height, raw and decoded. Headers are kept forever, so this
answers on a pruned node too.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `height` | integer | 0 or more |

### Result

| field | type | notes |
|---|---|---|
| `hash` | hex string | header hash |
| `height` | integer | |
| `confirmations` | integer | 1 for the tip; 0 when the header is not on the canonical chain |
| `canonical` | boolean | the header is on the node's best chain |
| `chainwork` | hex string | cumulative work, 32 bytes; plaine-noded fills it for the tip header only and returns 64 zeros for every other header |
| `raw` | hex string | the 132-byte header |
| `version` | integer | header version |
| `prevHash` | hex string | hash of the parent header |
| `txRoot` | hex string | Merkle root of the block's transactions |
| `extRoot` | hex string | reserved commitment; all zeros under current rules |
| `time` | integer | header timestamp |
| `bits` | integer | compact difficulty target |
| `authorNoteLen` | integer | length of the coinbase author note, bytes |
| `nonce` | string | the proof-of-work nonce, a u64 as a decimal string |

The fields from `version` to `nonce` are decoded from `raw`. If `raw` does not decode, they
are left out and the first six fields are still returned.

### Errors

| code | when |
|---|---|
| `-32001` not found | `height` is above the tip (the detail gives the tip and the sync state), or there is no header at that height |
| `-32602` invalid params | `height` missing, negative or not an integer; more than one parameter |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getHeaderByHeight","params":[12345]}'
```

```json
{"jsonrpc":"2.0","result":{
  "hash":"00000a3f...9c41","height":12345,"confirmations":1,"canonical":true,
  "chainwork":"00000000...7e2b00","raw":"00000020...0000",
  "version":536870912,"prevHash":"000002b1...77d0","txRoot":"5e0c9a44...13fa",
  "extRoot":"00000000...0000","time":1790900100,"bits":503382015,
  "authorNoteLen":0,"nonce":"8410295537"},"id":1}
```

---

## `chain_getHeaderByHash`

The header with a given hash, including headers of side branches the node has seen.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `hash` | hex string | 64 hex characters |

### Result

The same object as `chain_getHeaderByHeight`. For a header off the best chain,
`canonical` is `false` and `confirmations` is 0.

### Errors

| code | when |
|---|---|
| `-32001` not found | the node does not know the hash; when it has refused a block with that hash, the detail names the reason and says whether the refusal can be reversed |
| `-32602` invalid params | `hash` missing, not a string, not 64 characters, or not hex; more than one parameter |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getHeaderByHash","params":["00000a3f...9c41"]}'
```

```json
{"jsonrpc":"2.0","result":{
  "hash":"00000a3f...9c41","height":12345,"confirmations":1,"canonical":true,
  "chainwork":"00000000...7e2b00","raw":"00000020...0000",
  "version":536870912,"prevHash":"000002b1...77d0","txRoot":"5e0c9a44...13fa",
  "extRoot":"00000000...0000","time":1790900100,"bits":503382015,
  "authorNoteLen":0,"nonce":"8410295537"},"id":1}
```

---

## `chain_getBlockByHeight`

The canonical block at a height: its header, the coinbase author note, and either the raw
block or its transaction ids.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `height` | integer | 0 or more |
| 1 | `verbosity` | integer, optional | 0 raw hex, 1 header and txids, 2 header, txids and transactions; default 1 |

### Result

| field | type | notes |
|---|---|---|
| `header` | object | the object `chain_getHeaderByHeight` returns |
| `txCount` | integer | transactions in the block, coinbase included |
| `sizeBytes` | integer | header plus body, bytes |
| `authorNoteHex` | hex string | the coinbase author note as stored; `""` when there is none |
| `authorNoteText` | string | the note decoded as UTF-8 with control, bidi-override and zero-width characters removed; for display |
| `authorNoteValidUtf8` | boolean | the note is valid UTF-8; when `false`, `authorNoteHex` is the only faithful form |
| `raw` | hex string | verbosity 0 only: the whole block, header followed by body |
| `txids` | array of hex strings | verbosity 1 and 2: transaction ids in block order |
| `txs` | array | verbosity 2 only: plaine-noded currently returns an empty array; use `tx_get` for each id |

### Errors

| code | when |
|---|---|
| `-32001` not found | `height` is above the tip, or no block body is stored at that height |
| `-32003` feature disabled | the node is pruned and `height` is below `pruneHorizonHeight`; the header is still available |
| `-32602` invalid params | `height` missing, negative or not an integer; `verbosity` other than 0, 1 or 2; more than two parameters |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getBlockByHeight","params":[12345, 1]}'
```

```json
{"jsonrpc":"2.0","result":{
  "header":{"hash":"00000a3f...9c41","height":12345,"confirmations":1,"canonical":true,
    "chainwork":"00000000...7e2b00","raw":"00000020...0000","version":536870912,
    "prevHash":"000002b1...77d0","txRoot":"5e0c9a44...13fa","extRoot":"00000000...0000",
    "time":1790900100,"bits":503382015,"authorNoteLen":5,"nonce":"8410295537"},
  "txCount":2,"sizeBytes":367,
  "authorNoteHex":"68656c6c6f","authorNoteText":"hello","authorNoteValidUtf8":true,
  "txids":["17070c7f...fa428","9b3e51d0...2c7e1"]},"id":1}
```

---

## `chain_getBlockByHash`

The block with a given hash, in the same shapes as `chain_getBlockByHeight`.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `hash` | hex string | 64 hex characters |
| 1 | `verbosity` | integer, optional | 0, 1 or 2 as for `chain_getBlockByHeight`; default 1 |

### Result

The same object as `chain_getBlockByHeight`.

### Errors

| code | when |
|---|---|
| `-32001` not found | the node does not know the hash; the hash is a side-branch block, whose header the node keeps but not its body (`chain_getHeaderByHash` still answers); or the body is not stored; unlike the by-height form, a pruned body is reported here too |
| `-32602` invalid params | `hash` missing or malformed; `verbosity` other than 0, 1 or 2; more than two parameters |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"chain_getBlockByHash","params":["00000a3f...9c41", 0]}'
```

```json
{"jsonrpc":"2.0","result":{
  "header":{"hash":"00000a3f...9c41","height":12345,"confirmations":1,"canonical":true,
    "chainwork":"00000000...7e2b00","raw":"00000020...0000","version":536870912,
    "prevHash":"000002b1...77d0","txRoot":"5e0c9a44...13fa","extRoot":"00000000...0000",
    "time":1790900100,"bits":503382015,"authorNoteLen":0,"nonce":"8410295537"},
  "txCount":1,"sizeBytes":201,
  "authorNoteHex":"","authorNoteText":"","authorNoteValidUtf8":true,
  "raw":"00000020...0000"},"id":1}
```

---

## `account_get`

An address's balance and nonce. Every valid address has an account; one that has never
received anything returns zeros.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `address` | string | a `plne1...` address |

### Result

| field | type | notes |
|---|---|---|
| `balance` | string | mile; includes coinbase credits that are not yet spendable |
| `nonce` | integer | the account's confirmed nonce: the nonce its next transaction in a block must carry |
| `pendingNonce` | integer | `nonce` plus the number of this address's transactions in the mempool; equals `nonce` if the mempool did not answer within 2 seconds |
| `immature` | string | mile; coinbase credits to this address in the last 60 blocks, which cannot be spent yet |
| `spendable` | string | mile; `balance` minus `immature` |

### Errors

| code | when |
|---|---|
| `-32602` invalid params | `address` missing or not a string; not valid bech32m (a typo: the checksum fails); a prefix other than `plne`; a payload that is not 20 bytes; more than one parameter |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"account_get",
  "params":["plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5"]}'
```

```json
{"jsonrpc":"2.0","result":{
  "balance":"12600000","nonce":3,"pendingNonce":4,
  "immature":"400000","spendable":"12200000"},"id":1}
```

---

## `tx_sendRaw`

Submits a signed transaction to the node's mempool, which relays it. Only transfers (type
`0x01`) and author announcements (type `0x02`) are accepted; the node never signs anything.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `raw` | hex string | the serialized transaction; at most 8,192 bytes (16,384 hex characters) |

### Result

The transaction id, a 64-character hex string.

### Errors

| code | when |
|---|---|
| `-32002` transaction rejected | the mempool refused it; `data.reason` is one of the tags below and `data.detail` carries the figures |
| `-32005` limit exceeded | `raw` is longer than 16,384 hex characters (checked before decoding) |
| `-32602` invalid params | `raw` missing, not a string, or not valid hex; more than one parameter |

`data.reason` tags:

| tag | meaning |
|---|---|
| `malformed` | the bytes do not decode as a transfer or an announcement, or carry a type RPC does not accept (a coinbase) |
| `too-large` | larger than the consensus limit of 8,192 bytes |
| `fee-below-relay-floor` | the fee is below this node's `mempool.relay_fee_mile` (operator policy) or the consensus floor of 1 mile |
| `bad-signature` | the ed25519 signature does not verify |
| `nonce-out-of-range` | the nonce is below the account's next nonce, or more than 256 above it |
| `insufficient-funds` | amount plus fee exceeds the spendable balance; immature coinbase credits do not count |
| `not-author-key` | an announcement not signed with the author key this network runs |
| `duplicate` | the node already has this transaction |
| `replacement-underpriced` | another transaction with the same sender and nonce is pooled, and this one's fee is too low to replace it |
| `pool-full` | the mempool, or this sender's share of it, is at its cap |
| `not-ready` | the node is syncing, or its validator did not take the transaction in time; retry later |
| `type-not-accepted` | defined, but plaine-noded reports such a transaction as `malformed` |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tx_sendRaw","params":["01a4c2...5f0e"]}'
```

```json
{"jsonrpc":"2.0","result":"9b3e51d0...2c7e1","id":1}
```

---

## `tx_get`

A transaction by id, with where it is: in the mempool, or in a block with its height and
confirmations. The node looks in the mempool, then in recent blocks not yet written to disk,
then in the txid index when it runs with `txindex = true`. Without `txindex`, older
confirmed transactions cannot be found by id alone.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `txid` | hex string | 64 hex characters |
| 1 | `address` | string, optional | an address the transaction touches: sender, recipient, coinbase recipient or announcement author. Added in this fork |

When `txindex` cannot answer and the node runs with `addrindex = true`, an `address` makes
the node search the confirmed transactions that touch it, newest first. This is how a
wallet follows its own transactions without `txindex`: it knows the address, and its
transactions are near the top of that address's history. With `txindex = true` the address
plays no part in the answer, but it is still checked.

### Result

| field | type | notes |
|---|---|---|
| `txid` | hex string | |
| `type` | integer | 0 coinbase, 1 transfer, 2 author announcement |
| `raw` | hex string | the serialized transaction |
| `location` | object | see below |
| `decoded` | null | reserved for a decoded form; plaine-noded always returns `null`. Decode `raw` instead |

`location` in the mempool:

| field | type | notes |
|---|---|---|
| `where` | string | `mempool` |
| `confirmations` | integer | always 0 |

`location` in a block:

| field | type | notes |
|---|---|---|
| `where` | string | `block` |
| `height` | integer | height of the block that holds it |
| `confirmations` | integer | 1 for a transaction in the tip block |

### Errors

| code | when |
|---|---|
| `-32001` not found | the node runs with `txindex`, and the transaction is in neither the mempool, the recent blocks, nor the index; or, searching by `address` with an address index that covers the whole chain, no transaction with that id touches the address |
| `-32003` feature disabled | not in the mempool or the recent blocks, and neither index can answer; or the index begins above the height needed, so the node cannot say whether the transaction exists (the detail names that height); or `txindex` places it in a block whose body a pruned node no longer stores (the detail names the block). Searching by `address`, the search also stops at the first block body a pruned node no longer has, and after 10,000 entries |
| `-32602` invalid params | `txid` missing, not a string, not 64 characters, or not hex; a malformed `address`; more than two parameters |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tx_get","params":["9b3e51d0...2c7e1"]}'
```

```json
{"jsonrpc":"2.0","result":{
  "txid":"9b3e51d0...2c7e1","type":1,"raw":"01a4c2...5f0e",
  "location":{"where":"block","height":12345,"confirmations":3},
  "decoded":null},"id":1}
```

A mempool transaction has `"location":{"where":"mempool","confirmations":0}`.

---

## `mempool_getInfo`

Mempool size and the fee floors this node applies. Use it to see how full the pool is before
choosing a fee.

### Parameters

None.

### Result

| field | type | notes |
|---|---|---|
| `txCount` | integer | transactions in the mempool |
| `bytes` | integer | their total size |
| `executable` | integer | transactions whose nonce follows on from their account's, so a block can include them now |
| `queued` | integer | `txCount` minus `executable`: waiting behind a nonce gap |
| `relayFeeMile` | string | this node's relay floor, `mempool.relay_fee_mile` (default `"1"`) |
| `maxTxs` | integer | the mempool's capacity, `mempool.max_txs` (default and maximum 20,000) |
| `consensusFeeFloorMile` | string | the lowest fee consensus allows: `"1"` |

If the mempool does not answer within 2 seconds, the four counts are 0.

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"mempool_getInfo"}'
```

```json
{"jsonrpc":"2.0","result":{
  "txCount":14,"bytes":2198,"executable":12,"queued":2,
  "relayFeeMile":"1","maxTxs":20000,"consensusFeeFloorMile":"1"},"id":1}
```

---

## `mempool_getBySender`

An address's transactions waiting in the mempool, in nonce order. A wallet uses it to see
what it has sent that is not yet confirmed.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `address` | string | a `plne1...` address, the sender |

### Result

An array of transaction objects in the shape `tx_get` returns, each with
`"location":{"where":"mempool","confirmations":0}`, ordered by nonce, lowest first. An empty
array when the address has nothing pooled, or when the mempool does not answer within 2
seconds.

### Errors

| code | when |
|---|---|
| `-32602` invalid params | a malformed address (as for `account_get`); more than one parameter |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"mempool_getBySender",
  "params":["plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5"]}'
```

```json
{"jsonrpc":"2.0","result":[
  {"txid":"9b3e51d0...2c7e1","type":1,"raw":"01a4c2...5f0e",
   "location":{"where":"mempool","confirmations":0},"decoded":null}],"id":1}
```

---

## `fee_suggest`

Suggested transaction fees, in mile per transaction, from the fees transfers actually paid
in the last 240 blocks. A wallet offers `p50Mile` by default, `p10Mile` when time does not
matter and `p90Mile` to be included sooner.

### Parameters

None.

### Result

| field | type | notes |
|---|---|---|
| `blocksSampled` | integer | blocks read, up to 240 back from the tip; fewer on a young chain, or on a pruned node whose bodies stop sooner |
| `p10Mile` | string | 10th-percentile transfer fee |
| `p50Mile` | string | median transfer fee |
| `p90Mile` | string | 90th-percentile transfer fee |
| `relayFloorMile` | string | this node's relay floor, the least it will accept |

Percentiles are nearest-rank over every transfer in the sampled blocks; coinbases and
announcements do not count. None is below `relayFloorMile`, since a lower fee would be
refused here, and with no transfers in the window all three equal it. The answer is
computed once per tip and cached until the tip moves. Before this fork the node sampled
nothing and always returned the floor.

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"fee_suggest"}'
```

```json
{"jsonrpc":"2.0","result":{
  "blocksSampled":240,"p10Mile":"1","p50Mile":"10","p90Mile":"250","relayFloorMile":"1"},"id":1}
```

---

## `emission_audit`

Compares the coins actually issued with what the emission formula allows, so anyone can
check the supply. Only the tip can be audited: the issued total is a running sum with no
per-height record.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `height` | integer, optional | must equal the tip height; default the tip |

### Result

| field | type | notes |
|---|---|---|
| `height` | integer | the height audited, the tip |
| `issuedMile` | string | total issued, as the node's ledger records it |
| `expectedByFormulaMile` | string | total the formula allows through `height`: 200,000 mile per block after genesis |
| `differenceMile` | string | `issuedMile` minus `expectedByFormulaMile`, signed: `"0"` when they match, with a leading `-` when below |
| `maxSupplyMile` | string or null | the supply cap; `null`, since emission has no cap |
| `subsidyAtHeightMile` | string | the block subsidy at `height` (`"200000"`; `"0"` at genesis) |
| `matchesFormula` | boolean | `issuedMile` equals `expectedByFormulaMile` |
| `underMaxSupply` | boolean | `issuedMile` is at most `maxSupplyMile`; `true` when there is no cap |

### Errors

| code | when |
|---|---|
| `-32602` invalid params | `height` is not the tip (the detail gives the tip); `height` negative or not an integer; more than one parameter |
| `-32001` not found | the tip moved below the height read a moment earlier (a reorg onto a shorter chain); retry |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"emission_audit"}'
```

```json
{"jsonrpc":"2.0","result":{
  "height":12345,"issuedMile":"2469000000","expectedByFormulaMile":"2469000000",
  "differenceMile":"0","maxSupplyMile":null,"subsidyAtHeightMile":"200000",
  "matchesFormula":true,"underMaxSupply":true},"id":1}
```

---

## `checkpoint_getStatus`

Whether signed checkpoints protect this node, with which keys, and the last anchor it
accepted. Signed checkpoints can only reject reorgs; they never create, reorder or censor
blocks, and they stop at a sunset height compiled into the binary.

### Parameters

None.

### Result

| field | type | notes |
|---|---|---|
| `enabled` | boolean | checkpoints are configured and can reach the chain; `false` when either is not the case |
| `configured` | boolean | `checkpoints.enabled` in the configuration |
| `ingest` | string | `live`: a signed checkpoint can reach the chain; `severed`: this build cannot deliver one. plaine-noded is always `live` |
| `keySource` | string | `embedded` (the keys built into the binary) or `config` |
| `keyFingerprints` | array of strings | one per authority key: the first 4 bytes of the key, as 8 hex characters |
| `threshold` | integer | signatures a checkpoint needs |
| `enforcedCount` | integer | heights currently enforced; 0 when `ingest` is `severed` |
| `sunsetHeight` | integer | height from which no checkpoint is accepted: 525,960 |
| `sunsetPassed` | boolean | the tip is at or above `sunsetHeight` |
| `blocksUntilSunset` | integer or null | `sunsetHeight` minus the tip height, never below 0 |
| `lastAnchor` | object or null | the highest accepted checkpoint; `null` when there is none or `ingest` is `severed` |
| `note` | string | a fixed statement of what checkpoints can and cannot do |
| `warning` | string, optional | present when checkpoints are off in the configuration, or configured but `severed` |

`lastAnchor`:

| field | type | notes |
|---|---|---|
| `height` | integer | |
| `hash` | hex string | the block hash the checkpoint names |

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"checkpoint_getStatus"}'
```

```json
{"jsonrpc":"2.0","result":{
  "enabled":true,"configured":true,"ingest":"live","keySource":"embedded",
  "keyFingerprints":["0741b159"],"threshold":1,"enforcedCount":1,
  "sunsetHeight":525960,"sunsetPassed":false,"blocksUntilSunset":513615,
  "lastAnchor":{"height":12000,"hash":"00000c71...a90b"},
  "note":"Signed checkpoints can only reject reorgs; they never create, reorder or censor blocks. The sunset height is compiled into the binary and configuration cannot extend it."},"id":1}
```

---

## `checkpoint_submit`

Delivers an offline-signed checkpoint record to the node. When it verifies against the
configured authority keys and threshold, the node's anchor moves up to it.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `record` | hex string | the encoded record; at most 1,482 bytes (2,964 hex characters) |

The record is: version (1 byte, `0x01`), height (8 bytes, little-endian), block hash (32
bytes), signature count (1 byte, at most 15), then for each signature the 32-byte public key
followed by the 64-byte signature.

### Result

| field | type | notes |
|---|---|---|
| `result` | string | `advanced` or `unchanged` |
| `anchorAdvanced` | boolean | `true` for `advanced` |
| `height` | integer | `advanced` only: height of the new anchor |
| `enforcedCount` | integer | `advanced` only: heights now enforced |
| `enforcing` | boolean | `advanced` only: the node holds the named block at that height, so the height is also enforced. `false` means the node is on another branch: a reorg onto a branch containing that block becomes admissible past the depth cap |
| `detail` | string | a sentence explaining the outcome |

`unchanged` means the record verified but the node already holds an anchor at that height
or higher; the anchor only moves up.

### Errors

| code | when |
|---|---|
| `-32007` checkpoint rejected | the record does not verify against the configured keys and threshold, or its height is at or past the sunset (`data.reason` `unverified`); or it names height 0 (`data.reason` `genesisImmutable`) |
| `-32003` feature disabled | `checkpoints.enabled = false` (`data.reason` `notConfigured`); or the build cannot deliver checkpoints (`data.reason` `severed`) |
| `-32000` node not ready | the chain did not take the record within 2 seconds (`data.reason` `busy`); the record was not examined, retry |
| `-32005` limit exceeded | `record` longer than 2,964 hex characters |
| `-32602` invalid params | `record` missing, not a string or not hex; the bytes are not a record (`data.reason` `malformed`, the detail names the fault: `truncated`, `unknown record version`, `too many signatures`, `trailing bytes after the last signature`); more than one parameter |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"checkpoint_submit","params":["01e02e0000...7c0d"]}'
```

```json
{"jsonrpc":"2.0","result":{
  "result":"advanced","anchorAdvanced":true,"height":12000,"enforcedCount":1,
  "enforcing":true,
  "detail":"the anchor advanced and this node holds the named block, so the height is also enforced (layer 3)."},"id":1}
```

---

## `author_getNotes`

Author announcements (type `0x02` transactions) found in blocks, newest first, with the
author key status. Readable while the node is stalled.

plaine-noded builds this list in memory from blocks it connects while running: after a
restart it lists only announcements from blocks connected since then.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `fromHeight` | integer, optional | only announcements at or below this height; not together with `cursor` |
| 1 | `limit` | integer, optional | announcements per page, 1–100; default 20 |
| 2 | `cursor` | integer, optional | the `nextCursor` of the previous page; not together with `fromHeight` |

`fromHeight` filters backwards from a height, because results are newest first; it is not a
point to read forwards from, so `fromHeight: 0` returns nothing unless there is an
announcement at genesis.

### Result

| field | type | notes |
|---|---|---|
| `notes` | array | see below, newest first |
| `more` | boolean | older announcements match beyond this page |
| `total` | integer | announcements the node holds, regardless of filter or cursor |
| `nextCursor` | integer or null | pass back as `cursor` for the next page; `null` on the last page |
| `hint` | string, optional | present when the page is empty but `total` is not 0: why, and how to get the newest |
| `authorKey` | object | see below |
| `node` | object | see below |

Each note:

| field | type | notes |
|---|---|---|
| `height` | integer | height of the block that holds the announcement |
| `seq` | integer | position in the node's list; used by `cursor`, not a stable id (renumbered after a reorg) |
| `txid` | hex string | |
| `time` | integer | block timestamp; plaine-noded currently reports 0 |
| `confirmations` | integer | 1 for an announcement in the tip block |
| `encoding` | integer | the author's display hint: 0 opaque, 1 UTF-8, 2 URI, 3 application-defined; not checked by consensus |
| `hex` | hex string | the payload as stored |
| `text` | string | the payload decoded as UTF-8, with control, bidi-override, zero-width and line-separator characters removed; for display |
| `validUtf8` | boolean | the payload is valid UTF-8; when `false`, `hex` is the only faithful form |
| `sanitizedChars` | integer | characters removed or replaced to make `text` |

`authorKey`:

| field | type | notes |
|---|---|---|
| `enabled` | boolean | always `true` in plaine-noded |
| `source` | string | `embedded` or `config` |
| `fingerprint` | string | first 4 bytes of the author public key, 8 hex characters |
| `showInLog` | boolean | `author.show_in_log`: new announcements are printed to the node log |

`node`:

| field | type | notes |
|---|---|---|
| `height` | integer | tip height |
| `sync` | string | as in `chain_getInfo` |
| `tipAgeSecs` | integer | as in `chain_getInfo` |
| `peers` | integer | connected peers |

### Errors

| code | when |
|---|---|
| `-32602` invalid params | both `fromHeight` and `cursor` given; `limit` 0 or above 100; a parameter negative or not an integer; more than three parameters |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"author_getNotes","params":{"limit":1}}'
```

```json
{"jsonrpc":"2.0","result":{
  "notes":[
    {"height":12001,"seq":1,"txid":"4c1d09e2...b7a3","time":0,"confirmations":345,
     "encoding":1,"hex":"4973...3030","text":"Isochron v2 activates at height 600000",
     "validUtf8":true,"sanitizedChars":0}],
  "more":true,"total":2,"nextCursor":1,
  "authorKey":{"enabled":true,"source":"embedded","fingerprint":"4d1e77b0","showInLog":true},
  "node":{"height":12345,"sync":"synced","tipAgeSecs":21,"peers":8}},"id":1}
```

---

## `account_getHistory`

Confirmed transactions that touch an address, newest first, from that address's side.
Added in this fork.

Needs the node to run with the address index:

```toml
[node]
addrindex = true
```

The index covers blocks connected after it was switched on; resync to cover the whole
chain. On a pruned node, transactions in blocks whose bodies are no longer stored cannot
be described; they are left out and `hint` says below which height.

### Parameters

| # | name | type | notes |
|---|---|---|---|
| 0 | `address` | string | a `plne1...` address |
| 1 | `limit` | integer, optional | entries per page, 1–200; default 50 |
| 2 | `cursor` | string, optional | the `nextCursor` of the previous page |

### Result

| field | type | notes |
|---|---|---|
| `address` | string | the address asked about |
| `indexedFrom` | integer | first height the index covers |
| `entries` | array | see below, newest first |
| `nextCursor` | string or null | pass back as `cursor` for the next page; `null` on the last page |
| `hint` | string, optional | present when the history is incomplete: entries skipped on a pruned node, or an index that does not start at genesis |

Each entry:

| field | type | notes |
|---|---|---|
| `txid` | hex string | |
| `height` | integer | block height |
| `index` | integer | position of the transaction in the block |
| `time` | integer | block timestamp, Unix seconds |
| `confirmations` | integer | 1 for a transaction in the tip block |
| `kind` | string | `coinbase`, `transfer` or `announcement` |
| `direction` | string | `in`, `out` or `self` (a transfer to oneself) |
| `amountMile` | string | coinbase credit (reward plus fees), transfer amount, or `"0"` for an announcement |
| `feeMile` | string | fee paid by the sender; `"0"` for a coinbase |
| `counterparty` | string or null | the other address of a transfer; `null` for a coinbase or an announcement |

### Errors

| code | when |
|---|---|
| `-32003` feature disabled | the node runs without `addrindex` |
| `-32602` invalid params | a malformed address, a `limit` outside 1–200, or a `cursor` that is not a `nextCursor` string |

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"account_getHistory",
  "params":["plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5", 2]}'
```

```json
{"jsonrpc":"2.0","result":{
  "address":"plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5",
  "indexedFrom":0,
  "entries":[
    {"txid":"17070c7f...fa428","height":3,"index":0,"time":1790159975,"confirmations":1,
     "kind":"coinbase","direction":"in","amountMile":"200000","feeMile":"0","counterparty":null},
    {"txid":"f7ce0cf0...34009","height":2,"index":0,"time":1790159973,"confirmations":2,
     "kind":"coinbase","direction":"in","amountMile":"200000","feeMile":"0","counterparty":null}
  ],
  "nextCursor":"2:0"},"id":1}
```

### How it stays correct across reorgs

The index stores positions — (address, height, index) — and never deletes them. For
every position the node reads the block body that is canonical at that height now and
checks that the transaction there still involves the address. A row left behind by a
block that a reorg replaced fails that check and is skipped. `tx_get` treats `txindex`
hits the same way.

---

## `net_getPeerInfo`

The node's connected peers, with direction, traffic counters and misbehaviour score.

### Parameters

None.

### Result

An array, one object per connected peer:

| field | type | notes |
|---|---|---|
| `id` | integer | the node's id for the connection |
| `addr` | string | the peer's IP address, without port; IPv4-mapped addresses print as dotted quads |
| `direction` | string | `outbound` or `inbound` |
| `connectedSecs` | integer | seconds since the connection opened |
| `bestHeight` | integer | the highest height the peer has claimed |
| `bytesRecv` | integer | bytes received from the peer |
| `bytesSent` | integer | bytes sent to the peer |
| `misbehaviour` | integer | the peer's misbehaviour score |
| `userAgent` | string | the peer's self-reported user agent, with control, bidi-override and zero-width characters removed |

The list is a snapshot the node refreshes periodically, not a live read.

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"net_getPeerInfo"}'
```

```json
{"jsonrpc":"2.0","result":[
  {"id":7,"addr":"203.0.113.5","direction":"outbound","connectedSecs":5412,
   "bestHeight":12345,"bytesRecv":1843220,"bytesSent":920113,"misbehaviour":0,
   "userAgent":"plaine-noded/1.0.0"}],"id":1}
```

---

## `stratum_getSessions`

The miners connected to this node's stratum server. An empty array when stratum is off or
no one is connected.

### Parameters

None.

### Result

An array, one object per session:

| field | type | notes |
|---|---|---|
| `worker` | string | `address.rig`, or `address` when the miner gave no rig name; `""` before login; control characters removed |
| `address` | string | the payout address the miner logged in with; `""` before login |
| `rig` | string | the rig name; `""` when none was given; control characters removed |
| `ip` | string | the miner's IP address |
| `authorized` | boolean | the session has logged in successfully |
| `connectedSecs` | integer | seconds since the connection opened |
| `lastShareSecs` | integer | seconds since the last accepted share; -1 when none has been accepted |
| `acceptedShares` | integer | shares accepted in this session |
| `acceptedDifficulty` | string | the difficulty of all accepted shares added up, a u128 as a decimal string |
| `difficulty` | integer | the share difficulty currently assigned to the session |

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"stratum_getSessions"}'
```

```json
{"jsonrpc":"2.0","result":[
  {"worker":"plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5.rig1",
   "address":"plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5","rig":"rig1",
   "ip":"192.168.1.20","authorized":true,"connectedSecs":3600,"lastShareSecs":4,
   "acceptedShares":412,"acceptedDifficulty":"412000","difficulty":1000}],"id":1}
```

---

## `node_getBudgets`

Live counters for the node's CPU, queue and memory budgets. For operators watching load;
none of these values affect consensus.

### Parameters

None.

### Result

| field | type | notes |
|---|---|---|
| `cpuPoolThreads` | integer | threads in the verification pool |
| `powVerifiesTotal` | integer | proof-of-work computations since start |
| `powCacheHitPct` | integer | share of proof-of-work checks answered from cache, percent, rounded down |
| `stratumSharesPerSec` | integer | stratum shares accepted per second, sampled at most once a second |
| `stratumSharesCapPerSec` | integer | the share rate the pool is sized for: `cpuPoolThreads` × 83.3, rounded down |
| `validatorQueueBytes` | integer | bytes queued for the validator |
| `validatorQueueBytesCap` | integer | the cap on those bytes (33,554,432) |
| `validatorQueueItems` | integer | commands queued for the validator |
| `validatorQueueItemsCap` | integer | the capacity of that queue |
| `chainEventsTotal` | integer | headers connected, bodies validated and reorgs, added up, since start |
| `mempoolSources` | integer | peers the chain currently tracks a rate budget for |
| `mempoolSourcesCap` | integer | the most it tracks at once |
| `rpcRejectedBusy` | integer | RPC queries to the validator that were refused or not answered within 2 seconds, since start |
| `rssBytes` | integer or null | resident memory of the process; `null` when it cannot be measured (only Linux measures it) |
| `rssBudgetBytes` | integer | the memory budget, 536,870,912 (512 MiB) |
| `bodyRejectsAlreadyHeld` | integer | block bodies refused because the node already had them |
| `bodyRejectsNotAdmissible` | integer | block bodies refused as not admissible |

Counters that come from the validator (`powVerifiesTotal`, `powCacheHitPct`,
`chainEventsTotal`, `mempoolSources`, `mempoolSourcesCap`, `bodyRejects*`) are 0 when it
does not answer within 2 seconds.

### Errors

None particular to this method.

### Example

```
curl -s http://127.0.0.1:9257 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"node_getBudgets"}'
```

```json
{"jsonrpc":"2.0","result":{
  "cpuPoolThreads":4,"powVerifiesTotal":18230,"powCacheHitPct":41,
  "stratumSharesPerSec":0,"stratumSharesCapPerSec":333,
  "validatorQueueBytes":0,"validatorQueueBytesCap":33554432,
  "validatorQueueItems":0,"validatorQueueItemsCap":1024,
  "chainEventsTotal":24702,"mempoolSources":8,"mempoolSourcesCap":64,
  "rpcRejectedBusy":0,"rssBytes":187904000,"rssBudgetBytes":536870912,
  "bodyRejectsAlreadyHeld":3,"bodyRejectsNotAdmissible":0},"id":1}
```
