use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::LaunchError;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Polymorphic result of `get_file_from_archive`.
///
/// - `file = Some(name)` → `FileData` (or `NotFound`)
/// - `file = None, include_dirs = true` → `Entries` with full metadata
/// - `file = None, include_dirs = false` → `Names` of file entries only
///
/// When `prefix` is provided it filters by entry name prefix in all list modes.
#[derive(Debug)]
pub enum ArchiveQueryResult {
    /// Raw bytes of the requested file.
    FileData(Vec<u8>),
    /// Full entry listing (includes directories when `include_dirs = true`).
    Entries(Vec<ArchiveEntry>),
    /// Entry names only (file-only when `include_dirs = false`).
    Names(Vec<String>),
    /// The requested `file` was not found.
    NotFound,
}

// ── ZIP / JAR ─────────────────────────────────────────────────────────────────

/// Query a ZIP or JAR archive.
///
/// All I/O is done inside `spawn_blocking` so this is safe to `.await` from
/// an async context without blocking the Tokio runtime.
pub async fn get_file_from_archive(
    path: PathBuf,
    file: Option<String>,
    prefix: Option<String>,
    include_dirs: bool,
) -> Result<ArchiveQueryResult, LaunchError> {
    tokio::task::spawn_blocking(move || {
        query_zip_sync(&path, file.as_deref(), prefix.as_deref(), include_dirs)
    })
    .await
    .map_err(|e| LaunchError::Archive(e.to_string()))?
}

fn query_zip_sync(
    path: &Path,
    file: Option<&str>,
    prefix: Option<&str>,
    include_dirs: bool,
) -> Result<ArchiveQueryResult, LaunchError> {
    let f = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(f).map_err(|e| LaunchError::Archive(e.to_string()))?;

    if let Some(name) = file {
        return match archive.by_name(name) {
            Ok(mut entry) => {
                let mut data = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut data)?;
                Ok(ArchiveQueryResult::FileData(data))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(ArchiveQueryResult::NotFound),
            Err(e) => Err(LaunchError::Archive(e.to_string())),
        };
    }

    if include_dirs {
        let mut entries = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let entry = archive
                .by_index(i)
                .map_err(|e| LaunchError::Archive(e.to_string()))?;
            let name = entry.name().to_string();
            if let Some(p) = prefix {
                if !name.starts_with(p) {
                    continue;
                }
            }
            entries.push(ArchiveEntry {
                is_dir: entry.is_dir(),
                size: entry.size(),
                name,
            });
        }
        Ok(ArchiveQueryResult::Entries(entries))
    } else {
        let mut names = Vec::new();
        for i in 0..archive.len() {
            let entry = archive
                .by_index(i)
                .map_err(|e| LaunchError::Archive(e.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            if let Some(p) = prefix {
                if !name.starts_with(p) {
                    continue;
                }
            }
            names.push(name);
        }
        Ok(ArchiveQueryResult::Names(names))
    }
}

// ── TAR.GZ ────────────────────────────────────────────────────────────────────

/// Extract a `.tar.gz` archive to `dest`, optionally stripping leading
/// path components (e.g. `strip_components = 1` removes the top-level
/// directory that most JDK tarballs include).
///
/// All I/O is done inside `spawn_blocking`.
pub async fn extract_tar_gz(
    src: PathBuf,
    dest: PathBuf,
    strip_components: usize,
) -> Result<(), LaunchError> {
    tokio::task::spawn_blocking(move || extract_tar_gz_sync(&src, &dest, strip_components))
        .await
        .map_err(|e| LaunchError::Archive(e.to_string()))?
}

fn extract_tar_gz_sync(
    src: &Path,
    dest: &Path,
    strip_components: usize,
) -> Result<(), LaunchError> {
    let file = std::fs::File::open(src)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    // Follow symlinks inside the archive (needed for some JDK tarballs).
    archive.set_preserve_permissions(true);

    for entry in archive
        .entries()
        .map_err(|e| LaunchError::Archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| LaunchError::Archive(e.to_string()))?;

        let raw_path = entry
            .path()
            .map_err(|e| LaunchError::Archive(e.to_string()))?
            .into_owned();

        let stripped: PathBuf = raw_path.components().skip(strip_components).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }

        let out = dest.join(&stripped);

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry
                .unpack(&out)
                .map_err(|e| LaunchError::Archive(e.to_string()))?;
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    // Build an in-memory ZIP and write it to a temp file.
    fn make_test_zip() -> NamedTempFile {
        use zip::write::SimpleFileOptions;

        let mut tmp = NamedTempFile::new().unwrap();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();

            w.add_directory("META-INF/", opts).unwrap();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            w.write_all(b"Manifest-Version: 1.0\n").unwrap();

            w.start_file("data/hello.txt", opts).unwrap();
            w.write_all(b"hello world").unwrap();

            w.start_file("data/world.txt", opts).unwrap();
            w.write_all(b"world hello").unwrap();

            let finished = w.finish().unwrap();
            tmp.write_all(finished.get_ref()).unwrap();
        }
        tmp
    }

    #[tokio::test]
    async fn read_specific_file() {
        let zip_file = make_test_zip();
        let result = get_file_from_archive(
            zip_file.path().to_path_buf(),
            Some("META-INF/MANIFEST.MF".into()),
            None,
            false,
        )
        .await
        .unwrap();

        match result {
            ArchiveQueryResult::FileData(data) => {
                assert_eq!(data, b"Manifest-Version: 1.0\n");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_file_returns_not_found() {
        let zip_file = make_test_zip();
        let result = get_file_from_archive(
            zip_file.path().to_path_buf(),
            Some("does_not_exist.txt".into()),
            None,
            false,
        )
        .await
        .unwrap();

        assert!(matches!(result, ArchiveQueryResult::NotFound));
    }

    #[tokio::test]
    async fn list_all_files_no_dirs() {
        let zip_file = make_test_zip();
        let result = get_file_from_archive(zip_file.path().to_path_buf(), None, None, false)
            .await
            .unwrap();

        match result {
            ArchiveQueryResult::Names(names) => {
                assert!(names.contains(&"META-INF/MANIFEST.MF".to_string()));
                assert!(names.contains(&"data/hello.txt".to_string()));
                assert!(names.contains(&"data/world.txt".to_string()));
                // directory entries excluded
                assert!(!names.iter().any(|n| n == "META-INF/"));
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_with_prefix() {
        let zip_file = make_test_zip();
        let result = get_file_from_archive(
            zip_file.path().to_path_buf(),
            None,
            Some("data/".into()),
            false,
        )
        .await
        .unwrap();

        match result {
            ArchiveQueryResult::Names(names) => {
                assert_eq!(names.len(), 2);
                assert!(names.contains(&"data/hello.txt".to_string()));
                assert!(names.contains(&"data/world.txt".to_string()));
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_all_entries_include_dirs() {
        let zip_file = make_test_zip();
        let result = get_file_from_archive(zip_file.path().to_path_buf(), None, None, true)
            .await
            .unwrap();

        match result {
            ArchiveQueryResult::Entries(entries) => {
                assert!(entries.iter().any(|e| e.is_dir && e.name == "META-INF/"));
                assert!(entries
                    .iter()
                    .any(|e| !e.is_dir && e.name == "data/hello.txt"));
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn extract_tar_gz_strips_root() {
        // Build a simple .tar.gz with one nested file: root/file.txt
        let dest = TempDir::new().unwrap();
        let src = {
            use flate2::write::GzEncoder;
            use flate2::Compression;

            let mut tar_data = Vec::new();
            {
                let enc = GzEncoder::new(&mut tar_data, Compression::fast());
                let mut builder = tar::Builder::new(enc);

                let content = b"tar content";
                let mut header = tar::Header::new_gnu();
                header.set_path("jdk-21/file.txt").unwrap();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, content.as_ref()).unwrap();
                builder.finish().unwrap();
            }

            let mut f = NamedTempFile::new().unwrap();
            f.write_all(&tar_data).unwrap();
            f
        };

        extract_tar_gz(src.path().to_path_buf(), dest.path().to_path_buf(), 1)
            .await
            .unwrap();

        let out = dest.path().join("file.txt");
        assert!(out.exists(), "file.txt should exist after extraction");
        assert_eq!(std::fs::read(&out).unwrap(), b"tar content");
    }
}
