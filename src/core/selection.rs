//! Literal member selection for extraction.
//!
//! A selection matches the archive's member identity through the one name rule in
//! `catalog` (`member_matches` / `member_subtree_suffix`): exact raw text, the
//! Hangul-composed form, or an ASCII case-insensitive form, plus descent into a
//! directory at a slash boundary. Raw identity is retained before display
//! normalization; separators are normalized to `/`. An empty selection means
//! "extract everything".

use crate::core::catalog::{member_form, member_subtree_suffix, CatalogEntry};

/// A validated set of literal member selection patterns.
#[derive(Debug, Clone)]
pub struct SelectionSpec {
    patterns: Vec<String>,
}

/// Normalize a caller-supplied member literal for matching against raw archive names.
fn normalize_member(member: &str) -> String {
    member_form(member)
}

/// True when `pattern` selects `name` itself or a member under it at a slash boundary,
/// using the one member-identity rule for every archive format.
fn member_matches(pattern: &str, name: &str) -> bool {
    member_subtree_suffix(pattern, name).is_some()
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
        self.patterns.iter().any(|p| member_matches(p, name))
    }
}

#[cfg(test)]
mod tests {
    use super::SelectionSpec;
    use crate::core::catalog::CatalogEntry;

    /// The same member name composed and in conjoining jamo, as macOS and Linux write it.
    const COMPOSED: &str = "\u{d55c}\u{ae00}.txt";
    const DECOMPOSED: &str = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}.txt";

    fn entry(name: &str) -> CatalogEntry {
        CatalogEntry {
            index: 0,
            name: name.to_string(),
            display_name: name.to_string(),
            is_dir: false,
            size: 4,
            compressed_size: 4,
            mtime: None,
            is_encrypted: false,
            crc: None,
        }
    }

    #[test]
    fn selection_resolves_a_member_in_both_hangul_compositions() {
        for (stored, requested) in [(DECOMPOSED, COMPOSED), (COMPOSED, DECOMPOSED)] {
            let catalog = vec![entry(stored), entry("notes.txt")];
            let spec = SelectionSpec::build(&catalog, &[requested.to_string()]).unwrap();
            assert!(
                spec.matches(stored),
                "stored {stored:?} requested {requested:?}"
            );
            assert!(!spec.matches("notes.txt"));
        }
    }

    #[test]
    fn selection_rejects_a_name_that_matches_no_member() {
        let catalog = vec![entry(DECOMPOSED), entry("notes.txt")];
        let error = SelectionSpec::build(&catalog, &["missing.txt".to_string()])
            .expect_err("an unmatched literal must be rejected");
        assert!(error.to_string().contains("missing.txt"), "{error}");
        let error = SelectionSpec::build(&catalog, &["\u{b2e4}\u{b978}.txt".to_string()])
            .expect_err("an unrelated Hangul literal must be rejected");
        assert!(error.to_string().contains("not found"), "{error}");
    }

    #[test]
    fn selection_still_keeps_a_directory_prefix_at_a_slash_boundary() {
        let catalog = vec![entry("dir/a.txt"), entry("dirish/b.txt")];
        let spec = SelectionSpec::build(&catalog, &["dir".to_string()]).unwrap();
        assert!(spec.matches("dir/a.txt"));
        assert!(!spec.matches("dirish/b.txt"));
    }
}
