# Third-party notices (product attribution list)

## Signalsmith Stretch (pitch-correct voice)

The pitch-correct playback voice uses **Signalsmith Stretch** (C++ time-stretch library) and its
dependency **signalsmith-linear**, through the Rust wrapper **signalsmith-stretch** 0.1.3 by Colin
Marc. All three are MIT licensed. They are vendored under `third_party/signalsmith-stretch/`
(cargo feature `stretch` of `plippa-audio`, on by default for `plippa-engine`); the licence texts
ship in that folder:

- `third_party/signalsmith-stretch/LICENSE.md` (Rust wrapper)
- `third_party/signalsmith-stretch/signalsmith-stretch/LICENSE.txt`
- `third_party/signalsmith-stretch/signalsmith-linear/LICENSE.txt`

> MIT License. Copyright (c) 2022 Geraint Luff / Signalsmith Audio Ltd. (Signalsmith Stretch);
> Copyright (c) 2025 Signalsmith Audio (signalsmith-linear); the wrapper is by Colin Marc.
> Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
> associated documentation files (the "Software"), to deal in the Software without restriction,
> including without limitation the rights to use, copy, modify, merge, publish, distribute,
> sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions: The above copyright notice and this
> permission notice shall be included in all copies or substantial portions of the Software.
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
> BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
> NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
> DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

Building without the C++ toolchain: `--no-default-features` on `plippa-engine` uses the in-tree
WSOLA stretcher instead (no third-party code).

## tiny_http (modified)

Vendored under `third_party/tiny_http/` — tiny_http 0.12.0 from crates.io, **modified by
Plippa** (MIT OR Apache-2.0; the licence texts ship in that folder as `LICENSE-MIT` /
`LICENSE-APACHE`). Every change is marked `PLIPPA PATCH` in the source; the authoritative list
is "Plippa local patches" in `third_party/tiny_http/README.md`. In summary:

1. worker-pool starvation on bursty accept;
2. bounded request/header/head reads with `414`/`431` answers and a bounded drain;
3. socket timeouts for write, first byte, head, and body silence — never a total-duration cap;
4. a connection gate and a worker ceiling, with `503` + `Retry-After` on refusal;
5. one request at a time per connection, so pipelining cannot queue unbounded requests;
6. unread-body draining with a fixed budget, closing the connection when any body byte is
   unread;
7. transient `accept()` errors (`EMFILE`/`ENFILE`/…) back off 100 ms and continue instead of
   ending the accept thread;
8. strict request framing: bad `Content-Length`, a second `Content-Length`, or anything but
   `chunked` is `400` + close;
9. single-port HTTPS: rustls 0.23 with a worker-side first-byte sniff and `SslConfig::accept_plaintext`.

## TLS stack

The portal's HTTPS uses these crates. They are already in the tree (the TLS stack came in
through `ureq`); the current build makes `ring` a direct dependency and `rustls`/`rustls-pki-types` direct
dependencies of the integration tests.

| Crate | Version | Licence | What it is |
|---|---|---|---|
| `ring` | 0.17.14 | Apache-2.0 AND ISC | crypto + key generation, and the certificate's ECDSA signature |
| `rustls` | 0.23.45 | Apache-2.0 OR ISC OR MIT | TLS 1.2/1.3 implementation |
| `rustls-pki-types` | 1.15.1 | MIT OR Apache-2.0 | the DER/PEM key types |
| `rustls-webpki` | 0.103.15 | ISC | certificate path validation inside rustls |
| `webpki-roots` | 1.0.9, 0.26.11 | CDLA-Permissive-2.0 | the public root certificate list (data only) |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | wiping key material |
| `ring` (build, via `bindgen`) | — | BSD-3-Clause | `bindgen` is a build dependency only, not redistributed |

`ring` is dual-licensed Apache-2.0 **and** ISC; both notices apply:

> Copyright 2015-2025 Brian Smith.
>
> Permission to use, copy, modify, and/or distribute this software for any purpose with or
> without fee is hereby granted, provided that the above copyright notice and this permission
> notice appear in all copies.
>
> THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
> SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL
> THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
> DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF
> CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE
> OR PERFORMANCE OF THIS SOFTWARE.

`rustls` and `rustls-webpki` (`Copyright (c) 2016, Joseph Birr-Pixton`) are elected under **ISC**
for the certificate-path pieces; `rustls` itself is used under `Apache-2.0 OR ISC OR MIT`, no
election needed.

`webpki-roots` carries the Community Data Licence Agreement, Permissive 2.0 (CDLA-Permissive-2.0)
— a **data** licence with no attribution condition: §2 of the agreement only requires that the
text of the agreement travels with the data. The full agreement text is reproduced in
`THIRD_PARTY_LICENSES.md` ("CDLA-Permissive-2.0"); the root certificates themselves are
unmodified.

## MPL-2.0 components (Symphonia, serialport)

`symphonia`, `symphonia-core`, `symphonia-bundle-flac`, `symphonia-bundle-mp3`,
`symphonia-codec-pcm`, `symphonia-format-riff`, `symphonia-metadata`, `symphonia-utils-xiph`
(all 0.5.5) and `serialport` 4.9.0 are **MPL-2.0**. They are used **unmodified**; MPL-2.0 §3.2
requires the source of the covered files to be available, and it is, at the published versions
(`https://crates.io/crates/symphonia/0.5.5`, each `symphonia-*` sibling, and
`https://crates.io/crates/serialport/4.9.0`). Plippa has edited none of their files, so nothing
has to be published as a modification. `serialport` reaches the build through
`linux-embedded-hal`.

## WS2811 output: `rs_ws281x` and the `rpi_ws281x` C library

`plippa-leds` (feature `ws281x`) drives the LED strips through `rs_ws281x` 0.5.1 (MIT,
Copyright 2018 Joseph Murphy), which compiles the bundled `rpi_ws281x` C library. That C
library is **BSD-2-Clause** and its notice must be reproduced:

> Copyright (c) 2014, jgarff. All rights reserved.
>
> Redistribution and use in source and binary forms, with or without modification, are
> permitted provided that the following conditions are met:
>
> 1. Redistributions of source code must retain the above copyright notice, this list of
>    conditions and the following disclaimer.
> 2. Redistributions in binary form must reproduce the above copyright notice, this list of
>    conditions and the following disclaimer in the documentation and/or other materials
>    provided with the distribution.
>
> THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS
> OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF
> MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
> COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
> EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE
> GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED
> AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
> NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED
> OF THE POSSIBILITY OF SUCH DAMAGE.

## UI fonts (`embedded-graphics`)

The on-device UI uses `FONT_6X13`, `FONT_6X13_BOLD`, `FONT_9X15`, `FONT_9X15_BOLD` and
`FONT_10X20` from `embedded-graphics` 0.8.2's `mono_font::iso_8859_1` module
(`bins/plippa-ui/src/ui/text.rs`). Those bitmaps come from the X.Org `misc-misc` font
distribution and are **public domain** (`fonts/src/README.md` in the crate: "public domain fonts
from the xorg project"). `embedded-graphics` itself is MIT OR Apache-2.0.

## `unescaper` (MIT election)

`unescaper` 0.1.8 is offered as `GPL-3.0/MIT` and reaches the build through `serialport` →
`linux-embedded-hal`. Plippa takes it under **MIT** (owner decision, 2026-11-08); the GPL
alternative is not used and no GPL code is linked.

## Eval-only and non-redistributed code

`eval/` is a separate workspace and none of it ships (see `eval/USAGE.md`). No third-party
datasheet is redistributed with this repository.

## Cover-art image crates (ART_02, `crates/plippa-image`)

Pure-Rust crates used for cover-art decode and thumbnail encode, pulled from crates.io (no
`build.rs`, no C code). Each ships its licence texts (`LICENSE-*`) in its published source.

| Crate | Version | Use | Licence |
|---|---|---|---|
| `zune-jpeg` | 0.5.15 | JPEG decode | MIT OR Apache-2.0 OR Zlib |
| `zune-png` | 0.5.2 | PNG decode | MIT OR Apache-2.0 OR Zlib |
| `zune-core` | 0.5.3 | shared by `zune-jpeg`, `zune-png` | MIT OR Apache-2.0 OR Zlib |
| `zune-inflate` | 0.2.54 | deflate for `zune-png` | MIT OR Apache-2.0 OR Zlib |
| `image-webp` | 0.2.4 | WebP decode | MIT OR Apache-2.0 |
| `byteorder-lite` | 0.1.0 | byte reading for `image-webp` | Unlicense OR MIT |
| `quick-error` | 2.0.1 | error macro for `image-webp` | MIT OR Apache-2.0 |
| `jpeg-encoder` | 0.7.1 | JPEG thumbnail encode | (MIT OR Apache-2.0) AND IJG |

Copyright holders: the zune-image developers (`zune-*`), the `image-webp` developers, Andrew Gallant
(`byteorder-lite`), Paul Colomiets, Colin Kiegel and the quick-error developers (`quick-error`),
Volker Strobel (`jpeg-encoder`). `zune-inflate` additionally depends on `simd-adler32`
(MIT OR Apache-2.0), which was already in the product's lock file.

> This software is based in part on the work of the Independent JPEG Group.

The sentence above is the attribution required by the IJG licence for the IJG-derived forward-DCT
code inside `jpeg-encoder` (ported from mozjpeg); it applies wherever Plippa software that links
`plippa-image` is distributed.

## Full inventory

[`THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md) is the complete generated list: every
crate in the closure of the shipped binaries (`plippa`, `plippa-engine`, `plippa-server`,
`plippa-ui`, `plippa-leds`, `plippa-media-helper`), with its version, licence expression and the
copyright lines from its own licence files. Its "Licence texts" section reproduces the terms of
every licence in use (MIT, Apache-2.0 and its LLVM exception, ISC, BSD-2/3-Clause, 0BSD, Zlib,
Unicode-3.0, CDLA-Permissive-2.0) so a distributed image carries the notices it must. The
generation commands are in that file's header. This page carries the prose and the notice texts
that must travel with the binary.

## Not resolved here

Provenance and licensing for the image fixtures
and the Smallbits icon bitmaps are unknown to the owner and are **not** claimed anywhere in this
document or in `THIRD_PARTY_LICENSES.md`. They are not yet resolved; nothing
here should be read as a licence for them.
