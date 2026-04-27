use std::time::Instant;

fn main() {
    let variant = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU")
        .unwrap_or_else(|_| "not-running-under-cargo-sonic".to_string());
    let iterations = std::env::var("SONIC_EXAMPLE_ITERS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2_000);
    let len = std::env::var("SONIC_EXAMPLE_LEN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1 << 18);

    let mut a = vec![0.0f32; len];
    let mut b = vec![0.0f32; len];
    let mut c = vec![0.0f32; len];
    initialize(&mut a, &mut b, &mut c);

    let started = Instant::now();
    let checksum = compute_kernel(&mut a, &mut b, &mut c, iterations);
    let elapsed = started.elapsed();

    println!("selected target-cpu: {variant}");
    println!("len: {len}");
    println!("iterations: {iterations}");
    println!("elapsed-ms: {}", elapsed.as_millis());
    println!("checksum: {checksum:.6}");
}

fn initialize(a: &mut [f32], b: &mut [f32], c: &mut [f32]) {
    for i in 0..a.len() {
        let x = i as u32;
        a[i] = ((x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) & 0xffff) as f32) * 0.000_031;
        b[i] = ((x.wrapping_mul(22_695_477).wrapping_add(1) & 0xffff) as f32) * 0.000_027;
        c[i] = ((x.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0xffff) as f32) * 0.000_019;
    }
}

#[inline(never)]
fn compute_kernel(a: &mut [f32], b: &mut [f32], c: &mut [f32], iterations: u64) -> f64 {
    let mut checksum = 0.0f64;
    for round in 0..iterations {
        let round_bias = (round as f32) * 0.000_001;
        let mut i = 0;
        while i < a.len() {
            let av = a[i];
            let bv = b[i];
            let cv = c[i];

            let x = av.mul_add(0.812_31, bv * 0.140_37) - cv * 0.031_11 + round_bias;
            let y = bv.mul_add(0.771_91, cv * 0.180_07) + av * 0.023_51;
            let z = cv.mul_add(0.902_03, x * y * 0.000_017) + 0.000_001;

            a[i] = x + z * 0.000_03;
            b[i] = y - x * 0.000_02;
            c[i] = z + y * 0.000_01;
            i += 1;
        }
        checksum += c[(round as usize).wrapping_mul(131) % c.len()] as f64;
    }

    for i in (0..a.len()).step_by(257) {
        checksum += a[i] as f64 * 0.25 + b[i] as f64 * 0.5 + c[i] as f64;
    }
    checksum
}
