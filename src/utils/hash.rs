use std::io::Read;
use std::path::Path;

use sha1::Digest;

use crate::error::LaunchError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha1,
    Md5,
    Sha256,
}

/// Compute a hex-encoded hash of the file at `path`.
pub fn get_file_hash(path: &Path, algorithm: HashAlgorithm) -> Result<String, LaunchError> {
    let mut file = std::fs::File::open(path)?;
    match algorithm {
        HashAlgorithm::Sha1 => stream_hash(sha1::Sha1::new(), &mut file),
        HashAlgorithm::Md5 => stream_hash(md5::Md5::new(), &mut file),
        HashAlgorithm::Sha256 => stream_hash(sha2::Sha256::new(), &mut file),
    }
}

fn stream_hash<D: Digest>(mut hasher: D, reader: &mut impl Read) -> Result<String, LaunchError> {
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f
    }

    #[test]
    fn sha1_empty() {
        let f = write_temp(b"");
        let h = get_file_hash(f.path(), HashAlgorithm::Sha1).unwrap();
        assert_eq!(h, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn sha1_known() {
        let f = write_temp(b"hello world");
        let h = get_file_hash(f.path(), HashAlgorithm::Sha1).unwrap();
        assert_eq!(h, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
    }

    #[test]
    fn md5_known() {
        let f = write_temp(b"hello world");
        let h = get_file_hash(f.path(), HashAlgorithm::Md5).unwrap();
        assert_eq!(h, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn sha256_known() {
        let f = write_temp(b"hello world");
        let h = get_file_hash(f.path(), HashAlgorithm::Sha256).unwrap();
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn missing_file_returns_error() {
        let r = get_file_hash(Path::new("/nonexistent/file.bin"), HashAlgorithm::Sha1);
        assert!(r.is_err());
    }
}
