# Vendored signalsmith-stretch 0.1.3

Copy of the crates.io crate `signalsmith-stretch` 0.1.3 (Rust wrapper by Colin Marc, MIT) bundling the
C++ library Signalsmith Stretch (Geraint Luff / Signalsmith Audio Ltd., MIT) and its dependency
`signalsmith-linear` (Signalsmith Audio, MIT).

Changes against upstream (WARP_09):
- `src/bindings.rs` is the pre-generated bindgen output, so `libclang` / `bindgen` are not needed;
  `build.rs` only compiles `src/wrapper.cpp` with `g++` (cross: `CXX_aarch64_unknown_linux_gnu`).
- Dropped the unused `dasp` dependency, examples, web/cmd/test folders.
- The workspace uses it through `[patch.crates-io]`; it is excluded from the workspace members.

Licences: `LICENSE.md` (Rust wrapper), `signalsmith-stretch/LICENSE.txt`, `signalsmith-linear/LICENSE.txt`.
