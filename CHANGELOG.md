# Changelog

## Unreleased

- Run the system-mode QEMU correctness suite against both `embedded` and `bundle` loader strategies.
- Reuse each architecture's QEMU target-cpu probe results across loader strategies and include skipped CPU reasons when no payload target-cpus are buildable.
- Add an early QEMU host Rust target check that reports the exact `rustup target add ...` command when a cross target is missing.
- Add `--loader=bundle` to build a CPU detection launcher plus an adjacent payload bundle directory for faster startup in container-style deployments.
- Add zstd payload compression with `--compress=zstd` and configurable `--compression-level`. Add runtime decompression support in the generated loader for compressed payloads while keeping the uncompressed reflink path unchanged.
- Add `-p`/`--parallelism` to build target-cpu payloads concurrently.

# 0.1.4

First public release
