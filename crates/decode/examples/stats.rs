//! Per-channel statistics of a decoded capture: `cargo run --release --example stats -- FILE`.
fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: stats FILE");
        return;
    };
    let cap = match silvercraft_decode::decode_file(std::path::Path::new(&path), &Default::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    println!("{}×{} {:?}", cap.rgb.width(), cap.rgb.height(), cap.kind);
    for c in 0..3 {
        let mut v: Vec<f32> = cap.rgb.data().iter().skip(c).step_by(3 * 7).copied().collect();
        v.sort_unstable_by(f32::total_cmp);
        let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        let at_max = v.iter().filter(|&&x| x >= q(1.0)).count() as f64 / v.len() as f64;
        println!("ch{c}: p1 {:.4} p50 {:.4} p90 {:.4} p99 {:.4} max {:.4}  at-max {:.3}%", q(0.01), q(0.5), q(0.9), q(0.99), q(1.0), at_max * 100.0);
    }
}
