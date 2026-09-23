Plaine is honest, censorship-free money open to everyone: built to shut out ASIC
and GPU so every CPU earns for the real power it brings, with a steady reward that
keeps issuance predictable and no supply cap to keep the network secure, while its
inflation falls toward zero with every passing day. No premine, no presale, no
dev fund.

In tribute to Satoshi Nakamoto, and in the author's own name, Plaine stands for
one CPU, one vote.

Nothing promised. Nothing hidden. See for yourself.

## Start mining

Three steps, on any machine with a CPU.

1. Run a node. It finds the network through the built-in seeds and syncs the chain.

       plaine-noded

2. Make an address to mine to. This prints the address and writes the key to miner.key.

       plaine-wallet new --role spend --out miner.key --no-passphrase

3. Start the miner with the address from step 2. With no host it mines to the node
   you started in step 1.

       plaine-miner <your-plne1-address>

That is all. The node keeps running and stays in sync; the miner mines against it; a
block you find pays its coinbase straight to your address. Stop either with Ctrl+C.

It does one thing and does it plainly: it moves coins. Accounts, nonces, ed25519
signatures. Flat emission of 0.2 PLNE per block, on and on, with no supply cap,
no premine, no founder's stash, and no developer tax. Nobody was paid before you
showed up.

The full rules a node enforces are in [SPEC.md](SPEC.md). Nothing below is a
summary of the consensus rules; it is just how to build and run the thing.

## Build

You need a recent stable Rust, 1.85 or newer.

```
cargo build --release
cargo build --release --manifest-path miner/Cargo.toml
```

The binaries come out in `target/release` and `miner/target/release`:

```
plaine-noded    the node
plaine-wallet   the wallet
plaine-miner    the CPU miner
```

## Run a node

```
plaine-noded
```

A fresh node dials the built-in seed, pulls headers and then bodies, and starts
following the chain. Its stratum server listens on 127.0.0.1:9258 and its RPC on
127.0.0.1:9257. To mine from other machines, set `listen = "0.0.0.0:9258"` under
`[stratum]` in the config. `plaine-noded --help` lists the rest.

## Mine

Point the miner at a node's stratum port and give it an address to pay:

```
plaine-miner --help
```

It is CPU only, by design. Isochron runs well on a normal processor and badly on
GPUs and ASICs, so there is nothing to gain by reaching for either.

## Wallet

The wallet holds the keys and the node holds none. It never opens a socket: it
prints a signed transaction as hex and you hand that to the node over RPC.
Because it cannot see the chain, you pass `--nonce` and `--fee` yourself; there
are no defaults to guess them for you.

```
plaine-wallet new --out key.plnekey --role spend --seed-stdin --passphrase-file pass.txt
plaine-wallet address --in key.plnekey
plaine-wallet transfer --in key.plnekey --to plne1... \
    --amount 5plne --fee 1000mile --nonce 0 --passphrase-file pass.txt
```

`plaine-wallet --help` has the full command list.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
