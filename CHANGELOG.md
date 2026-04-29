# Changelog

## Unreleased

- Add `cargo sonic score` to rank rustc-supported target CPUs compatible with the current host without requiring `--target-cpus`.
- Fix generated x86_64 GNU loaders so they no longer force the system linker path in CI.

# 0.1.5

- Add `--loader=bundle` to build a CPU detection launcher plus an adjacent payload bundle directory for faster startup in container-style deployments.
- Add zstd payload compression with `--compress=zstd` and configurable `--compression-level`. Add runtime decompression support in the generated loader for compressed payloads while keeping the uncompressed reflink path unchanged.
- Add `-p`/`--parallelism` to build target-cpu payloads concurrently.
- Fix aarch64 neoverse detection

# 0.1.4

First public release
