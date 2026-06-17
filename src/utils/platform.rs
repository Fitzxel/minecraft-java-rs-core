use serde::{Deserialize, Serialize};

// ── Rule types (used here and re-exported via models::minecraft in Step 3) ──

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibraryRule {
    pub action: RuleAction,
    pub os: Option<OsRule>,
    /// Feature flags (e.g. `has_custom_resolution`). We don't evaluate them;
    /// rules that carry features are skipped conservatively.
    pub features: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Disallow,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OsRule {
    pub name: Option<String>,
    pub version: Option<String>,
    pub arch: Option<String>,
}

// ── Platform detection ───────────────────────────────────────────────────────

/// OS name in Mojang's format: `"windows"`, `"osx"`, `"linux"`.
/// macOS is `"macos"` in Rust but `"osx"` in all Mojang JSON.
pub fn mojang_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "osx",
        other => other, // "windows" and "linux" pass through unchanged
    }
}

/// CPU architecture in the format used by Mojang native classifiers.
///
/// Rust's `std::env::consts::ARCH` already produces `"x86"`, `"x86_64"`,
/// `"aarch64"`, `"arm"` — these align with Mojang's native strings.
/// Anything unrecognised defaults to `"x86_64"` (the overwhelmingly common case).
pub fn mojang_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "x86",
        "aarch64" => "aarch64",
        "arm" => "arm",
        _ => "x86_64",
    }
}

// ── Library rule evaluation ──────────────────────────────────────────────────

/// Returns `true` if the library should be **skipped** on the current platform.
///
/// Implements the Mojang rule semantics from `skipLibrary` in the original JS:
/// - No rules → never skip.
/// - Rules are evaluated in order; the last matching rule wins.
/// - `action = allow`    → include the library.
/// - `action = disallow` → exclude the library.
/// - Rules with `features` are ignored (not yet modelled).
/// - A rule with no `os` clause matches every platform.
pub fn skip_library(rules: &[LibraryRule]) -> bool {
    if rules.is_empty() {
        return false;
    }

    let current_os = mojang_os();
    let mut should_skip = true;

    for rule in rules {
        // Skip feature-gated rules — we don't model feature flags yet.
        if rule.features.is_some() {
            continue;
        }

        let os_matches = match &rule.os {
            None => true,
            Some(os_rule) => os_rule.name.as_deref() == Some(current_os),
        };

        if os_matches {
            should_skip = matches!(rule.action, RuleAction::Disallow);
        }
    }

    should_skip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(os: Option<&str>) -> LibraryRule {
        LibraryRule {
            action: RuleAction::Allow,
            os: os.map(|n| OsRule {
                name: Some(n.to_string()),
                version: None,
                arch: None,
            }),
            features: None,
        }
    }

    fn disallow(os: Option<&str>) -> LibraryRule {
        LibraryRule {
            action: RuleAction::Disallow,
            os: os.map(|n| OsRule {
                name: Some(n.to_string()),
                version: None,
                arch: None,
            }),
            features: None,
        }
    }

    #[test]
    fn no_rules_means_include() {
        assert!(!skip_library(&[]));
    }

    #[test]
    fn allow_all_includes_on_every_os() {
        // allow with no os clause → matches all platforms
        assert!(!skip_library(&[allow(None)]));
    }

    #[test]
    fn last_rule_wins() {
        // allow then disallow(all) → skip
        assert!(skip_library(&[allow(None), disallow(None)]));
        // disallow then allow(all) → include
        assert!(!skip_library(&[disallow(None), allow(None)]));
    }

    #[test]
    fn os_specific_rule_only_matches_that_os() {
        // A disallow rule for a different OS should not affect the current platform.
        let other_os = if mojang_os() == "linux" {
            "windows"
        } else {
            "linux"
        };
        let rules = vec![allow(None), disallow(Some(other_os))];
        // allow(all) set should_skip=false, then disallow(other) doesn't match → still false
        assert!(!skip_library(&rules));
    }

    #[test]
    fn mojang_os_is_not_macos() {
        // Ensure macOS is mapped to "osx" in Mojang format
        if std::env::consts::OS == "macos" {
            assert_eq!(mojang_os(), "osx");
        }
    }
}
