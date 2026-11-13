# Third-party licences

The licence inventory for the crates that ship inside the Plippa binaries. The prose
attribution and the special-handling notes live in [`docs/third-party-notices.md`](docs/third-party-notices.md);
this file is the complete, reproducible list it refers to.

**Scope.** The product binaries are `plippa`, `plippa-engine` (feature `stretch`),
`plippa-server`, `plippa-ui` (features `poc2,accessory-2g4`), `plippa-leds` (feature `ws281x`)
and `plippa-media-helper` (bin of `plippa-audio`). The list below is the union of their
normal-dependency closures. Build-only crates are included because they are in the graph;
they are compiled but never redistributed. The `eval/` POC workspace is **not** part of the
product and is not listed here (see `eval/USAGE.md`).

**How this file is produced** (cargo-about / cargo-deny are deliberately not installed —
owner decision; this list is hand-generated with the commands below):

```bash
# 1. closure of each shipped binary, with cargo's licence field
LIBCLANG_PATH=/tmp/libclang cargo tree -p plippa --prefix none --format '{p}|{l}' --offline
LIBCLANG_PATH=/tmp/libclang cargo tree -p plippa-engine --features stretch --prefix none --format '{p}|{l}' --offline
cargo tree -p plippa-server --prefix none --format '{p}|{l}' --offline
LIBCLANG_PATH=/tmp/libclang cargo tree -p plippa-ui --features poc2,accessory-2g4 --prefix none --format '{p}|{l}' --offline
LIBCLANG_PATH=/tmp/libclang cargo tree -p plippa-leds --features ws281x --prefix none --format '{p}|{l}' --offline
cargo tree -p plippa-audio --prefix none --format '{p}|{l}' --offline

# 2. union and de-duplicate; for each crate read LICENSE*/COPYING/NOTICE* from
#    ~/.cargo/registry/src/index.crates.io-*/<crate>-<version>/ and keep the
#    "Copyright <year> ..." lines
```

254 packages: 241 third-party, 13 first-party
(`plippa`, `plippa-audio`, `plippa-board`, `plippa-core`, `plippa-engine`, `plippa-hw`, `plippa-image`, `plippa-ipc-proto`, `plippa-leds`, `plippa-noubin`, `plippa-server`, `plippa-store`, `plippa-ui`).

## Licences in use

| Licence expression | Crates |
|---|---|
| `MIT OR Apache-2.0` | 111 |
| `MIT` | 36 |
| `Apache-2.0 OR MIT` | 26 |
| `Unicode-3.0` | 18 |
| `MIT/Apache-2.0` | 12 |
| `MPL-2.0` | 9 |
| `MIT OR Apache-2.0 OR Zlib` | 4 |
| `Unlicense OR MIT` | 3 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 2 |
| `Apache-2.0/MIT` | 2 |
| `BSD-3-Clause` | 2 |
| `CDLA-Permissive-2.0` | 2 |
| `ISC` | 2 |
| `Zlib OR Apache-2.0 OR MIT` | 2 |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | 1 |
| `(MIT OR Apache-2.0) AND IJG` | 1 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 |
| `0BSD OR MIT OR Apache-2.0` | 1 |
| `Apache-2.0` | 1 |
| `Apache-2.0 AND ISC` | 1 |
| `Apache-2.0 OR ISC OR MIT` | 1 |
| `BSD-2-Clause OR Apache-2.0 OR MIT` | 1 |
| `GPL-3.0/MIT` | 1 |
| `MIT OR Zlib OR Apache-2.0` | 1 |

**Election.** `unescaper 0.1.8` is offered as `GPL-3.0/MIT`; Plippa takes it under **MIT**. It reaches the build through `linux-embedded-hal` →
`serialport`. No other crate in the closure offers a copyleft option.

## Inventory

| Crate | Version | Licence |
|---|---|---|
| `adler2` | 2.0.1 | `0BSD OR MIT OR Apache-2.0` |
| `ahash` | 0.8.12 | `MIT OR Apache-2.0` |
| `arrayvec` | 0.7.7 | `MIT OR Apache-2.0` |
| `ascii` | 1.1.0 | `Apache-2.0 OR MIT` |
| `async-broadcast` | 0.7.2 | `MIT OR Apache-2.0` |
| `async-channel` | 2.5.0 | `Apache-2.0 OR MIT` |
| `async-executor` | 1.14.0 | `Apache-2.0 OR MIT` |
| `async-io` | 2.6.0 | `Apache-2.0 OR MIT` |
| `async-lock` | 3.4.2 | `Apache-2.0 OR MIT` |
| `async-process` | 2.5.0 | `Apache-2.0 OR MIT` |
| `async-recursion` | 1.2.0 | `MIT OR Apache-2.0` |
| `async-signal` | 0.2.14 | `Apache-2.0 OR MIT` |
| `async-task` | 4.7.1 | `Apache-2.0 OR MIT` |
| `async-trait` | 0.1.92 | `MIT OR Apache-2.0` |
| `atomic-waker` | 1.1.2 | `Apache-2.0 OR MIT` |
| `autocfg` | 1.5.1 | `Apache-2.0 OR MIT` |
| `az` | 1.2.1 | `MIT/Apache-2.0` |
| `base64` | 0.22.1 | `MIT OR Apache-2.0` |
| `bindgen` | 0.69.5 | `BSD-3-Clause` |
| `bitflags` | 1.3.2 | `MIT/Apache-2.0` |
| `bitflags` | 2.13.0 | `MIT OR Apache-2.0` |
| `blocking` | 1.7.0 | `Apache-2.0 OR MIT` |
| `bytemuck` | 1.25.0 | `Zlib OR Apache-2.0 OR MIT` |
| `byteorder` | 1.5.0 | `Unlicense OR MIT` |
| `byteorder-lite` | 0.1.0 | `Unlicense OR MIT` |
| `cast` | 0.3.0 | `MIT OR Apache-2.0` |
| `cc` | 1.2.65 | `MIT OR Apache-2.0` |
| `cexpr` | 0.6.0 | `Apache-2.0/MIT` |
| `cfg-if` | 1.0.4 | `MIT OR Apache-2.0` |
| `cfg_aliases` | 0.2.1 | `MIT` |
| `chunked_transfer` | 1.5.0 | `MIT OR Apache-2.0` |
| `clang-sys` | 1.8.1 | `Apache-2.0` |
| `cobs` | 0.3.0 | `MIT OR Apache-2.0` |
| `concurrent-queue` | 2.5.0 | `Apache-2.0 OR MIT` |
| `crc32fast` | 1.5.0 | `MIT OR Apache-2.0` |
| `crossbeam-utils` | 0.8.23 | `MIT OR Apache-2.0` |
| `ctrlc` | 3.5.2 | `MIT/Apache-2.0` |
| `data-encoding` | 2.11.0 | `MIT` |
| `displaydoc` | 0.2.7 | `MIT OR Apache-2.0` |
| `either` | 1.16.0 | `MIT OR Apache-2.0` |
| `embedded-graphics` | 0.8.2 | `MIT OR Apache-2.0` |
| `embedded-graphics-core` | 0.4.1 | `MIT OR Apache-2.0` |
| `embedded-hal` | 1.0.0 | `MIT OR Apache-2.0` |
| `embedded-hal-nb` | 1.0.0 | `MIT OR Apache-2.0` |
| `encoding_rs` | 0.8.35 | `(Apache-2.0 OR MIT) AND BSD-3-Clause` |
| `endi` | 1.1.1 | `MIT` |
| `enumflags2` | 0.7.12 | `MIT OR Apache-2.0` |
| `enumflags2_derive` | 0.7.12 | `MIT OR Apache-2.0` |
| `equivalent` | 1.0.2 | `Apache-2.0 OR MIT` |
| `errno` | 0.3.14 | `MIT OR Apache-2.0` |
| `event-listener` | 5.4.2 | `Apache-2.0 OR MIT` |
| `event-listener-strategy` | 0.5.4 | `Apache-2.0 OR MIT` |
| `extended` | 0.1.0 | `MIT` |
| `fallible-iterator` | 0.3.0 | `MIT/Apache-2.0` |
| `fallible-streaming-iterator` | 0.1.9 | `MIT/Apache-2.0` |
| `fastrand` | 2.5.0 | `Apache-2.0 OR MIT` |
| `find-msvc-tools` | 0.1.9 | `MIT OR Apache-2.0` |
| `flate2` | 1.1.9 | `MIT OR Apache-2.0` |
| `float-cmp` | 0.9.0 | `MIT` |
| `form_urlencoded` | 1.2.2 | `MIT OR Apache-2.0` |
| `futures-core` | 0.3.34 | `MIT OR Apache-2.0` |
| `futures-io` | 0.3.34 | `MIT OR Apache-2.0` |
| `futures-lite` | 2.6.1 | `Apache-2.0 OR MIT` |
| `getrandom` | 0.2.17 | `MIT OR Apache-2.0` |
| `getrandom` | 0.4.3 | `MIT OR Apache-2.0` |
| `glob` | 0.3.3 | `MIT OR Apache-2.0` |
| `gpio-cdev` | 0.6.0 | `MIT OR Apache-2.0` |
| `hash32` | 0.2.1 | `MIT OR Apache-2.0` |
| `hash32` | 0.3.1 | `MIT OR Apache-2.0` |
| `hashbrown` | 0.14.5 | `MIT OR Apache-2.0` |
| `hashbrown` | 0.17.1 | `MIT OR Apache-2.0` |
| `hashlink` | 0.9.1 | `MIT OR Apache-2.0` |
| `heapless` | 0.7.17 | `MIT OR Apache-2.0` |
| `heapless` | 0.8.0 | `MIT OR Apache-2.0` |
| `hex` | 0.4.3 | `MIT OR Apache-2.0` |
| `httpdate` | 1.0.3 | `MIT OR Apache-2.0` |
| `i2cdev` | 0.6.2 | `MIT/Apache-2.0` |
| `icu_collections` | 2.3.0 | `Unicode-3.0` |
| `icu_locale_core` | 2.3.0 | `Unicode-3.0` |
| `icu_normalizer` | 2.3.0 | `Unicode-3.0` |
| `icu_normalizer_data` | 2.3.0 | `Unicode-3.0` |
| `icu_properties` | 2.3.0 | `Unicode-3.0` |
| `icu_properties_data` | 2.3.0 | `Unicode-3.0` |
| `icu_provider` | 2.3.1 | `Unicode-3.0` |
| `idna` | 1.1.0 | `MIT OR Apache-2.0` |
| `idna_adapter` | 1.2.2 | `Apache-2.0 OR MIT` |
| `image-webp` | 0.2.4 | `MIT OR Apache-2.0` |
| `indexmap` | 2.14.0 | `Apache-2.0 OR MIT` |
| `itertools` | 0.12.1 | `MIT OR Apache-2.0` |
| `itoa` | 1.0.18 | `MIT OR Apache-2.0` |
| `jpeg-encoder` | 0.7.1 | `(MIT OR Apache-2.0) AND IJG` |
| `lazy_static` | 1.5.0 | `MIT OR Apache-2.0` |
| `lazycell` | 1.3.0 | `MIT/Apache-2.0` |
| `libc` | 0.2.186 | `MIT OR Apache-2.0` |
| `libsqlite3-sys` | 0.30.1 | `MIT` |
| `linux-embedded-hal` | 0.4.1 | `MIT OR Apache-2.0` |
| `linux-raw-sys` | 0.12.1 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |
| `litemap` | 0.8.3 | `Unicode-3.0` |
| `lock_api` | 0.4.14 | `MIT OR Apache-2.0` |
| `lofty` | 0.22.4 | `MIT OR Apache-2.0` |
| `lofty_attr` | 0.11.1 | `MIT OR Apache-2.0` |
| `log` | 0.4.33 | `MIT OR Apache-2.0` |
| `memchr` | 2.8.2 | `Unlicense OR MIT` |
| `memoffset` | 0.7.1 | `MIT` |
| `micromath` | 2.1.0 | `Apache-2.0 OR MIT` |
| `minimal-lexical` | 0.2.1 | `MIT/Apache-2.0` |
| `miniz_oxide` | 0.8.9 | `MIT OR Zlib OR Apache-2.0` |
| `mipidsi` | 0.10.0 | `MIT` |
| `nb` | 1.1.0 | `MIT OR Apache-2.0` |
| `nix` | 0.26.4 | `MIT` |
| `nix` | 0.27.1 | `MIT` |
| `nix` | 0.29.0 | `MIT` |
| `nix` | 0.31.3 | `MIT` |
| `nom` | 7.1.3 | `MIT` |
| `num-complex` | 0.4.6 | `MIT OR Apache-2.0` |
| `num-integer` | 0.1.46 | `MIT OR Apache-2.0` |
| `num-traits` | 0.2.19 | `MIT OR Apache-2.0` |
| `ogg_pager` | 0.7.2 | `MIT OR Apache-2.0` |
| `once_cell` | 1.21.4 | `MIT OR Apache-2.0` |
| `ordered-stream` | 0.2.0 | `MIT OR Apache-2.0` |
| `parking` | 2.2.1 | `Apache-2.0 OR MIT` |
| `paste` | 1.0.15 | `MIT OR Apache-2.0` |
| `percent-encoding` | 2.3.2 | `MIT OR Apache-2.0` |
| `pin-project-lite` | 0.2.17 | `Apache-2.0 OR MIT` |
| `pin-utils` | 0.1.0 | `MIT OR Apache-2.0` |
| `piper` | 0.2.5 | `MIT OR Apache-2.0` |
| `pkg-config` | 0.3.33 | `MIT OR Apache-2.0` |
| `pn532` | 0.5.0 | `MIT OR Apache-2.0` |
| `polling` | 3.11.0 | `Apache-2.0 OR MIT` |
| `postcard` | 1.1.3 | `MIT OR Apache-2.0` |
| `potential_utf` | 0.1.6 | `Unicode-3.0` |
| `ppv-lite86` | 0.2.21 | `MIT OR Apache-2.0` |
| `primal-check` | 0.3.4 | `MIT OR Apache-2.0` |
| `proc-macro-crate` | 3.5.0 | `MIT OR Apache-2.0` |
| `proc-macro2` | 1.0.106 | `MIT OR Apache-2.0` |
| `qrcodegen` | 1.8.0 | `MIT` |
| `quick-error` | 2.0.1 | `MIT/Apache-2.0` |
| `quote` | 1.0.46 | `MIT OR Apache-2.0` |
| `rand` | 0.8.6 | `MIT OR Apache-2.0` |
| `rand_chacha` | 0.3.1 | `MIT OR Apache-2.0` |
| `rand_core` | 0.6.4 | `MIT OR Apache-2.0` |
| `realfft` | 3.5.0 | `MIT` |
| `regex` | 1.12.4 | `MIT OR Apache-2.0` |
| `regex-automata` | 0.4.14 | `MIT OR Apache-2.0` |
| `regex-syntax` | 0.8.11 | `MIT OR Apache-2.0` |
| `ring` | 0.17.14 | `Apache-2.0 AND ISC` |
| `rs_ws281x` | 0.5.1 | `MIT` |
| `rubato` | 0.16.2 | `MIT` |
| `rusqlite` | 0.32.1 | `MIT` |
| `rustc-hash` | 1.1.0 | `Apache-2.0/MIT` |
| `rustc_version` | 0.4.1 | `MIT OR Apache-2.0` |
| `rustfft` | 6.4.1 | `MIT OR Apache-2.0` |
| `rustix` | 1.1.4 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |
| `rustls` | 0.23.45 | `Apache-2.0 OR ISC OR MIT` |
| `rustls-pki-types` | 1.15.1 | `MIT OR Apache-2.0` |
| `rustls-webpki` | 0.103.15 | `ISC` |
| `scopeguard` | 1.2.0 | `MIT OR Apache-2.0` |
| `semver` | 1.0.28 | `MIT OR Apache-2.0` |
| `serde` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_core` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_derive` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_json` | 1.0.151 | `MIT OR Apache-2.0` |
| `serde_repr` | 0.1.21 | `MIT OR Apache-2.0` |
| `serde_spanned` | 0.6.9 | `MIT OR Apache-2.0` |
| `serialport` | 4.9.0 | `MPL-2.0` |
| `shlex` | 1.3.0 | `MIT OR Apache-2.0` |
| `shlex` | 2.0.1 | `MIT OR Apache-2.0` |
| `signal-hook-registry` | 1.4.8 | `MIT OR Apache-2.0` |
| `signalsmith-stretch` | 0.1.3 | `MIT` |
| `simd-adler32` | 0.3.10 | `MIT` |
| `slab` | 0.4.12 | `MIT` |
| `smallvec` | 1.15.2 | `MIT OR Apache-2.0` |
| `spidev` | 0.6.1 | `MIT/Apache-2.0` |
| `spin` | 0.9.8 | `MIT` |
| `stable_deref_trait` | 1.2.1 | `MIT OR Apache-2.0` |
| `strength_reduce` | 0.2.4 | `MIT OR Apache-2.0` |
| `subtle` | 2.6.1 | `BSD-3-Clause` |
| `symphonia` | 0.5.5 | `MPL-2.0` |
| `symphonia-bundle-flac` | 0.5.5 | `MPL-2.0` |
| `symphonia-bundle-mp3` | 0.5.5 | `MPL-2.0` |
| `symphonia-codec-pcm` | 0.5.5 | `MPL-2.0` |
| `symphonia-core` | 0.5.5 | `MPL-2.0` |
| `symphonia-format-riff` | 0.5.5 | `MPL-2.0` |
| `symphonia-metadata` | 0.5.5 | `MPL-2.0` |
| `symphonia-utils-xiph` | 0.5.5 | `MPL-2.0` |
| `syn` | 2.0.118 | `MIT OR Apache-2.0` |
| `syn` | 3.0.2 | `MIT OR Apache-2.0` |
| `synstructure` | 0.14.0 | `MIT` |
| `tempfile` | 3.27.0 | `MIT OR Apache-2.0` |
| `thiserror` | 2.0.18 | `MIT OR Apache-2.0` |
| `thiserror-impl` | 2.0.18 | `MIT OR Apache-2.0` |
| `tiny_http` | 0.12.0 | `MIT OR Apache-2.0` |
| `tinystr` | 0.8.4 | `Unicode-3.0` |
| `tinyvec` | 1.13.3 | `Zlib OR Apache-2.0 OR MIT` |
| `toml` | 0.8.23 | `MIT OR Apache-2.0` |
| `toml_datetime` | 0.6.11 | `MIT OR Apache-2.0` |
| `toml_datetime` | 1.1.2+spec-1.1.0 | `MIT OR Apache-2.0` |
| `toml_edit` | 0.22.27 | `MIT OR Apache-2.0` |
| `toml_edit` | 0.25.17+spec-1.1.0 | `MIT OR Apache-2.0` |
| `toml_parser` | 1.1.5+spec-1.1.0 | `MIT OR Apache-2.0` |
| `toml_write` | 0.1.2 | `MIT OR Apache-2.0` |
| `tracing` | 0.1.44 | `MIT` |
| `tracing-attributes` | 0.1.31 | `MIT` |
| `tracing-core` | 0.1.36 | `MIT` |
| `transpose` | 0.2.3 | `MIT OR Apache-2.0` |
| `unescaper` | 0.1.8 | `GPL-3.0/MIT` |
| `unicode-ident` | 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` |
| `unicode-normalization` | 0.1.25 | `MIT OR Apache-2.0` |
| `untrusted` | 0.9.0 | `ISC` |
| `ureq` | 2.12.1 | `MIT OR Apache-2.0` |
| `url` | 2.5.8 | `MIT OR Apache-2.0` |
| `utf8_iter` | 1.0.4 | `Apache-2.0 OR MIT` |
| `uuid` | 1.29.0 | `Apache-2.0 OR MIT` |
| `vcpkg` | 0.2.15 | `MIT/Apache-2.0` |
| `version_check` | 0.9.5 | `MIT/Apache-2.0` |
| `webpki-roots` | 0.26.11 | `CDLA-Permissive-2.0` |
| `webpki-roots` | 1.0.9 | `CDLA-Permissive-2.0` |
| `winnow` | 0.7.15 | `MIT` |
| `winnow` | 1.0.4 | `MIT` |
| `writeable` | 0.6.4 | `Unicode-3.0` |
| `yoke` | 0.8.3 | `Unicode-3.0` |
| `yoke-derive` | 0.8.3 | `Unicode-3.0` |
| `zbus` | 5.19.0 | `MIT` |
| `zbus_macros` | 5.19.0 | `MIT` |
| `zbus_names` | 4.3.5 | `MIT` |
| `zcheapstr` | 1.1.0 | `MIT` |
| `zerocopy` | 0.8.52 | `BSD-2-Clause OR Apache-2.0 OR MIT` |
| `zerofrom` | 0.1.8 | `Unicode-3.0` |
| `zerofrom-derive` | 0.1.8 | `Unicode-3.0` |
| `zeroize` | 1.9.0 | `Apache-2.0 OR MIT` |
| `zerotrie` | 0.2.5 | `Unicode-3.0` |
| `zerovec` | 0.11.8 | `Unicode-3.0` |
| `zerovec-derive` | 0.11.6 | `Unicode-3.0` |
| `zmij` | 1.0.23 | `MIT` |
| `zune-core` | 0.5.3 | `MIT OR Apache-2.0 OR Zlib` |
| `zune-inflate` | 0.2.54 | `MIT OR Apache-2.0 OR Zlib` |
| `zune-jpeg` | 0.5.15 | `MIT OR Apache-2.0 OR Zlib` |
| `zune-png` | 0.5.2 | `MIT OR Apache-2.0 OR Zlib` |
| `zvariant` | 5.15.1 | `MIT` |
| `zvariant_derive` | 5.15.1 | `MIT` |
| `zvariant_utils` | 4.2.0 | `MIT` |

## Attribution notices

The copyright lines each licence requires to travel with the binary, copied from the licence files shipped in the published crate (from `third_party/` for the two vendored ones). "Carries no copyright line" means exactly that: the published file is the bare permission text, so there is no line to reproduce — not that no one holds the copyright.

- **adler2 2.0.1** — Copyright (C) Jonas Schievink <jonasschievink@gmail.com> — https://github.com/oyvindln/adler2
- **ahash 0.8.12** — Copyright (c) 2018 Tom Kaitchuck — https://github.com/tkaitchuck/ahash
- **arrayvec 0.7.7** — Copyright (c) Ulrik Sverdrup "bluss" 2015-2023 — https://github.com/bluss/arrayvec
- **ascii 1.1.0** — Copyright (c) 2017 Thomas Bahn and contributors; Copyright (c) 2014 The Rust Project Developers — https://github.com/tomprogrammer/rust-ascii
- **async-broadcast 0.7.2** — Copyright 2020 Yoshua Wuyts; Copyright (c) 2020 Yoshua Wuyts — https://github.com/smol-rs/async-broadcast
- **async-channel 2.5.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-channel
- **async-executor 1.14.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-executor
- **async-io 2.6.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-io
- **async-lock 3.4.2** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-lock
- **async-process 2.5.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-process
- **async-recursion 1.2.0** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dcchut/async-recursion
- **async-signal 0.2.14** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-signal
- **async-task 4.7.1** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/async-task
- **async-trait 0.1.92** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/async-trait
- **atomic-waker 1.1.2** — Copyright (c) 2016 Alex Crichton; Copyright (c) 2017 The Tokio Authors — https://github.com/smol-rs/atomic-waker
- **autocfg 1.5.1** — Copyright (c) 2018 Josh Stone — https://github.com/cuviper/autocfg
- **az 1.2.1** — Copyright © 2019–2021 Trevor Spiteri — https://gitlab.com/tspiteri/az
- **base64 0.22.1** — Copyright (c) 2015 Alice Maz — https://github.com/marshallpierce/rust-base64
- **bindgen 0.69.5** — Copyright (c) 2013, Jyun-Yan You — https://github.com/rust-lang/rust-bindgen
- **bitflags 1.3.2** — Copyright (c) 2014 The Rust Project Developers — https://github.com/bitflags/bitflags
- **bitflags 2.13.0** — Copyright (c) 2014 The Rust Project Developers — https://github.com/bitflags/bitflags
- **blocking 1.7.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/blocking
- **bytemuck 1.25.0** — Copyright (c) 2019 Daniel "Lokathor" Gee. — https://github.com/Lokathor/bytemuck
- **byteorder 1.5.0** — Copyright (c) 2015 Andrew Gallant — https://github.com/BurntSushi/byteorder
- **byteorder-lite 0.1.0** — Copyright (c) 2015 Andrew Gallant — https://github.com/image-rs/byteorder-lite
- **cast 0.3.0** — Copyright (c) 2014-2017 Jorge Aparicio — https://github.com/japaric/cast.rs
- **cc 1.2.65** — Copyright (c) 2014 Alex Crichton — https://github.com/rust-lang/cc-rs
- **cexpr 0.6.0** — (C) Copyright 2016 Jethro G. Beekman — https://github.com/jethrogb/rust-cexpr
- **cfg-if 1.0.4** — Copyright (c) 2014 Alex Crichton — https://github.com/rust-lang/cfg-if
- **cfg_aliases 0.2.1** — Copyright (c) 2020 Katharos Technology — https://github.com/katharostech/cfg_aliases
- **chunked_transfer 1.5.0** — Copyright (c) 2015 The tiny-http Contributors; Copyright (c) 2015 The rust-chunked-transfer Contributors — https://github.com/frewsxcv/rust-chunked-transfer
- **clang-sys 1.8.1** — `Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/KyleMayes/clang-sys
- **cobs 0.3.0** — Copyright (c) 2015 The cobs.rs Developers — https://github.com/jamesmunns/cobs.rs
- **concurrent-queue 2.5.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/concurrent-queue
- **crc32fast 1.5.0** — Copyright (c) 2018 Sam Rijs, Alex Crichton and contributors — https://github.com/srijs/rust-crc32fast
- **crossbeam-utils 0.8.23** — Copyright (c) 2019 The Crossbeam Project Developers — https://github.com/crossbeam-rs/crossbeam
- **ctrlc 3.5.2** — Copyright 2017 CtrlC developers — https://github.com/Detegr/rust-ctrlc.git
- **data-encoding 2.11.0** — Copyright (c) 2015-2020 Julien Cretin; Copyright (c) 2017-2020 Google Inc. — https://github.com/ia0/data-encoding
- **displaydoc 0.2.7** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/yaahc/displaydoc
- **either 1.16.0** — Copyright (c) 2015 — https://github.com/rayon-rs/either
- **embedded-graphics 0.8.2** — Copyright (c) 2020 James Waples — https://github.com/embedded-graphics/embedded-graphics
- **embedded-graphics-core 0.4.1** — `MIT OR Apache-2.0`; the published crate ships no licence file and no copyright line, so there is none to reproduce — https://github.com/embedded-graphics/embedded-graphics
- **embedded-hal 1.0.0** — Copyright (c) 2017-2018 Jorge Aparicio — https://github.com/rust-embedded/embedded-hal
- **embedded-hal-nb 1.0.0** — Copyright (c) 2021-2022 The Rust embedded HAL team and contributors. — https://github.com/rust-embedded/embedded-hal
- **encoding_rs 0.8.35** — Copyright Mozilla Foundation; Copyright © WHATWG (Apple, Google, Mozilla, Microsoft). — https://github.com/hsivonen/encoding_rs
- **endi 1.1.1** — `MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/zeenix/endi
- **enumflags2 0.7.12** — Copyright 2017-2023 Maik Klein, Maja Kądziołka; Copyright (c) 2017-2023 Maik Klein, Maja Kądziołka — https://github.com/meithecatte/enumflags2
- **enumflags2_derive 0.7.12** — Copyright [2017] [Maik Klein]; Copyright (c) 2017 Maik Klein — https://github.com/meithecatte/enumflags2
- **equivalent 1.0.2** — Copyright (c) 2016--2023 — https://github.com/indexmap-rs/equivalent
- **errno 0.3.14** — Copyright (c) 2014 Chris Wong — https://github.com/lambda-fairy/rust-errno
- **event-listener 5.4.2** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/event-listener
- **event-listener-strategy 0.5.4** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/event-listener-strategy
- **extended 0.1.0** — Copyright 2022 Dietrich Epp — https://github.com/depp/extended-rs
- **fallible-iterator 0.3.0** — Copyright (c) 2015 The rust-openssl-verify Developers — https://github.com/sfackler/rust-fallible-iterator
- **fallible-streaming-iterator 0.1.9** — Copyright (c) 2016 The fallible-streaming-iterator Developers — https://github.com/sfackler/fallible-streaming-iterator
- **fastrand 2.5.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/fastrand
- **find-msvc-tools 0.1.9** — Copyright (c) 2014 Alex Crichton — https://github.com/rust-lang/cc-rs
- **flate2 1.1.9** — Copyright (c) 2014-2026 Alex Crichton — https://github.com/rust-lang/flate2-rs
- **float-cmp 0.9.0** — Copyright (c) 2014-2020 Optimal Computing (NZ) Ltd — https://github.com/mikedilger/float-cmp
- **form_urlencoded 1.2.2** — Copyright (c) 2013-2016 The rust-url developers — https://github.com/servo/rust-url
- **futures-core 0.3.34** — Copyright (c) 2016 Alex Crichton; Copyright (c) 2017 The Tokio Authors — https://github.com/rust-lang/futures-rs
- **futures-io 0.3.34** — Copyright (c) 2016 Alex Crichton; Copyright (c) 2017 The Tokio Authors — https://github.com/rust-lang/futures-rs
- **futures-lite 2.6.1** — Copyright (c) 2016 Alex Crichton; Copyright (c) 2017 The Tokio Authors — https://github.com/smol-rs/futures-lite
- **getrandom 0.2.17** — Copyright (c) 2018-2024 The rust-random Project Developers; Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-random/getrandom
- **getrandom 0.4.3** — Copyright (c) 2018-2026 The rust-random Project Developers; Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-random/getrandom
- **glob 0.3.3** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-lang/glob
- **gpio-cdev 0.6.0** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/rust-embedded/gpio-cdev
- **hash32 0.2.1** — Copyright (c) 2018 Jorge Aparicio — https://github.com/japaric/hash32
- **hash32 0.3.1** — Copyright (c) 2018 Jorge Aparicio — https://github.com/japaric/hash32
- **hashbrown 0.14.5** — Copyright (c) 2016 Amanieu d'Antras — https://github.com/rust-lang/hashbrown
- **hashbrown 0.17.1** — Copyright (c) 2016 Amanieu d'Antras — https://github.com/rust-lang/hashbrown
- **hashlink 0.9.1** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/kyren/hashlink
- **heapless 0.7.17** — Copyright (c) 2017 Jorge Aparicio — https://github.com/japaric/heapless
- **heapless 0.8.0** — Copyright (c) 2017 Jorge Aparicio — https://github.com/rust-embedded/heapless
- **hex 0.4.3** — Copyright (c) 2013-2014 The Rust Project Developers.; Copyright (c) 2015-2020 The rust-hex Developers — https://github.com/KokaKiwi/rust-hex
- **httpdate 1.0.3** — Copyright (c) 2016 Pyfisch — https://github.com/pyfisch/httpdate
- **i2cdev 0.6.2** — Copyright (c) 2015 Paul Osborne; Copyright (c) 2021-2025 The Rust Embedded Linux Team and contributors. — https://github.com/rust-embedded/rust-i2cdev
- **icu_collections 2.3.0** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **icu_locale_core 2.3.0** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **icu_normalizer 2.3.0** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **icu_normalizer_data 2.3.0** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **icu_properties 2.3.0** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **icu_properties_data 2.3.0** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **icu_provider 2.3.1** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **idna 1.1.0** — Copyright (c) 2013-2025 The rust-url developers — https://github.com/servo/rust-url/
- **idna_adapter 1.2.2** — Copyright (c) The rust-url developers — https://github.com/hsivonen/idna_adapter
- **image-webp 0.2.4** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/image-rs/image-webp
- **indexmap 2.14.0** — Copyright (c) 2016--2017 — https://github.com/indexmap-rs/indexmap
- **itertools 0.12.1** — Copyright (c) 2015 — https://github.com/rust-itertools/itertools
- **itoa 1.0.18** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/itoa
- **jpeg-encoder 0.7.1** — Copyright (c) 2021 Volker Ströbel <volkerstroebel@mysurdity.de> — https://github.com/vstroebel/jpeg-encoder
- **lazy_static 1.5.0** — Copyright (c) 2010 The Rust Project Developers — https://github.com/rust-lang-nursery/lazy-static.rs
- **lazycell 1.3.0** — `MIT/Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/indiv0/lazycell
- **libc 0.2.186** — Copyright (c) The Rust Project Developers — https://github.com/rust-lang/libc
- **libsqlite3-sys 0.30.1** — Copyright (c) 2014-2021 The rusqlite developers — https://github.com/rusqlite/rusqlite
- **linux-embedded-hal 0.4.1** — Copyright (c) 2018 Jorge Aparicio; Copyright (c) 2019-2024 The Rust embedded linux team and contributors. — https://github.com/rust-embedded/linux-embedded-hal
- **linux-raw-sys 0.12.1** — `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/sunfishcode/linux-raw-sys
- **litemap 0.8.3** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **lock_api 0.4.14** — Copyright (c) 2016 The Rust Project Developers — https://github.com/Amanieu/parking_lot
- **lofty 0.22.4** — `MIT OR Apache-2.0`; the published crate ships no licence file and no copyright line, so there is none to reproduce — https://github.com/Serial-ATA/lofty-rs
- **lofty_attr 0.11.1** — `MIT OR Apache-2.0`; the published crate ships no licence file and no copyright line, so there is none to reproduce — https://github.com/Serial-ATA/lofty-rs
- **log 0.4.33** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-lang/log
- **memchr 2.8.2** — Copyright (c) 2015 Andrew Gallant — https://github.com/BurntSushi/memchr
- **memoffset 0.7.1** — Copyright (c) 2017 Gilad Naaman — https://github.com/Gilnaa/memoffset
- **micromath 2.1.0** — Copyright (c) 2019-2021 Tony Arcieri — https://github.com/tarcieri/micromath
- **minimal-lexical 0.2.1** — Copyright (c) 2009 The Go Authors. All rights reserved. — https://github.com/Alexhuszagh/minimal-lexical
- **miniz_oxide 0.8.9** — Copyright 2013-2014 RAD Game Tools and Valve Software; Copyright 2010-2014 Rich Geldreich and Tenacious Software LLC; Copyright (c) 2017 Frommi; Copyright (c) 2017-2024 oyvindln; Copyright (c) 2020 Frommi — https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide
- **mipidsi 0.10.0** — Copyright 2020 Aleš Katona. — https://github.com/almindor/mipidsi
- **nb 1.1.0** — Copyright (c) 2017 Jorge Aparicio — https://github.com/rust-embedded/nb
- **nix 0.26.4** — Copyright (c) 2015 Carl Lerche + nix-rust Authors — https://github.com/nix-rust/nix
- **nix 0.27.1** — Copyright (c) 2015 Carl Lerche + nix-rust Authors — https://github.com/nix-rust/nix
- **nix 0.29.0** — Copyright (c) 2015 Carl Lerche + nix-rust Authors — https://github.com/nix-rust/nix
- **nix 0.31.3** — Copyright (c) 2015 Carl Lerche + nix-rust Authors — https://github.com/nix-rust/nix
- **nom 7.1.3** — Copyright (c) 2014-2019 Geoffroy Couprie — https://github.com/Geal/nom
- **num-complex 0.4.6** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-num/num-complex
- **num-integer 0.1.46** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-num/num-integer
- **num-traits 0.2.19** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-num/num-traits
- **ogg_pager 0.7.2** — `MIT OR Apache-2.0`; the published crate ships no licence file and no copyright line, so there is none to reproduce — https://github.com/Serial-ATA/lofty-rs
- **once_cell 1.21.4** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/matklad/once_cell
- **ordered-stream 0.2.0** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/danieldg/ordered-stream
- **parking 2.2.1** — Copyright 2014-2020 The Rust Project Developers — https://github.com/smol-rs/parking
- **paste 1.0.15** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/paste
- **percent-encoding 2.3.2** — Copyright (c) 2013-2025 The rust-url developers — https://github.com/servo/rust-url/
- **pin-project-lite 0.2.17** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/taiki-e/pin-project-lite
- **pin-utils 0.1.0** — Copyright 2018 The pin-utils authors; Copyright (c) 2018 The pin-utils authors — https://github.com/rust-lang-nursery/pin-utils
- **piper 0.2.5** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/piper
- **pkg-config 0.3.33** — Copyright (c) 2014 Alex Crichton — https://github.com/rust-lang/pkg-config-rs
- **pn532 0.5.0** — Copyright 2021 WMT GmbH; Copyright (c) 2021 WMT GmbH — https://github.com/WMT-GmbH/pn532
- **polling 3.11.0** — `Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/smol-rs/polling
- **postcard 1.1.3** — Copyright (c) 2019 Anthony James Munns — https://github.com/jamesmunns/postcard
- **potential_utf 0.1.6** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **ppv-lite86 0.2.21** — Copyright 2019 The CryptoCorrosion Contributors; Copyright (c) 2019 The CryptoCorrosion Contributors — https://github.com/cryptocorrosion/cryptocorrosion
- **primal-check 0.3.4** — Copyright (c) 2014 Huon Wilson — https://github.com/huonw/primal
- **proc-macro-crate 3.5.0** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/bkchr/proc-macro-crate
- **proc-macro2 1.0.106** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/proc-macro2
- **qrcodegen 1.8.0** — `MIT`; the published crate ships no licence file and no copyright line, so there is none to reproduce — https://github.com/nayuki/QR-Code-generator
- **quick-error 2.0.1** — Copyright (c) 2015 The quick-error Developers — http://github.com/tailhook/quick-error
- **quote 1.0.46** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/quote
- **rand 0.8.6** — Copyright 2018 Developers of the Rand project; Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-random/rand
- **rand_chacha 0.3.1** — Copyright 2018 Developers of the Rand project; Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-random/rand
- **rand_core 0.6.4** — Copyright 2018 Developers of the Rand project; Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-random/rand
- **realfft 3.5.0** — `MIT`; the published crate ships no licence file and no copyright line, so there is none to reproduce — https://github.com/HEnquist/realfft
- **regex 1.12.4** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-lang/regex
- **regex-automata 0.4.14** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-lang/regex
- **regex-syntax 0.8.11** — Copyright (c) 2014 The Rust Project Developers — https://github.com/rust-lang/regex
- **ring 0.17.14** — Copyright (c) 2009 The Go Authors. All rights reserved.; Copyright 2015 The Chromium Authors. All rights reserved.; Copyright 2015-2025 Brian Smith. — https://github.com/briansmith/ring
- **rs_ws281x 0.5.1** — Copyright 2018, Joseph Murphy — https://github.com/rpi-ws281x/rpi-ws281x-rust
- **rubato 0.16.2** — Copyright (c) 2020 Henrik Enquist — https://github.com/HEnquist/rubato
- **rusqlite 0.32.1** — Copyright (c) 2014-2021 The rusqlite developers — https://github.com/rusqlite/rusqlite
- **rustc-hash 1.1.0** — `Apache-2.0/MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/rust-lang-nursery/rustc-hash
- **rustc_version 0.4.1** — Copyright (c) 2016 The Rust Project Developers — https://github.com/djc/rustc-version-rs
- **rustfft 6.4.1** — Copyright (c) 2015 The RustFFT Developers — https://github.com/ejmahler/RustFFT
- **rustix 1.1.4** — `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/bytecodealliance/rustix
- **rustls 0.23.45** — Copyright (c) 2016, Joseph Birr-Pixton <jpixton@gmail.com>; Copyright (c) 2016 Joseph Birr-Pixton <jpixton@gmail.com> — https://github.com/rustls/rustls
- **rustls-pki-types 1.15.1** — Copyright 2023 Dirkjan Ochtman; Copyright (c) 2023 Dirkjan Ochtman <dirkjan@ochtman.nl> — https://github.com/rustls/pki-types
- **rustls-webpki 0.103.15** — Copyright 2015 Brian Smith. — https://github.com/rustls/webpki
- **scopeguard 1.2.0** — Copyright (c) 2016-2019 Ulrik Sverdrup "bluss" and scopeguard developers — https://github.com/bluss/scopeguard
- **semver 1.0.28** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/semver
- **serde 1.0.229** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/serde-rs/serde
- **serde_core 1.0.229** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/serde-rs/serde
- **serde_derive 1.0.229** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/serde-rs/serde
- **serde_json 1.0.151** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/serde-rs/json
- **serde_repr 0.1.21** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/serde-repr
- **serde_spanned 0.6.9** — Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **serialport 4.9.0** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/serialport/serialport-rs
- **shlex 1.3.0** — Copyright 2015 Nicholas Allegra (comex).; Copyright (c) 2015 Nicholas Allegra (comex). — https://github.com/comex/rust-shlex
- **shlex 2.0.1** — Copyright 2015 Nicholas Allegra (comex).; Copyright (c) 2015 Nicholas Allegra (comex). — https://github.com/comex/rust-shlex
- **signal-hook-registry 1.4.8** — Copyright (c) 2017 tokio-jsonrpc developers — https://github.com/vorner/signal-hook
- **signalsmith-stretch 0.1.3** — Copyright 2024 Colin Marc <hi@colinmarc.com>
- **simd-adler32 0.3.10** — Copyright (c) [2021] [Marvin Countryman] — https://github.com/mcountryman/simd-adler32
- **slab 0.4.12** — Copyright (c) 2019 Carl Lerche — https://github.com/tokio-rs/slab
- **smallvec 1.15.2** — Copyright (c) 2018 The Servo Project Developers — https://github.com/servo/rust-smallvec
- **spidev 0.6.1** — Copyright (c) 2015 Paul Osborne; Copyright (c) 2021-2023 The Rust Embedded Linux Team and contributors. — https://github.com/rust-embedded/rust-spidev
- **spin 0.9.8** — Copyright (c) 2014 Mathijs van de Nes — https://github.com/mvdnes/spin-rs.git
- **stable_deref_trait 1.2.1** — Copyright (c) 2017 Robert Grosse — https://github.com/storyyeller/stable_deref_trait
- **strength_reduce 0.2.4** — Copyright (c) 2015 The RustFFT Developers — http://github.com/ejmahler/strength_reduce
- **subtle 2.6.1** — Copyright (c) 2016-2017 Isis Agora Lovecruft, Henry de Valence. All rights reserved.; Copyright (c) 2016-2024 Isis Agora Lovecruft. All rights reserved. — https://github.com/dalek-cryptography/subtle
- **symphonia 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-bundle-flac 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-bundle-mp3 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-codec-pcm 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-core 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-format-riff 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-metadata 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **symphonia-utils-xiph 0.5.5** — `MPL-2.0`; the shipped licence file carries no copyright line (the MPL keeps the notice in each source file, and these are unmodified — see the MPL-2.0 section) — https://github.com/pdeljanov/Symphonia
- **syn 2.0.118** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/syn
- **syn 3.0.2** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/syn
- **synstructure 0.14.0** — Copyright 2016 Nika Layzell — https://github.com/mystor/synstructure
- **tempfile 3.27.0** — Copyright (c) 2015 Steven Allen — https://github.com/Stebalien/tempfile
- **thiserror 2.0.18** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/thiserror
- **thiserror-impl 2.0.18** — `MIT OR Apache-2.0`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/thiserror
- **tiny_http 0.12.0** — Copyright (c) 2014-2019 The tiny-http contributors — https://github.com/tiny-http/tiny-http
- **tinystr 0.8.4** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **tinyvec 1.13.3** — Copyright (c) 2019 Daniel "Lokathor" Gee. — https://github.com/Lokathor/tinyvec
- **toml 0.8.23** — Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **toml_datetime 0.6.11** — Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **toml_datetime 1.1.2+spec-1.1.0** — Copyright Individual contributors; Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **toml_edit 0.22.27** — Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **toml_edit 0.25.17+spec-1.1.0** — Copyright Individual contributors; Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **toml_parser 1.1.5+spec-1.1.0** — Copyright Individual contributors; Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **toml_write 0.1.2** — Copyright (c) Individual contributors — https://github.com/toml-rs/toml
- **tracing 0.1.44** — Copyright (c) 2019 Tokio Contributors — https://github.com/tokio-rs/tracing
- **tracing-attributes 0.1.31** — Copyright (c) 2019 Tokio Contributors — https://github.com/tokio-rs/tracing
- **tracing-core 0.1.36** — Copyright (c) 2019 Tokio Contributors — https://github.com/tokio-rs/tracing
- **transpose 0.2.3** — Copyright 2022 The transpose developers; Copyright (c) 2022 The transpose Developers — https://github.com/ejmahler/transpose
- **unescaper 0.1.8** — Copyright (C) 2007 Free Software Foundation, Inc. <http://fsf.org/>; copyright on the Program, and are irrevocable provided the stated; Copyright (C) {year} {name of author}; Copyright (c) 2023 Hack Ink — https://github.com/hack-ink/unescaper
- **unicode-ident 1.0.24** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 1991-2023 Unicode, Inc. — https://github.com/dtolnay/unicode-ident
- **unicode-normalization 0.1.25** — Copyright (c) 2015 The Rust Project Developers — https://github.com/unicode-rs/unicode-normalization
- **untrusted 0.9.0** — Copyright 2015-2016 Brian Smith. — https://github.com/briansmith/untrusted
- **ureq 2.12.1** — Copyright (c) 2019 Martin Algesten — https://github.com/algesten/ureq
- **url 2.5.8** — Copyright (c) 2013-2025 The rust-url developers — https://github.com/servo/rust-url
- **utf8_iter 1.0.4** — Copyright Mozilla Foundation — https://github.com/hsivonen/utf8_iter
- **uuid 1.29.0** — Copyright (c) 2014 The Rust Project Developers; Copyright (c) 2018 Ashley Mannix, Christopher Armstrong, Dylan DPC, Hunar Roop Kahlon — https://github.com/uuid-rs/uuid
- **vcpkg 0.2.15** — Copyright (c) 2017 Jim McGrath — https://github.com/mcgoo/vcpkg-rs
- **version_check 0.9.5** — Copyright (c) 2017-2018 Sergio Benitez — https://github.com/SergioBenitez/version_check
- **webpki-roots 0.26.11** — `CDLA-Permissive-2.0` data, no copyright line; the agreement text is reproduced below and the data is unmodified — https://github.com/rustls/webpki-roots
- **webpki-roots 1.0.9** — `CDLA-Permissive-2.0` data, no copyright line; the agreement text is reproduced below and the data is unmodified — https://github.com/rustls/webpki-roots
- **winnow 0.7.15** — `MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/winnow-rs/winnow
- **winnow 1.0.4** — `MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/winnow-rs/winnow
- **writeable 0.6.4** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **yoke 0.8.3** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **yoke-derive 0.8.3** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **zbus 5.19.0** — Copyright (c) 2024 Zeeshan Ali Khan & zbus contributors — https://github.com/z-galaxy/zbus/
- **zbus_macros 5.19.0** — Copyright (c) 2024 Zeeshan Ali Khan & zbus contributors — https://github.com/z-galaxy/zbus/
- **zbus_names 4.3.5** — Copyright (c) 2024 Zeeshan Ali Khan & zbus contributors — https://github.com/z-galaxy/zbus/
- **zcheapstr 1.1.0** — Copyright (c) 2026 Zeeshan Ali Khan & zcheapstr contributors — https://github.com/z-galaxy/zcheapstr/
- **zerocopy 0.8.52** — Copyright 2023 The Fuchsia Authors; Copyright 2019 The Fuchsia Authors. — https://github.com/google/zerocopy
- **zerofrom 0.1.8** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **zerofrom-derive 0.1.8** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **zeroize 1.9.0** — Copyright (c) 2018-2026 The RustCrypto Project Developers — https://github.com/RustCrypto/utils
- **zerotrie 0.2.5** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **zerovec 0.11.8** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **zerovec-derive 0.11.6** — COPYRIGHT AND PERMISSION NOTICE; Copyright © 2020-2024 Unicode, Inc. — https://github.com/unicode-org/icu4x
- **zmij 1.0.23** — `MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/dtolnay/zmij
- **zune-core 0.5.3** — Copyright (c) zune-image developers — https://github.com/etemesi254/zune-image
- **zune-inflate 0.2.54** — `MIT OR Apache-2.0 OR Zlib`; the published crate ships no licence file and no copyright line, so there is none to reproduce
- **zune-jpeg 0.5.15** — Copyright (c) zune-image developers — https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg
- **zune-png 0.5.2** — Copyright (c) zune-image developers — https://github.com/etemesi254/zune-image/tree/dev/zune-png
- **zvariant 5.15.1** — Copyright (c) 2024 Zeeshan Ali Khan & zbus contributors — https://github.com/z-galaxy/zbus/
- **zvariant_derive 5.15.1** — Copyright (c) 2024 Zeeshan Ali Khan & zbus contributors — https://github.com/z-galaxy/zbus/
- **zvariant_utils 4.2.0** — `MIT`; the published licence file carries no copyright line, so there is none to reproduce — https://github.com/z-galaxy/zbus/

## Licence texts

The permission and warranty terms of every licence in use, so a distributed binary carries
the notices it must. The copyright line that belongs with each text is in the attribution list
above (one line per crate, taken from that crate's own licence file). The two texts already
quoted in full on the product page — `rpi_ws281x` (BSD-2-Clause) and Signalsmith Stretch
(MIT) — are not repeated here.

### MIT

```
Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

### Apache-2.0

```
Apache License
                        Version 2.0, January 2004
                     http://www.apache.org/licenses/

TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

1. Definitions.

   "License" shall mean the terms and conditions for use, reproduction,
   and distribution as defined by Sections 1 through 9 of this document.

   "Licensor" shall mean the copyright owner or entity authorized by
   the copyright owner that is granting the License.

   "Legal Entity" shall mean the union of the acting entity and all
   other entities that control, are controlled by, or are under common
   control with that entity. For the purposes of this definition,
   "control" means (i) the power, direct or indirect, to cause the
   direction or management of such entity, whether by contract or
   otherwise, or (ii) ownership of fifty percent (50%) or more of the
   outstanding shares, or (iii) beneficial ownership of such entity.

   "You" (or "Your") shall mean an individual or Legal Entity
   exercising permissions granted by this License.

   "Source" form shall mean the preferred form for making modifications,
   including but not limited to software source code, documentation
   source, and configuration files.

   "Object" form shall mean any form resulting from mechanical
   transformation or translation of a Source form, including but
   not limited to compiled object code, generated documentation,
   and conversions to other media types.

   "Work" shall mean the work of authorship, whether in Source or
   Object form, made available under the License, as indicated by a
   copyright notice that is included in or attached to the work
   (an example is provided in the Appendix below).

   "Derivative Works" shall mean any work, whether in Source or Object
   form, that is based on (or derived from) the Work and for which the
   editorial revisions, annotations, elaborations, or other modifications
   represent, as a whole, an original work of authorship. For the purposes
   of this License, Derivative Works shall not include works that remain
   separable from, or merely link (or bind by name) to the interfaces of,
   the Work and Derivative Works thereof.

   "Contribution" shall mean any work of authorship, including
   the original version of the Work and any modifications or additions
   to that Work or Derivative Works thereof, that is intentionally
   submitted to Licensor for inclusion in the Work by the copyright owner
   or by an individual or Legal Entity authorized to submit on behalf of
   the copyright owner. For the purposes of this definition, "submitted"
   means any form of electronic, verbal, or written communication sent
   to the Licensor or its representatives, including but not limited to
   communication on electronic mailing lists, source code control systems,
   and issue tracking systems that are managed by, or on behalf of, the
   Licensor for the purpose of discussing and improving the Work, but
   excluding communication that is conspicuously marked or otherwise
   designated in writing by the copyright owner as "Not a Contribution."

   "Contributor" shall mean Licensor and any individual or Legal Entity
   on behalf of whom a Contribution has been received by Licensor and
   subsequently incorporated within the Work.

2. Grant of Copyright License. Subject to the terms and conditions of
   this License, each Contributor hereby grants to You a perpetual,
   worldwide, non-exclusive, no-charge, royalty-free, irrevocable
   copyright license to reproduce, prepare Derivative Works of,
   publicly display, publicly perform, sublicense, and distribute the
   Work and such Derivative Works in Source or Object form.

3. Grant of Patent License. Subject to the terms and conditions of
   this License, each Contributor hereby grants to You a perpetual,
   worldwide, non-exclusive, no-charge, royalty-free, irrevocable
   (except as stated in this section) patent license to make, have made,
   use, offer to sell, sell, import, and otherwise transfer the Work,
   where such license applies only to those patent claims licensable
   by such Contributor that are necessarily infringed by their
   Contribution(s) alone or by combination of their Contribution(s)
   with the Work to which such Contribution(s) was submitted. If You
   institute patent litigation against any entity (including a
   cross-claim or counterclaim in a lawsuit) alleging that the Work
   or a Contribution incorporated within the Work constitutes direct
   or contributory patent infringement, then any patent licenses
   granted to You under this License for that Work shall terminate
   as of the date such litigation is filed.

4. Redistribution. You may reproduce and distribute copies of the
   Work or Derivative Works thereof in any medium, with or without
   modifications, and in Source or Object form, provided that You
   meet the following conditions:

   (a) You must give any other recipients of the Work or
       Derivative Works a copy of this License; and

   (b) You must cause any modified files to carry prominent notices
       stating that You changed the files; and

   (c) You must retain, in the Source form of any Derivative Works
       that You distribute, all copyright, patent, trademark, and
       attribution notices from the Source form of the Work,
       excluding those notices that do not pertain to any part of
       the Derivative Works; and

   (d) If the Work includes a "NOTICE" text file as part of its
       distribution, then any Derivative Works that You distribute must
       include a readable copy of the attribution notices contained
       within such NOTICE file, excluding those notices that do not
       pertain to any part of the Derivative Works, in at least one
       of the following places: within a NOTICE text file distributed
       as part of the Derivative Works; within the Source form or
       documentation, if provided along with the Derivative Works; or,
       within a display generated by the Derivative Works, if and
       wherever such third-party notices normally appear. The contents
       of the NOTICE file are for informational purposes only and
       do not modify the License. You may add Your own attribution
       notices within Derivative Works that You distribute, alongside
       or as an addendum to the NOTICE text from the Work, provided
       that such additional attribution notices cannot be construed
       as modifying the License.

   You may add Your own copyright statement to Your modifications and
   may provide additional or different license terms and conditions
   for use, reproduction, or distribution of Your modifications, or
   for any such Derivative Works as a whole, provided Your use,
   reproduction, and distribution of the Work otherwise complies with
   the conditions stated in this License.

5. Submission of Contributions. Unless You explicitly state otherwise,
   any Contribution intentionally submitted for inclusion in the Work
   by You to the Licensor shall be under the terms and conditions of
   this License, without any additional terms or conditions.
   Notwithstanding the above, nothing herein shall supersede or modify
   the terms of any separate license agreement you may have executed
   with Licensor regarding such Contributions.

6. Trademarks. This License does not grant permission to use the trade
   names, trademarks, service marks, or product names of the Licensor,
   except as required for reasonable and customary use in describing the
   origin of the Work and reproducing the content of the NOTICE file.

7. Disclaimer of Warranty. Unless required by applicable law or
   agreed to in writing, Licensor provides the Work (and each
   Contributor provides its Contributions) on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
   implied, including, without limitation, any warranties or conditions
   of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
   PARTICULAR PURPOSE. You are solely responsible for determining the
   appropriateness of using or redistributing the Work and assume any
   risks associated with Your exercise of permissions under this License.

8. Limitation of Liability. In no event and under no legal theory,
   whether in tort (including negligence), contract, or otherwise,
   unless required by applicable law (such as deliberate and grossly
   negligent acts) or agreed to in writing, shall any Contributor be
   liable to You for damages, including any direct, indirect, special,
   incidental, or consequential damages of any character arising as a
   result of this License or out of the use or inability to use the
   Work (including but not limited to damages for loss of goodwill,
   work stoppage, computer failure or malfunction, or any and all
   other commercial damages or losses), even if such Contributor
   has been advised of the possibility of such damages.

9. Accepting Warranty or Additional Liability. While redistributing
   the Work or Derivative Works thereof, You may choose to offer,
   and charge a fee for, acceptance of support, warranty, indemnity,
   or other liability obligations and/or rights consistent with this
   License. However, in accepting such obligations, You may act only
   on Your own behalf and on Your sole responsibility, not on behalf
   of any other Contributor, and only if You agree to indemnify,
   defend, and hold each Contributor harmless for any liability
   incurred by, or claims asserted against, such Contributor by reason
   of your accepting any such warranty or additional liability.

END OF TERMS AND CONDITIONS
```

Every crate whose expression offers `Apache-2.0` is used under it unmodified; where one ships
a `NOTICE` file its lines are in the attribution list above. `bindgen` and `subtle`
(BSD-3-Clause) and `encoding_rs` (Apache-2.0/MIT **and** BSD-3-Clause) also carry the
BSD-3-Clause conditions below.

### Apache-2.0 WITH LLVM-exception

```
LLVM Exceptions to the Apache 2.0 License ----

As an exception, if, as a result of your compiling your source code, portions
of this Software are embedded into an Object form of such source code, you
may redistribute such embedded portions in such Object form without complying
with the conditions of Sections 4(a), 4(b) and 4(d) of the License.

In addition, if you combine or link compiled forms of this Software with
software that is licensed under the GPLv2 ("Combined Software") and if a
court of competent jurisdiction determines that the patent provision (Section
3), the indemnity provision (Section 9) or other Section of the License
conflicts with the conditions of the GPLv2, you may retroactively and
prospectively choose to deem waived or otherwise exclude such Section(s) of
the License, but only in their entirety and only with respect to the Combined
Software.
```

### ISC

```
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

The files under third-party/chromium are licensed as described in
third-party/chromium/LICENSE.
```

### BSD-2-Clause

```
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### BSD-3-Clause

```
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

1. Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED
TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### 0BSD

```
Copyright (C) Jonas Schievink <jonasschievink@gmail.com>

Permission to use, copy, modify, and/or distribute this software for
any purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

### Zlib

```
Copyright (c) 2019 Daniel "Lokathor" Gee.

This software is provided 'as-is', without any express or implied warranty. In no event will the authors be held liable for any damages arising from the use of this software.

Permission is granted to anyone to use this software for any purpose, including commercial applications, and to alter it and redistribute it freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim that you wrote the original software. If you use this software in a product, an acknowledgment in the product documentation would be appreciated but is not required.

2. Altered source versions must be plainly marked as such, and must not be misrepresented as being the original software.

3. This notice may not be removed or altered from any source distribution.
```

### Unicode-3.0

```
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2023 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```

### CDLA-Permissive-2.0

Data licence for the root certificate list in `webpki-roots` (unmodified). §2.1 requires the
text of the agreement to travel with the data, so it is reproduced in full.

```
# Community Data License Agreement - Permissive - Version 2.0

This is the Community Data License Agreement - Permissive, Version
2.0 (the "agreement"). Data Provider(s) and Data Recipient(s) agree
as follows:

## 1. Provision of the Data

1.1. A Data Recipient may use, modify, and share the Data made
available by Data Provider(s) under this agreement if that Data
Recipient follows the terms of this agreement.

1.2. This agreement does not impose any restriction on a Data
Recipient's use, modification, or sharing of any portions of the
Data that are in the public domain or that may be used, modified,
or shared under any other legal exception or limitation.

## 2. Conditions for Sharing Data

2.1. A Data Recipient may share Data, with or without modifications, so
long as the Data Recipient makes available the text of this agreement
with the shared Data.

## 3. No Restrictions on Results

3.1. This agreement does not impose any restriction or obligations
with respect to the use, modification, or sharing of Results.

## 4. No Warranty; Limitation of Liability

4.1. All Data Recipients receive the Data subject to the following
terms:

THE DATA IS PROVIDED ON AN "AS IS" BASIS, WITHOUT REPRESENTATIONS,
WARRANTIES OR CONDITIONS OF ANY KIND, EITHER EXPRESS OR IMPLIED
INCLUDING, WITHOUT LIMITATION, ANY WARRANTIES OR CONDITIONS OF TITLE,
NON-INFRINGEMENT, MERCHANTABILITY OR FITNESS FOR A PARTICULAR PURPOSE.

NO DATA PROVIDER SHALL HAVE ANY LIABILITY FOR ANY DIRECT, INDIRECT,
INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING
WITHOUT LIMITATION LOST PROFITS), HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE DATA OR RESULTS,
EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGES.

## 5. Definitions

5.1. "Data" means the material received by a Data Recipient under
this agreement.

5.2. "Data Provider" means any person who is the source of Data
provided under this agreement and in reliance on a Data Recipient's
agreement to its terms.

5.3. "Data Recipient" means any person who receives Data directly
or indirectly from a Data Provider and agrees to the terms of this
agreement.

5.4. "Results" means any outcome obtained by computational analysis
of Data, including for example machine learning models and models'
insights.
```

### MPL-2.0

The nine MPL-2.0 crates are listed in the MPL-2.0 section below. The licence text is
<https://mozilla.org/MPL/2.0/> and ships in each crate (`LICENSE`); §3.2 source availability is
covered by the published crate versions, and no covered file has been edited.

## MPL-2.0 components

Nine crates are MPL-2.0: `symphonia`, `symphonia-core`, `symphonia-bundle-flac`,
`symphonia-bundle-mp3`, `symphonia-codec-pcm`, `symphonia-format-riff`, `symphonia-metadata`,
`symphonia-utils-xiph` (all 0.5.5) and `serialport` 4.9.0. They are used **unmodified** and
linked as Rust crates. MPL-2.0 §3.2 requires the source of the covered files to be available;
it is, at the published versions:

- `https://crates.io/crates/symphonia/0.5.5` (and each `symphonia-*` sibling at 0.5.5)
- `https://crates.io/crates/serialport/4.9.0`

No MPL-covered file has been edited by Plippa, so no source need be published for a
modification. `serialport` reaches the build through `linux-embedded-hal`; the Symphonia reader
set is the trimmed one (`mp3`, `flac`, `wav`, `pcm` only).

## Vendored and patched code

Listed separately because it is not a crates.io dependency at build time:

- `third_party/tiny_http` — tiny_http 0.12.0, **modified** by Plippa (MIT OR Apache-2.0).
- `third_party/signalsmith-stretch` — Signalsmith Stretch + signalsmith-linear C++ libraries
  behind a vendored Rust wrapper (MIT); licence texts ship in that folder.
- `rs_ws281x 0.5.1` (MIT, Copyright 2018 Joseph Murphy) compiles the bundled `rpi_ws281x` C
  library (`src/rpi_ws281x/`, **BSD-2-Clause**, Copyright (c) 2014 jgarff, all rights reserved)
  into `plippa-leds`. Both notices are reproduced in `docs/third-party-notices.md`.
