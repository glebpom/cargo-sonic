# Changelog

## Unreleased

# 0.2.0

- Add `cargo sonic score` to rank rustc-supported target CPUs compatible with the current host without requiring `--target-cpus`.
- Add a `blake3` benchmark example for workloads that already use runtime CPU feature detection in a dependency.
- Include the first failing QEMU matrix sub-variant, app status, and reason in summary failures.
- Build generated loaders with controlled rustflags so ambient Cargo target rustflags cannot make the loader CPU-specific.
- Add a `just miri` loader unsafe-code suite for Linux x86_64 and AArch64 and run it in CI.

# 0.1.5

- Add `--loader=bundle` to build a CPU detection launcher plus an adjacent payload bundle directory for faster startup in container-style deployments.
- Add zstd payload compression with `--compress=zstd` and configurable `--compression-level`. Add runtime decompression support in the generated loader for compressed payloads while keeping the uncompressed reflink path unchanged.
- Add `-p`/`--parallelism` to build target-cpu payloads concurrently.
- Fix aarch64 neoverse detection

# 0.1.4

First public release
