//! Persistence of undo history. See `docs/superpowers/specs/2026-09-26-persistent-undo-design.md`.

/// Returned when a persisted history cannot be trusted and must be discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHistory(&'static str);

impl InvalidHistory {
    pub(crate) fn new(reason: &'static str) -> Self {
        Self(reason)
    }
}

impl std::fmt::Display for InvalidHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidHistory {}
