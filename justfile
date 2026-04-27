set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: test

# Run the normal Rust test suite.
test:
    cargo test

# Prepare pinned system-mode QEMU assets under target/sonic-qemu-system.
qemu-prepare:
    cargo run -p xtask -- qemu-prepare

# Run the system-mode QEMU correctness suite.
qemu:
    cargo run -p xtask -- qemu
