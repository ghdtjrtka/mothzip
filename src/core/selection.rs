//! Literal member selection for extraction.
//!
//! A selection matches the archive's *raw* normalized member identity (exact)
//! or a directory prefix at a slash boundary. Raw identity is retained before
//! display normalization; separators are normalized to `/`. An empty selection
//! means "extract everything".

use crate::core::catalog::CatalogEntry;

/// A validated set of literal member selection patterns.
#[derive(Debug, Clone)]
pub struct SelectionSpec {
    patterns: Vec<String>,
}

/// Normalize a caller-supplied member literal for matching against raw archive names.
fn normalize_member(member: &str) -> String {
    member.replace('\\', "/").trim_matches('/').to_string()
}

/// True when `pattern` selects `name` exactly, or when `name` lies under the
/// directory `pattern` at a slash boundary.
fn member_matches(pattern: &str, name: &str) -> bool {
    if name == pattern {
        return true;
    }
    name.strip_prefix(pattern)
        .is_some_and(|rest| rest.starts_with('/'))
}

impl SelectionSpec {
    /// Validate every requested member against the raw catalog. A member that
    /// matches no single entry is rejected up front, before any staging or
    /// commit, so requested-but-missing members never disturb existing files.
    pub fn build(
        catalog: &[CatalogEntry],
        members: &[String],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let patterns: Vec<String> = members.iter().map(|m| normalize_member(m)).collect();
        let mut missing = Vec::new();
        for pattern in &patterns {
            if !catalog
                .iter()
                .any(|entry| member_matches(pattern, &entry.name))
            {
                missing.push(pattern.clone());
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "Requested archive member(s) not found: {}",
                missing.join(", ")
            )
            .into());
        }
        Ok(Self { patterns })
    }

    /// Whether the given raw normalized entry name is selected.
    pub fn matches(&self, name: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let name = name.replace('\\', "/");
        self.patterns.iter().any(|p| member_matches(p, &name))
    }
}
