# Node RPC: methods added by this fork

The node's RPC is JSON-RPC 2.0 over HTTP on `127.0.0.1:9257`. Upstream's methods are
listed in SPEC §14. This file documents what the fork adds; it grows into a full
reference in roadmap item 1.4.

Parameters are positional (`"params": [...]`) or named (`"params": {...}`). Amounts are
decimal strings in mile (1 PLNE = 1,000,000 mile), never JSON numbers.

---

## `account_getHistory`

Confirmed transactions that touch an address, newest first, from that address's side.

Needs the node to run with the address index:

```toml
[node]
addrindex = true
```

The index covers blocks connected after it was switched on; resync to cover the whole
chain. On a pruned node, transactions in blocks whose bodies are no longer stored cannot
be described and are reported as skipped.

### Parameters

| # | name | type | |
|---|---|---|---|
| 0 | `address` | string | a `plne1...` address |
| 1 | `limit` | integer, optional | entries per page, 1–200; default 50 |
| 2 | `cursor` | string, optional | the `nextCursor` of the previous page |

### Result

| field | type | |
|---|---|---|
| `address` | string | the address asked about |
| `indexedFrom` | integer | first height the index covers |
| `entries` | array | see below, newest first |
| `nextCursor` | string or null | pass back as `cursor` for the next page; `null` on the last page |
| `hint` | string, optional | present when the history is incomplete: entries skipped on a pruned node, or an index that does not start at genesis |

Each entry:

| field | type | |
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
curl -s http://127.0.0.1:9257 -d '{"jsonrpc":"2.0","id":1,"method":"account_getHistory",
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
