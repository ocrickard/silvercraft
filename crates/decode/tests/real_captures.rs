//! Decodes a real camera-scan capture when one is available on this machine; skipped
//! otherwise. Point `SILVERCRAFT_ARW` at a camera-scan raw to choose the file.

use std::path::PathBuf;

fn sample_arw() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("SILVERCRAFT_ARW")?);
    p.is_file().then_some(p)
}

#[test]
fn sony_a1_camera_scan_decodes_to_an_orange_negative() {
    let Some(path) = sample_arw() else {
        eprintln!("no camera-scan raw on this machine; skipped");
        return;
    };
    let cap = silvercraft_decode::decode_file(&path, &Default::default()).unwrap();
    let (w, h) = (cap.rgb.width(), cap.rgb.height());
    assert!(w > 4000 && h > 2500, "{w}×{h}");
    assert!(!cap.srgb_primaries);
    // a C-41 negative: red transmits most, blue least (the orange mask)
    let mut sum = [0f64; 3];
    for p in cap.rgb.data().chunks_exact(3) {
        for c in 0..3 {
            sum[c] += p[c] as f64;
        }
    }
    assert!(sum[0] > sum[1] && sum[1] > sum[2], "{sum:?}");
    assert!(cap.rgb.data().iter().all(|v| v.is_finite() && *v >= 0.0));
}
