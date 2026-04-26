# sonic-variant-printer

Minimal `cargo-sonic` example. `generic` is implicit and is always built, so it is
not listed in `Cargo.toml`.

Build the fat executable:

```bash
just build
```

Run it:

```bash
just run
```

The loader is silent. The only printed line comes from the application, which reads:

```text
CARGO_SONIC_SELECTED_TARGET_CPU
```

The generated executable is written to:

```text
target/sonic/x86_64-unknown-linux-gnu/debug/sonic-variant-printer
```
