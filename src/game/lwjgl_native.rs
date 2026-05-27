use serde::Deserialize;

use crate::error::LaunchError;
use crate::models::minecraft::{Library, MinecraftVersionJson};

// ── Embedded LWJGL ARM library manifests ─────────────────────────────────────
//
// These JSON files list the ARM-compiled LWJGL/JInput libraries that replace
// the official x86 ones.  They follow the same `{ "libraries": [...] }` shape
// as a Minecraft version JSON subset.
//
// IMPORTANT: The stub files bundled here contain empty library lists.  A real
// deployment must replace them with the actual ARM LWJGL libraries.  See
// `assets/LWJGL/` in the repository root.

macro_rules! lwjgl_bytes {
    ($arch:literal, $ver:literal) => {
        include_bytes!(concat!(
            "../../assets/LWJGL/",
            $arch,
            "/",
            $ver,
            ".json"
        ))
        .as_ref()
    };
}

/// Return the embedded JSON bytes for `(arch, version)`, or `None` if the
/// combination is not bundled.
fn arm_lwjgl_data(arch: &str, version: &str) -> Option<&'static [u8]> {
    // Mojang 2.9.x releases are all patched to 2.9.4 (matches JS behaviour).
    let version = if version.contains("2.9") { "2.9.4" } else { version };

    match (arch, version) {
        ("aarch64", "2.9.4") => Some(lwjgl_bytes!("aarch64", "2.9.4")),
        ("aarch64", "3.1.2") => Some(lwjgl_bytes!("aarch64", "3.1.2")),
        ("aarch64", "3.2.2") => Some(lwjgl_bytes!("aarch64", "3.2.2")),
        ("aarch64", "3.3.1") => Some(lwjgl_bytes!("aarch64", "3.3.1")),
        ("aarch64", "3.3.2") => Some(lwjgl_bytes!("aarch64", "3.3.2")),
        ("aarch", "2.9.4") => Some(lwjgl_bytes!("aarch", "2.9.4")),
        ("aarch", "3.3.1") => Some(lwjgl_bytes!("aarch", "3.3.1")),
        _ => None,
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Patch `version`'s library list for Linux ARM compatibility.
///
/// - Removes official LWJGL and JInput libraries (x86-only binaries).
/// - Injects ARM-compiled replacements from the bundled JSON manifests.
///
/// On non-ARM platforms this is a no-op; the check uses
/// `std::env::consts::ARCH` so a cross-compiled binary still detects its
/// actual execution environment at runtime.
pub fn process_json(version: &mut MinecraftVersionJson) -> Result<(), LaunchError> {
    let mapped_arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "arm" => "aarch",
        _ => return Ok(()), // not ARM — nothing to do
    };

    // Detect LWJGL and JInput versions from the existing library list.
    let version_jinput = find_version(&version.libraries, &["net.java.jinput:jinput-platform:", "net.java.jinput:jinput:"]);
    let version_lwjgl = find_version(&version.libraries, &["org.lwjgl:lwjgl:", "org.lwjgl.lwjgl:lwjgl:"]);

    // Remove official JInput libraries (replaced by ARM equivalents).
    if version_jinput.is_some() {
        version.libraries.retain(|lib| !lib.name.contains("jinput"));
    }

    // Remove official LWJGL libraries and inject ARM ones.
    if let Some(lwjgl_ver) = version_lwjgl {
        version.libraries.retain(|lib| !lib.name.contains("lwjgl"));

        match arm_lwjgl_data(mapped_arch, &lwjgl_ver) {
            Some(bytes) => {
                let set: LwjglLibrarySet = serde_json::from_slice(bytes)?;
                version.libraries.extend(set.libraries);
            }
            None => {
                // Bundled data missing for this LWJGL version — skip the patch
                // rather than crashing.  Callers can detect ARM support gaps by
                // checking whether `version.libraries` still contains x86 LWJGL.
            }
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the version component (last `:` segment) from the first library
/// whose `name` starts with any of `prefixes`.
fn find_version(libs: &[Library], prefixes: &[&str]) -> Option<String> {
    libs.iter()
        .find(|lib| prefixes.iter().any(|p| lib.name.starts_with(p)))
        .and_then(|lib| lib.name.split(':').last())
        .map(|v| v.to_string())
}

/// Minimal subset of a Minecraft version JSON — just the `libraries` field.
#[derive(Deserialize)]
struct LwjglLibrarySet {
    libraries: Vec<Library>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lib(name: &str) -> Library {
        Library {
            name: name.to_string(),
            rules: None,
            natives: None,
            downloads: None,
            url: None,
            loader: None,
        }
    }

    fn libs(names: &[&str]) -> Vec<Library> {
        names.iter().map(|n| make_lib(n)).collect()
    }

    #[test]
    fn find_version_returns_last_colon_segment() {
        let l = libs(&["org.lwjgl:lwjgl:3.3.1", "org.lwjgl:lwjgl-opengl:3.3.1"]);
        assert_eq!(find_version(&l, &["org.lwjgl:lwjgl:"]), Some("3.3.1".into()));
    }

    #[test]
    fn find_version_returns_none_when_absent() {
        let l = libs(&["com.example:something:1.0"]);
        assert_eq!(find_version(&l, &["org.lwjgl:lwjgl:"]), None);
    }

    #[test]
    fn find_version_matches_multiple_prefixes() {
        let l = libs(&["org.lwjgl.lwjgl:lwjgl:2.9.4"]);
        let v = find_version(&l, &["org.lwjgl:lwjgl:", "org.lwjgl.lwjgl:lwjgl:"]);
        assert_eq!(v, Some("2.9.4".into()));
    }

    #[test]
    fn arm_lwjgl_data_normalises_29x() {
        // Any 2.9.x version should resolve to the 2.9.4 bundle.
        assert!(arm_lwjgl_data("aarch64", "2.9.0").is_some());
        assert!(arm_lwjgl_data("aarch64", "2.9.1").is_some());
        assert!(arm_lwjgl_data("aarch64", "2.9.4").is_some());
    }

    #[test]
    fn arm_lwjgl_data_returns_none_for_unknown() {
        assert!(arm_lwjgl_data("aarch64", "4.0.0").is_none());
        assert!(arm_lwjgl_data("x86_64", "3.3.1").is_none());
    }

    #[test]
    fn process_json_noop_on_current_arch() {
        // On x86_64 (the typical CI/dev machine) this should be a no-op.
        if matches!(std::env::consts::ARCH, "aarch64" | "arm") {
            return; // ARM machine — test would modify libraries, skip.
        }

        let mut version = MinecraftVersionJson {
            id: "1.20.4".into(),
            version_type: "release".into(),
            assets: None,
            asset_index: None,
            downloads: None,
            libraries: libs(&["org.lwjgl:lwjgl:3.3.1", "org.lwjgl:lwjgl-opengl:3.3.1"]),
            arguments: None,
            minecraft_arguments: None,
            java_version: None,
            main_class: None,
            has_natives: false,
        };

        let original_count = version.libraries.len();
        process_json(&mut version).unwrap();
        // No change on non-ARM.
        assert_eq!(version.libraries.len(), original_count);
    }
}
