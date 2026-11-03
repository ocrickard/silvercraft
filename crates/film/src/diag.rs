//! Structured diagnostics shared by profile loading and config validation
//! (and reused by the top-level config crate).

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Note,
    Warning,
    Error,
}

/// One finding: where (`source` file, dotted `path` inside it), what, and how bad.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// File (or other origin such as "builtin:kodak-portra-400"); `None` for in-memory values.
    pub source: Option<String>,
    /// Dotted field path, e.g. "scene.mid_grey" or "negative.layer_gamma". Empty = whole document.
    pub path: String,
    pub message: String,
    pub severity: Severity,
}

impl Diagnostic {
    pub fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { source: None, path: path.into(), message: message.into(), severity: Severity::Error }
    }

    pub fn warning(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { source: None, path: path.into(), message: message.into(), severity: Severity::Warning }
    }

    pub fn note(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { source: None, path: path.into(), message: message.into(), severity: Severity::Note }
    }

    /// Attach (or replace) the source.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Prefix the field path (e.g. "processing" + "scene.mid_grey").
    pub fn with_path_prefix(mut self, prefix: &str) -> Self {
        self.path = if self.path.is_empty() { prefix.to_owned() } else { format!("{prefix}.{}", self.path) };
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        write!(f, "{level}")?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        if !self.path.is_empty() {
            write!(f, ": {}", self.path)?;
        }
        write!(f, ": {}", self.message)
    }
}

/// True when any diagnostic is an error.
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(Diagnostic::is_error)
}

/// A toml parse/deserialize error as a diagnostic (line/column in the message).
pub fn from_toml_error(error: &toml::de::Error, text: &str) -> Diagnostic {
    let mut message = error.message().trim().to_owned();
    if let Some(span) = error.span() {
        let before = text.get(..span.start).unwrap_or("");
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        message = format!("line {line}, column {column}: {message}");
    }
    Diagnostic::error("", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_helpers() {
        let d = Diagnostic::error("mid_grey", "out of range").with_path_prefix("scene").with_source("a.toml");
        assert_eq!(d.to_string(), "error: a.toml: scene.mid_grey: out of range");
        assert!(has_errors(&[Diagnostic::note("", "x"), d]));
        assert!(!has_errors(&[Diagnostic::warning("", "x")]));
        let err = toml::from_str::<toml::Table>("a = 1\nb = = 2").unwrap_err();
        let d = from_toml_error(&err, "a = 1\nb = = 2");
        assert!(d.message.starts_with("line 2"), "{}", d.message);
    }
}
