use ndarray::Array2;
use std::time::Instant;

fn main() {
    let variant = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU")
        .unwrap_or_else(|_| "not-running-under-cargo-sonic".to_string());
    let n = std::env::var("SONIC_NDARRAY_N")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(384);
    let iterations = std::env::var("SONIC_NDARRAY_ITERS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(48);

    let mut a = Array2::from_shape_fn((n, n), |(row, col)| seed_value(row, col, 0.000_19));
    let mut b = Array2::from_shape_fn((n, n), |(row, col)| seed_value(col, row, 0.000_23));

    let started = Instant::now();
    let checksum = compute_kernel(&mut a, &mut b, iterations);
    let elapsed = started.elapsed();

    println!("selected target-cpu: {variant}");
    println!("n: {n}");
    println!("iterations: {iterations}");
    println!("elapsed-ms: {}", elapsed.as_millis());
    println!("checksum: {checksum:.6}");
}

fn seed_value(row: usize, col: usize, scale: f32) -> f32 {
    let x = (row as u32)
        .wrapping_mul(1_664_525)
        .wrapping_add((col as u32).wrapping_mul(22_695_477))
        .wrapping_add(1_013_904_223);
    ((x & 0xffff) as f32) * scale
}

#[inline(never)]
fn compute_kernel(a: &mut Array2<f32>, b: &mut Array2<f32>, iterations: u64) -> f64 {
    let mut checksum = 0.0f64;
    for round in 0..iterations {
        let mut c = a.dot(b);
        let scale = 0.000_003 + (round as f32) * 0.000_000_001;
        c.mapv_inplace(|value| value.mul_add(scale, 0.000_017));

        let idx = (round as usize).wrapping_mul(193) % c.len();
        let row = idx / c.ncols();
        let col = idx % c.ncols();
        checksum += c[(row, col)] as f64;

        *a = std::mem::replace(b, c);
    }

    for row in (0..a.nrows()).step_by(13) {
        for col in (0..a.ncols()).step_by(17) {
            checksum += a[(row, col)] as f64 * 0.25 + b[(row, col)] as f64 * 0.5;
        }
    }
    checksum
}
