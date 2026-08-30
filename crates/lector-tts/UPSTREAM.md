# Upstream source

This package vendors the MIT-licensed `tts` crate from
<https://github.com/ndarilek/tts-rs> at revision
`8fbcb720cb86c5166a9ed46272e3777f742da18e` (upstream version `0.27.0`).

The Rust sources, Android bridge, build script, and README are copied without
functional changes except for Lector's maintained host and backend adapters.
The package retains the Rust library name `tts` while publishing the
cross-platform `lector-tts` host binary from the same crate.
