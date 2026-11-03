//! Persisted roll-calibration document (serde round-trip; unknown fields
//! are preserved in `extra`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollCalibrationDocument {
    pub film_base: [f64; 3],
    pub density_matrix: [[f64; 3]; 3],
    pub curve_alpha: [f64; 3],
    pub curve_gamma: [f64; 3],
    pub curve_log_min: [f64; 3],
    pub curve_log_max: [f64; 3],
    pub curve_max_log_correction: f64,
    pub density_black: f64,
    pub density_white: f64,
    /// Median corrected neutral luminance (density units) over the roll;
    /// sets the scene-linear scale `k` . Optional for
    /// older documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e_mid: Option<f64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RollCalibrationPayload {
    Complete(Box<RollCalibrationDocument>),
    Legacy(serde_json::Map<String, Value>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_document_and_preserves_unknown_fields() {
        let json = r#"{"film_base":[0.8,0.7,0.5],"density_matrix":[[1,0,0],[0,1,0],[0,0,1]],
            "curve_alpha":[0,0,0],"curve_gamma":[1,1,1],"curve_log_min":[-20,-20,-20],
            "curve_log_max":[20,20,20],"curve_max_log_correction":0.2231,"density_black":0.1,
            "density_white":1.9,"future":{"x":7}}"#;
        let doc: RollCalibrationDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.e_mid, None);
        assert_eq!(doc.extra["future"]["x"], 7);
        let back: Value = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(back["future"]["x"], 7);
        assert!(back.get("e_mid").is_none());
        let payload: RollCalibrationPayload = serde_json::from_str(json).unwrap();
        assert!(matches!(payload, RollCalibrationPayload::Complete(_)));
        let legacy: RollCalibrationPayload = serde_json::from_str(r#"{"gain":[1,1,1]}"#).unwrap();
        assert!(matches!(legacy, RollCalibrationPayload::Legacy(_)));
    }
}
