# tzim 0.16.3 patch

Source: the crates.io release of `tzim` 0.16.3 (MIT, upstream commit
`ae35a5eed89dcfe0156ae62b3208a6d0d48ee8c8`). See `LICENSE` and
`THIRD-PARTY.md`. This copy omits CI assets and support files not needed to
build or test the crate.

`src/formats/jpeg.rs` adds `optimize_verified`. It reuses the parsed and decoded
input during verification, compares each candidate coefficient block against
the retained input, then runs the existing independent pixel check. It returns
the candidate buffer directly. The existing writer and verifier APIs remain.

Remove this patch when an upstream version offers the same verified path.
