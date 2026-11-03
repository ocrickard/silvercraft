//! Hostile-input properties: arbitrary bytes, samples and config values must produce
//! an error or a finite result — never a panic, hang or unbounded allocation. The
//! generated TIFF/RAW bytes are structureless garbage on purpose; decoders must
//! reject them cheaply.

use proptest::prelude::*;
use silvercraft_decode::mask_aware;
use silvercraft_decode::{DecodeConfig, tiff_scan};

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn garbage_tiffs_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = tiff_scan::decode(&bytes);
        let _ = tiff_scan::decode_ir(&bytes, 4, 4);
    }

    #[test]
    fn garbage_raws_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = silvercraft_decode::decode_camera_raw(&bytes, &DecodeConfig::default());
        let custom = DecodeConfig {
            camera_matrix: silvercraft_decode::MatrixSource::Custom,
            xyz_to_camera: Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            ..Default::default()
        };
        let _ = silvercraft_decode::decode_camera_raw(&bytes, &custom);
    }

    /// Any sample values (including NaN/±∞) and any percentile (including NaN):
    /// the mask-aware conversion finishes with finite, non-negative output.
    #[test]
    fn mask_aware_conversion_stays_finite(
        rgb in proptest::collection::vec(any::<f32>(), 0..300),
        percentile in any::<f64>(),
    ) {
        let m = mask_aware::camera_to_srgb(&[[0.4, 0.3, 0.2], [0.2, 0.7, 0.1], [0.1, 0.2, 0.6]]).unwrap();
        let mut data = rgb;
        mask_aware::convert(&mut data, &m, percentile);
        let n = data.len() / 3 * 3;
        prop_assert!(data[..n].iter().all(|v| v.is_finite() && *v >= 0.0));
        let scale = mask_aware::mask_scale(&data, percentile);
        prop_assert!(scale.iter().all(|v| v.is_finite() && *v > 0.0));
    }
}
