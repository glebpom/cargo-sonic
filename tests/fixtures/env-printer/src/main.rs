fn main() {
    println!("argv={}", std::env::args().skip(1).collect::<Vec<_>>().join(","));
    println!("keep={}", std::env::var("KEEP_ME").unwrap_or_default());
    println!("enabled={}", std::env::var("CARGO_SONIC_ENABLED").unwrap_or_default());
    println!("cpu={}", std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU").unwrap_or_default());
    println!("flags={}", std::env::var("CARGO_SONIC_SELECTED_FLAGS").unwrap_or_default());
}
