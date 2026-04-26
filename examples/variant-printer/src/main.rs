fn main() {
    let variant = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU")
        .unwrap_or_else(|_| "not-running-under-cargo-sonic".to_string());
    println!("selected target-cpu: {variant}");
}
