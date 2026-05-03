# Changelog

## Unreleased

- Add `cargo sonic score` to rank rustc-supported target CPUs compatible with the current host without requiring `--target-cpus`.
- Add a `blake3` benchmark example for workloads that already use runtime CPU feature detection in a dependency.
- Fix generated x86_64 GNU loaders so they no longer force the system linker path in CI.
- Extend the QEMU suite to exercise glibc and musl static/dynamic payloads plus no-std payloads across plain/zstd compression, embedded/bundle loaders, and normal/cross compilation modes.
- Make missing dynamic musl inputs a hard QEMU setup error instead of skipping musl-dynamic variants.
- Include the first failing QEMU matrix sub-variant, app status, and reason in summary failures.
- Fix QEMU dynamic musl payload builds to pass an explicit musl dynamic linker and avoid requiring a musl-compatible `libgcc_s`.
- Simplify CI dynamic musl setup to install and stage only the aarch64 musl `libc.so`.
- Build generated loaders with controlled rustflags so ambient Cargo target rustflags cannot make the loader CPU-specific.
- Fix generated musl loaders so they do not pass unsupported `-nostartfiles` linker arguments to `rust-lld`.
- Add a `just miri` loader unsafe-code suite for Linux x86_64 and AArch64 and run it in CI.

# 0.1.5

- Add `--loader=bundle` to build a CPU detection launcher plus an adjacent payload bundle directory for faster startup in container-style deployments.
- Add zstd payload compression with `--compress=zstd` and configurable `--compression-level`. Add runtime decompression support in the generated loader for compressed payloads while keeping the uncompressed reflink path unchanged.
- Add `-p`/`--parallelism` to build target-cpu payloads concurrently.
- Fix aarch64 neoverse detection

# 0.1.4

First public release
