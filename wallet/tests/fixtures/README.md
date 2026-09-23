# Key file fixtures

Written by upstream's `plaine-wallet` (`noaltitude/plaine` at `189598d`), unmodified,
so the tests can prove that key files made before this fork's changes still open.

- `upstream-none.plnekey`: `kdf: none`, the seed in the clear.
- `upstream-blake3.plnekey`: `kdf: blake3-iter-v1`, 1,000 iterations, passphrase
  `fixture passphrase`.

Both hold the same **public test seed**,
`02716da46ebbd89a18e033566cd4a5564e26745fe2e76d760d39cf34ace21ea9` (SHA-256 of
`plaine-wallet public test fixture seed`), address
`plne1ned7pg9p8wk3jdu69m5g7yt39yc4s6ye8zgdk9`. Anyone can spend from it. Never send
anything there.
