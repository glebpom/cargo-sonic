# cargo-sonic

`cargo-sonic` is an experimental Cargo subcommand that builds one Linux fat
executable for a Rust binary.

The output executable contains multiple ELF payloads built from the same binary
with different `-C target-cpu=<cpu>` values. At runtime, a small generated
`no_std` loader detects CPU features, selects the best compatible payload,
writes it to a `memfd`, and executes it with `execveat(AT_EMPTY_PATH)`.

The loader is intentionally silent. Applications can observe the selected
variant through environment variables.

## Status

This repository currently has a working Linux x86_64/AArch64 vertical slice:

- Cargo metadata config under `[package.metadata.sonic]`
- implicit `generic` payload, always built
- target-cpu validation through `rustc --print target-cpus`
- payload builds with isolated target directories
- generated Rust 2024 `no_std`/`no_main` loader
- `include_bytes!` payload embedding
- `memfd_create` plus `execveat`
- `CARGO_SONIC_*` env replacement
- x86_64 CPUID and AArch64 auxv feature detection for the implemented feature set
- x86_64 CPU identity detection for selection affinity
- unit tests, generated-loader integration tests, and fixture-driven modern CPU
  selector tests
- pinned QEMU 11.0.0 system-mode correctness suite via `just qemu`

Known incomplete areas:

- Build-time warning analysis is still partial.
- `sonic-survey` is a placeholder.

## Run Locally

From a target crate directory:

```bash
cargo run --manifest-path /path/to/cargo-sonic/crates/cargo-sonic/Cargo.toml -- sonic build
```

From this repository root, build the included example by passing the target
crate manifest through Cargo's normal `--manifest-path` flag:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- sonic build \
  --manifest-path examples/variant-printer/Cargo.toml
```

The final executable is written under the target crate:

```text
target/sonic/<target-triple>/<profile>/<bin-name>
```

If `CARGO_TARGET_DIR` is set, `cargo-sonic` writes under that directory instead:

```text
$CARGO_TARGET_DIR/sonic/<target-triple>/<profile>/<bin-name>
```

`CARGO_TARGET` is also accepted as a compatibility alias, but
`CARGO_TARGET_DIR` is the Cargo-standard variable.

For example:

```text
examples/variant-printer/target/sonic/x86_64-unknown-linux-gnu/debug/sonic-variant-printer
```

## Configuration

Configure variants in `Cargo.toml`:

```toml
[package.metadata.sonic]
target-cpus = [
  "x86-64-v3",
  "raptorlake",
  "znver5",
]
```

`generic` is implicit. It is always built and is always eligible at runtime, so
do not list it unless you want to be explicit.

Rules:

- `target-cpus` must exist.
- `generic` is added automatically.
- `native` is rejected.
- CPU names must exactly match local `rustc --print target-cpus` names.
- Cross-architecture CPU names are skipped for the current target.
- Unknown CPU names are hard errors.

The target triple comes from normal Cargo arguments:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- sonic build --release
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- sonic build --target x86_64-unknown-linux-gnu
```

Binary/package selection also uses normal Cargo arguments:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- sonic build --bin server
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- sonic build --package my-crate --bin worker
```

## Probe

Use `cargo sonic probe` to inspect the current host without building payloads:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- sonic probe \
  --manifest-path examples/variant-printer/Cargo.toml
```

The probe reads the same `[package.metadata.sonic]` configuration, asks rustc
for each configured target-cpu's feature contract, detects the current CPU, and
prints which configured target-cpus are eligible. It uses the same selection
logic as the loader, but runs as a normal `std` command.

## Runtime Environment

Before executing the selected payload, the loader removes old `CARGO_SONIC_*`
values and appends:

```text
CARGO_SONIC_ENABLED=1
CARGO_SONIC_SELECTED_TARGET_CPU=<target_cpu>
CARGO_SONIC_SELECTED_FLAGS=<rustc target_feature CSV>
```

Selection is enabled by default. Set `CARGO_SONIC_ENABLE=0` or
`CARGO_SONIC_ENABLE=false` to force the loader to select the `generic` payload:

```bash
CARGO_SONIC_ENABLE=0 target/sonic/x86_64-unknown-linux-gnu/release/sonic-variant-printer
```

Application code can read these like normal environment variables:

```rust
fn main() {
    let cpu = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU")
        .unwrap_or_else(|_| "not-running-under-cargo-sonic".to_string());
    println!("selected target-cpu: {cpu}");
}
```

The application cannot inspect loader internals directly. The environment is
the supported observation surface.

## Loader Debugging

The loader is silent by default. Set `CARGO_SONIC_DEBUG` to make it print
selection diagnostics to stderr before it executes the selected payload:

```bash
CARGO_SONIC_DEBUG=1 target/sonic/x86_64-unknown-linux-gnu/release/sonic-variant-printer
```

Debug output includes:

- detected host feature mask and feature names
- detected CPU identity
- every configured variant
- each variant's eligibility
- missing feature names for ineligible variants
- selected target CPU

## Example

See [examples/variant-printer](examples/variant-printer).

It has a `justfile`:

```bash
cd examples/variant-printer
just build
just run
just check-loader
```

The example builds in release mode and runs a CPU-heavy floating point kernel
implemented directly in the example, with no crate-level runtime CPU dispatch.
On a Raptor Lake host, `just run` prints:

```text
selected target-cpu: raptorlake
```

Compare default selection with forced generic:

```bash
just compare
```

The loader itself prints nothing.

## Supported Targets

The intended v0 target set is:

```text
target_os = "linux"
target_arch = "x86_64" | "aarch64"
```

The currently verified paths are Linux x86_64 and Linux AArch64.

## Testing

Run pure unit tests, generated-loader integration tests, and fixture-driven
selector tests:

```bash
cargo test
```

Run only the modern CPU fixture suites:

```bash
cargo test -p sonic-loader fixture_modern
```

These tests parse `tests/cpu-fixtures/x86_64-modern.tsv` and
`tests/cpu-fixtures/aarch64-modern.tsv`, synthesize loader `HostInfo` values
from CPUID family/model or AArch64 MIDR implementer/part rows, and assert that
the real loader selector chooses the expected rustc/LLVM target-cpu. They are
not QEMU tests and do not execute a generated binary; they cover modern CPU
identity cases that QEMU TCG cannot model as strict guest oracles.

Run the QEMU system-mode suite:

```bash
just qemu-prepare
just qemu
```

The QEMU correctness suite is system-mode only. It must boot a controlled Linux
guest for each CPU model listed in `tests/qemu/system.toml`, run rustc inside
that guest with `-C target-cpu=native`, run the cargo-sonic fat executable inside
the same guest, and compare the loader-selected target against the rustc-derived
expectation. Host `qemu-user`, host rustc, and checked-in selected-target goldens
are intentionally not used as correctness oracles.

QEMU cases run in parallel. The default worker count is the number of available
CPU cores; override it with `SONIC_QEMU_JOBS=<n>` when needed:

```bash
SONIC_QEMU_JOBS=4 just qemu
```

`just qemu-prepare` owns the asset directory under
`$CARGO_TARGET_DIR/sonic-qemu-system` when `CARGO_TARGET_DIR` is set, otherwise
`target/sonic-qemu-system`.
It downloads and builds the pinned QEMU version from source, downloads pinned
guest kernels/initrds, installs pinned guest Rust toolchains into cached Ubuntu
base rootfs directories, and leaves a generated asset README in that directory.

The default matrix is intentionally limited to CPU models where pinned QEMU TCG
can expose a runtime feature set consistent with the rustc native target. For
example, QEMU 11.0.0 reports `EPYC-Turin` as `znver5` to rustc, but TCG cannot
expose the AVX512 features required by rustc's `znver5` contract, so that model
is documented in `tests/qemu/system.toml` rather than kept as a permanently
failing exact-oracle case.

## Loader Minimalism Check

The example includes:

```bash
just check-loader
```

This verifies the generated loader executable has no ELF interpreter and no
dynamic libc dependency:

```bash
readelf -l target/sonic/x86_64-unknown-linux-gnu/release/sonic-variant-printer
readelf -d target/sonic/x86_64-unknown-linux-gnu/release/sonic-variant-printer
```

Expected:

```text
no INTERP segment
no NEEDED libc
```

## Workspace Layout

```text
crates/cargo-sonic          Cargo subcommand entrypoint
crates/sonic-build          build orchestration and loader generation
crates/sonic-loader         testable no_std loader logic
crates/xtask                automation and QEMU matrix runner
examples/variant-printer    minimal runnable example
tests/cpu-fixtures          fixture-driven modern CPU selector suites
tests/fixtures              integration fixtures
tests/qemu                  QEMU system-mode matrix
```
