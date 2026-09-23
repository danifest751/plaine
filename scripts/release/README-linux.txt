Plaine for Linux (x86_64, static binaries) - {VERSION}
https://github.com/danifest751/plaine

  plaine-noded    the node
  plaine-wallet   the command-line wallet
  plaine-miner    the CPU miner
  noded.toml      node settings with the address history on

Quick start

  ./plaine-noded --config noded.toml          # leave it running
  ./plaine-wallet new --role spend --out my.plnekey --passphrase-file pass.txt
  ./plaine-miner plne1youraddress             # mines to the local node
  ./plaine-miner plne1youraddress.rig@eu.rplant.xyz:17190   # or on a pool

The binaries are statically linked and need no libraries. The desktop wallet is not in
this archive; build it from source (wallet-gui/).

User guide: https://github.com/danifest751/plaine/blob/main/docs/USER_GUIDE.md
Every program prints the commit it was built from with --version.
