set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: test

# Run the normal Rust test suite.
test:
    cargo test

# Run the Miri loader unsafe-code suite on supported Linux architectures.
miri:
    rustup toolchain install nightly --profile minimal --component miri
    cargo +nightly miri setup
    cargo +nightly miri test -p cargo-sonic --lib --no-default-features --target x86_64-unknown-linux-gnu loader_miri
    cargo +nightly miri test -p cargo-sonic --lib --no-default-features --target aarch64-unknown-linux-gnu loader_miri

# Prepare pinned system-mode QEMU assets under target/sonic-qemu-system.
qemu-prepare:
    cargo run -p xtask -- qemu-prepare

# Run the system-mode QEMU correctness suite.
qemu:
    cargo run -p xtask -- qemu
