use std::hint::black_box;
use std::time::Instant;

fn main() {
    let variant = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU")
        .unwrap_or_else(|_| "not-running-under-cargo-sonic".to_string());
    let mib = std::env::var("SONIC_BLAKE3_MIB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64);
    let iterations = std::env::var("SONIC_BLAKE3_ITERS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(8);

    let input = make_input(mib.saturating_mul(1024 * 1024));

    let started = Instant::now();
    let digest = hash_kernel(&input, iterations);
    let elapsed = started.elapsed();
    let total_mib = mib as u64 * iterations;
    let throughput = total_mib as f64 / elapsed.as_secs_f64();

    println!("selected target-cpu: {variant}");
    println!("input-mib: {mib}");
    println!("iterations: {iterations}");
    println!("elapsed-ms: {}", elapsed.as_millis());
    println!("throughput-mib-s: {throughput:.1}");
    println!("digest: {digest}");
}

fn make_input(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut input = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        input.push((state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8);
    }
    input
}

#[inline(never)]
fn hash_kernel(input: &[u8], iterations: u64) -> blake3::Hash {
    let mut digest = blake3::Hash::from([0u8; 32]);
    for round in 0..iterations {
        let mut hasher = blake3::Hasher::new();
        hasher.update(input);
        hasher.update(&round.to_le_bytes());
        hasher.update(digest.as_bytes());
        digest = black_box(hasher.finalize());
    }
    digest
}
