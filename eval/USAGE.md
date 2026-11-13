# Running the eval apps

`eval/` is a **separate cargo workspace** (`eval/Cargo.toml`), excluded from the
root one. The hardware POCs, bench tools and one-off experiments live here; they
are not part of the product, are never built by a root `cargo build
--workspace`, and are not redistributed.

The root workspace builds only the shipped binaries. Their board notes are in
`boards/prototypes/plippa-D-poc2/boardSetup.md`; `eval/poc2/poc2_readme.md` only
points there now.

## Why it is separate

The POCs pull in hardware crates that a normal dev box and the Pi do not have:
`rs_ws281x` (bindgen → needs libclang and a C compiler), `spidev`, `gpio-cdev`,
`evdev`, `libpulse-simple`. Keeping them in the root workspace meant every
`cargo build --workspace` on a clean machine either failed to build or failed to
link.

## Build and test one app

```bash
# Check / test one package (cheap, still needs libclang for rs_ws281x)
cargo check --manifest-path eval/Cargo.toml -p poc2_screen_anim_ILI9341_bouncy
cargo test  --manifest-path eval/Cargo.toml -p poc2_screen_anim_ILI9341_bouncy

# The whole eval workspace (slow: builds every POC)
cargo check --manifest-path eval/Cargo.toml --workspace
```

Package names are `poc2_<dir>` (e.g. `eval/poc2/warp_bench` is
package `poc2_warp_bench`), except the two POC2 apps that declare their own
`[[bin]]` name (`warp_bench`).

### libclang

`rs_ws281x` runs bindgen at build time. If `libclang.so` is not where bindgen
looks, point it at the versioned library:

```bash
mkdir -p /tmp/libclang
ln -sfn "$(ls /usr/lib/llvm-*/lib/libclang-*.so.* | head -1)" /tmp/libclang/libclang.so
LIBCLANG_PATH=/tmp/libclang cargo check --manifest-path eval/Cargo.toml -p poc2_led_test
```

On Debian/Ubuntu, `sudo apt install libclang-dev` is the tidier fix.

## Build for the Pi and push

`scripts/deploy.sh eval` builds one POC2 app for `aarch64` through the eval
workspace, pushes it to `$PI_DIR/target/<profile>/` and does not touch the
service or any capability:

```bash
PROFILE=release scripts/deploy.sh eval warp_bench
PROFILE=release scripts/deploy.sh eval qr_probe
```

`cross_env` in that script exports the sysroot, linker, `PKG_CONFIG_*`,
bindgen's cross args and (if needed) a `LIBCLANG_PATH`; a cross build also needs
`g++-aarch64-linux-gnu` for the C++ stretcher. Performance numbers are only
meaningful from a release build.

## Lock files

`eval/Cargo.lock` pins the versions the POCs use. It is independent of the root
`Cargo.lock` on purpose: an experiment here cannot move a version the product
depends on. Do not run `cargo update` in either workspace without the owner.

## Adding a new eval app

Add the crate directory and list it in `eval/Cargo.toml` under `members`, then use
`workspace = true` for any dependency already in its `[workspace.dependencies]`.
If a dependency is new, add it to the eval list only — not to the root workspace —
unless a production crate needs it too.
