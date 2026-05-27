/// Returns `true` when the version JSON describes a legacy Minecraft release
/// (pre-1.6) that uses the old `resources/` asset layout instead of the
/// modern `assets/` object store.
///
/// Mirrors the JS `isold()` check on `json.assets`.
#[inline]
pub fn is_old(assets: Option<&str>) -> bool {
    matches!(assets, Some("legacy") | Some("pre-1.6"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_legacy_strings() {
        assert!(is_old(Some("legacy")));
        assert!(is_old(Some("pre-1.6")));
    }

    #[test]
    fn modern_versions_are_not_old() {
        assert!(!is_old(Some("1.19")));
        assert!(!is_old(Some("1")));
        assert!(!is_old(None));
        // Case-sensitive — Mojang always uses lowercase
        assert!(!is_old(Some("Legacy")));
    }
}
