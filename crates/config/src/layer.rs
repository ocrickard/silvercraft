//! Layered TOML: later layers override earlier ones key by key (tables merge recursively, every
//! other value replaces), then the result is deserialised once, so unknown keys and wrong types
//! anywhere are reported with the layer they came from.

use serde::Serialize;
use serde::de::DeserializeOwned;
use toml::{Table, Value};

/// Where a layer came from, for messages and provenance.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LayerSource {
    /// `defaults`, `stock <id>`, a file path, or `--set`.
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub source: LayerSource,
    pub table: Table,
}

#[derive(Debug, thiserror::Error)]
pub enum LayerError {
    #[error("{layer}: {message}")]
    Parse { layer: String, message: String },
    #[error("--set {expr}: {message}")]
    Set { expr: String, message: String },
    #[error("{0}")]
    Invalid(String),
}

impl Layer {
    pub fn new(name: impl Into<String>, table: Table) -> Self {
        Self { source: LayerSource { name: name.into() }, table }
    }

    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self, LayerError> {
        let name = name.into();
        let table: Table = toml::from_str(text).map_err(|e| LayerError::Parse { layer: name.clone(), message: e.to_string() })?;
        Ok(Self::new(name, table))
    }

    /// A layer holding `value` serialised (e.g. a struct's defaults).
    pub fn from_value<T: Serialize>(name: impl Into<String>, value: &T) -> Result<Self, LayerError> {
        let name = name.into();
        match Value::try_from(value) {
            Ok(Value::Table(t)) => Ok(Self::new(name, t)),
            Ok(_) => Err(LayerError::Invalid(format!("{name}: not a table"))),
            Err(e) => Err(LayerError::Invalid(format!("{name}: {e}"))),
        }
    }

    /// `section.key=value` assignments (the value is TOML when it parses, else a string).
    pub fn from_assignments(exprs: &[String]) -> Result<Self, LayerError> {
        let mut table = Table::new();
        for expr in exprs {
            let (path, raw) = expr.split_once('=').ok_or_else(|| LayerError::Set { expr: expr.clone(), message: "expected key=value".into() })?;
            let path: Vec<&str> = path.trim().split('.').collect();
            if path.iter().any(|p| p.is_empty()) {
                return Err(LayerError::Set { expr: expr.clone(), message: "empty key".into() });
            }
            let value = parse_value(raw.trim());
            insert(&mut table, &path, value).map_err(|message| LayerError::Set { expr: expr.clone(), message })?;
        }
        Ok(Self::new("--set", table))
    }

    /// Wrap this layer's table under `prefix` (e.g. a stock's `[processing]` under `film`).
    pub fn nested(self, prefix: &str) -> Self {
        let mut t = Table::new();
        t.insert(prefix.to_string(), Value::Table(self.table));
        Self { source: self.source, table: t }
    }
}

/// A TOML scalar/array/inline table when `raw` parses as one, else the raw text as a string.
fn parse_value(raw: &str) -> Value {
    toml::from_str::<Table>(&format!("v = {raw}")).ok().and_then(|mut t| t.remove("v")).unwrap_or_else(|| Value::String(raw.to_string()))
}

fn insert(table: &mut Table, path: &[&str], value: Value) -> Result<(), String> {
    match path {
        [] => Err("empty key".into()),
        [last] => {
            table.insert((*last).to_string(), value);
            Ok(())
        }
        [head, rest @ ..] => {
            let entry = table.entry((*head).to_string()).or_insert_with(|| Value::Table(Table::new()));
            match entry {
                Value::Table(t) => insert(t, rest, value),
                _ => Err(format!("`{head}` is a value, not a section")),
            }
        }
    }
}

/// Merge `over` into `base`: tables merge recursively, anything else replaces.
pub fn merge(base: &mut Table, over: &Table) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Merge `layers` in order and deserialise. On failure, each layer is checked alone on top of the
/// defaults (the first layer) to name the one that introduced the problem.
pub fn resolve<T: DeserializeOwned>(layers: &[Layer]) -> Result<T, LayerError> {
    let mut merged = Table::new();
    for l in layers {
        merge(&mut merged, &l.table);
    }
    match T::deserialize(Value::Table(merged)) {
        Ok(v) => Ok(v),
        Err(e) => {
            let culprit = layers.iter().skip(1).find(|l| {
                let mut t = layers.first().map(|d| d.table.clone()).unwrap_or_default();
                merge(&mut t, &l.table);
                T::deserialize(Value::Table(t)).is_err()
            });
            let source = culprit.map(|l| l.source.name.clone()).unwrap_or_else(|| "configuration".into());
            Err(LayerError::Parse { layer: source, message: e.to_string() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct Inner {
        a: f64,
        b: String,
        list: Vec<i64>,
    }

    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct Conf {
        inner: Inner,
        flag: bool,
    }

    #[test]
    fn later_layers_override_key_by_key() {
        let defaults = Layer::from_value("defaults", &Conf { inner: Inner { a: 1.0, b: "x".into(), list: vec![1] }, flag: false }).unwrap();
        let file = Layer::parse("roll.toml", "[inner]\nb = \"file\"\nlist = [2, 3]").unwrap();
        let set = Layer::from_assignments(&["inner.a=2.5".into(), "flag=true".into()]).unwrap();
        let c: Conf = resolve(&[defaults, file, set]).unwrap();
        assert_eq!(c, Conf { inner: Inner { a: 2.5, b: "file".into(), list: vec![2, 3] }, flag: true });
    }

    #[test]
    fn errors_name_the_layer() {
        let defaults = Layer::from_value("defaults", &Conf::default()).unwrap();
        let good = Layer::parse("a.toml", "flag = true").unwrap();
        let bad = Layer::parse("b.toml", "[inner]\ntypo = 1").unwrap();
        let err = resolve::<Conf>(&[defaults.clone(), good, bad]).unwrap_err().to_string();
        assert!(err.starts_with("b.toml:"), "{err}");
        let set = Layer::from_assignments(&["inner.a=fast".into()]).unwrap();
        let err = resolve::<Conf>(&[defaults, set]).unwrap_err().to_string();
        assert!(err.starts_with("--set:"), "{err}");
        assert!(Layer::parse("c.toml", "not = = toml").is_err());
    }

    #[test]
    fn assignments_parse_values_and_reject_bad_paths() {
        let l = Layer::from_assignments(&["x.n=3".into(), "x.s=hello world".into(), "x.l=[1, 2]".into(), "x.q=\"quoted\"".into()]).unwrap();
        let x = l.table["x"].as_table().unwrap();
        assert_eq!(x["n"].as_integer(), Some(3));
        assert_eq!(x["s"].as_str(), Some("hello world"));
        assert_eq!(x["l"].as_array().map(Vec::len), Some(2));
        assert_eq!(x["q"].as_str(), Some("quoted"));
        assert!(Layer::from_assignments(&["noequals".into()]).is_err());
        assert!(Layer::from_assignments(&["a..b=1".into()]).is_err());
        assert!(Layer::from_assignments(&["a=1".into(), "a.b=2".into()]).is_err());
    }

    #[test]
    fn nesting_moves_a_layer_under_a_section() {
        let l = Layer::parse("stock", "[scene]\nmid_grey = 0.2").unwrap().nested("film");
        assert!(l.table["film"]["scene"]["mid_grey"].as_float().is_some());
    }
}
