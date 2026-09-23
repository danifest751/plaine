Plaine for Windows - {VERSION}
https://github.com/danifest751/plaine

  plaine-noded.exe       the node
  plaine-wallet-gui.exe  the desktop wallet
  plaine-miner.exe       the CPU miner
  plaine-wallet.exe      the command-line wallet
  plaine-wallet-cli.exe  the same, able to open key files the desktop wallet writes

  start-node.bat         runs the node with noded.toml (history on)
  mine-pool.bat          mines on a pool; put your address in it first

Quick start

  1. Run start-node.bat and leave it open. The first sync takes a few minutes.
  2. Run plaine-wallet-gui.exe. Create a key; write down the backup string it shows
     once. Your address is on the Home screen.
  3. Mine from the wallet's Mining tab, or on a pool with mine-pool.bat.

The user guide, with pictures:
  https://github.com/danifest751/plaine/blob/main/docs/USER_GUIDE.md

Every program prints the commit it was built from with --version.
Check the download against SHA256SUMS on the release page.
