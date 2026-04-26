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

This repository currently has a working Linux/x86_64 vertical slice:

- Cargo metadata config under `[package.metadata.sonic]`
- implicit `generic` payload, always built
- target-cpu validation through `rustc --print target-cpus`
- payload builds with isolated target directories
- generated `no_std`/`no_main` loader
- `include_bytes!` payload embedding
- `memfd_create` plus `execveat`
- `CARGO_SONIC_*` env replacement
- x86_64 CPUID feature detection for the implemented feature set
- x86_64 CPU identity detection for selection affinity
- unit tests and a generic fixture integration test

Known incomplete areas:

- AArch64 end-to-end runtime testing is not complete.
- QEMU user/system test runners are scaffolds, not full matrix executors yet.
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

For example:

```text
examples/variant-printer/target/sonic/x86_64-unknown-linux-gnu/debug/sonic-variant-printer
```

## Configuration

Configure variants in `Cargo.toml`:

```toml
[package.metadata.sonic]
target-cpus = [
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

The currently verified path is Linux x86_64.

## Testing

Run pure unit tests and the current integration fixture:

```bash
cargo test
```

Run the QEMU discovery scaffold:

```bash
cargo run -p xtask -- qemu-user
```

Run the opt-in system-mode scaffold:

```bash
SONIC_QEMU_SYSTEM=1 cargo run -p xtask -- qemu-system
```

System-mode runs require kernel/initrd environment variables and currently skip
cleanly when those inputs are absent.

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
crates/sonic-loader-probe   probe placeholder
crates/sonic-survey         survey placeholder
crates/xtask                automation scaffolding
examples/variant-printer    minimal runnable example
tests/fixtures              integration fixtures
tests/qemu                  QEMU matrix files
```
