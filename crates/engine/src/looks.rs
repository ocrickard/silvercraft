//! Film-stock looks as LightCraft presets. SilverCraft's DNGs are neutral scene-linear captures;
//! a stock's character (contrast, saturation, black and white) is a LightCraft preset applied on
//! import, so it stays editable and removable there.
//!
//! The preset comes from the profile's `[look]` table: print-look values in
//! `[look.print]` are mapped to LightCraft sliders, and a `[look.lightcraft]` table, if
//! present, is LightCraft develop settings (as in a `.lcpreset`) merged on top, verbatim.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use silvercraft_film::profiles::FilmProfile;
use silvercraft_handoff::LookPreset;

use crate::Result;

/// The LightCraft preset group stock looks live in.
pub const GROUP: &str = "SilverCraft Film Stocks";

pub fn preset_id(stock: &str) -> String {
    format!("silvercraft.{stock}")
}

fn num(t: Option<&toml::Table>, key: &str) -> Option<f64> {
    let v = t?.get(key)?;
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64)).filter(|x| x.is_finite())
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Partial LightCraft develop settings for `p`'s look (empty when the stock has no look).
pub fn look_settings(p: &FilmProfile) -> Value {
    let print = p.look.get("print").and_then(toml::Value::as_table);
    let mono = !p.kind.is_color();
    let mut out = Map::new();
    let mut light = Map::new();
    let mut color = Map::new();
    if mono {
        out.insert("treatment".into(), json!("bw"));
    }
    if let Some(e) = num(print, "exposure").filter(|e| *e > 0.0) {
        let ev = e.log2();
        if ev.abs() >= 0.05 {
            light.insert("exposure".into(), json!((ev * 100.0).round() / 100.0));
        }
    }
    // paper grade (print gamma multiplier) and the look's contrast multiplier → Contrast
    let grade = num(print, "paper_grade").unwrap_or(1.0);
    let contrast = num(print, "contrast").unwrap_or(1.0);
    let c = ((grade - 1.0) * 60.0 + (contrast - 1.0) * 100.0).clamp(-60.0, 60.0);
    if c.abs() >= 1.0 {
        light.insert("contrast".into(), json!(round1(c)));
    }
    // a lifted paper black → raised Blacks
    if let Some(pb) = num(print, "paper_black").filter(|b| *b > 0.0) {
        light.insert("blacks".into(), json!(round1((pb * 200.0).min(25.0))));
    }
    if !mono && let Some(s) = num(print, "saturation") {
        let v = ((s - 1.0) * 100.0).clamp(-100.0, 100.0);
        if v.abs() >= 1.0 {
            color.insert("saturation".into(), json!(round1(v)));
        }
    }
    if !light.is_empty() {
        out.insert("light".into(), Value::Object(light));
    }
    if !color.is_empty() {
        out.insert("color".into(), Value::Object(color));
    }
    let mut v = Value::Object(out);
    if let Some(lc) = p.look.get("lightcraft").and_then(toml::Value::as_table)
        && let Ok(extra) = serde_json::to_value(lc)
    {
        merge(&mut v, extra);
    }
    v
}

fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        a.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// `.lcpreset` text for `p`'s look, or `None` when it has none.
pub fn lcpreset(p: &FilmProfile) -> Option<String> {
    let settings = look_settings(p);
    if settings.as_object().is_none_or(Map::is_empty) {
        return None;
    }
    let doc = json!({
        "format": "lightcraft.preset",
        "version": 1,
        "presets": [{"id": preset_id(&p.id), "name": p.name, "group": GROUP, "settings": settings}],
    });
    serde_json::to_string_pretty(&doc).ok()
}

/// Write `p`'s look into `dir` as `<stock>.lcpreset` (importable by hand too) and describe it
/// for the handoff; `None` when the stock has no look.
pub fn write_look(p: &FilmProfile, dir: &Path) -> Result<Option<LookPreset>> {
    let Some(text) = lcpreset(p) else { return Ok(None) };
    // Slugged: a profile id from a TOML file must not be able to walk out of `dir`.
    let file: PathBuf = dir.join(format!("{}.lcpreset", silvercraft_handoff::config::slug(&p.id)));
    silvercraft_handoff::write::write_atomic(&file, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Some(LookPreset { id: preset_id(&p.id), name: p.name.clone(), group: GROUP.into(), file }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use silvercraft_film::profiles::builtin_registry;

    #[test]
    fn colour_negative_look_maps_print_values() {
        let p = builtin_registry().get("kodak-portra-400").unwrap();
        let v = look_settings(p);
        // paper grade 0.82 → softer; saturation 1.18 → +18
        assert!(v["light"]["contrast"].as_f64().unwrap() < 0.0, "{v}");
        assert_eq!(v["color"]["saturation"], json!(18.0));
        assert!(v.get("treatment").is_none());
    }

    #[test]
    fn black_and_white_stocks_are_monochrome() {
        let p = builtin_registry().get("ilford-xp2-super").unwrap();
        let v = look_settings(p);
        assert_eq!(v["treatment"], json!("bw"));
        assert!(v.get("color").is_none(), "no saturation on a mono look");
        let text = lcpreset(p).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["presets"][0]["id"], json!("silvercraft.ilford-xp2-super"));
        assert_eq!(doc["presets"][0]["group"], json!(GROUP));
    }

    #[test]
    fn lightcraft_table_is_merged_verbatim() {
        let mut p = builtin_registry().get("kodak-portra-400").unwrap().clone();
        let extra: toml::Table = toml::from_str("[lightcraft.grain]\namount = 20.0\n[lightcraft.color]\nvibrance = 5.0\n").unwrap();
        p.look.extend(extra);
        let v = look_settings(&p);
        assert_eq!(v["grain"]["amount"], json!(20.0));
        assert_eq!(v["color"]["vibrance"], json!(5.0));
        assert_eq!(v["color"]["saturation"], json!(18.0), "derived values stay");
    }

    #[test]
    fn no_look_no_preset() {
        let mut p = builtin_registry().get("kodak-portra-400").unwrap().clone();
        p.look.clear();
        assert!(lcpreset(&p).is_none());
    }
}
