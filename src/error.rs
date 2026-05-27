use thiserror::Error;

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("no authenticator provided")]
    NoAuthenticator,

    #[error("no internet connection and no local cache available")]
    NoInternetNoCache,

    #[error("corrupt local cache: {0}")]
    CorruptCache(String),

    #[error("version '{0}' not found in Mojang manifest")]
    VersionNotFound(String),

    #[error("Java process error: {0}")]
    ProcessError(String),

    #[error("archive error: {0}")]
    Archive(String),

    #[error("download error: {0}")]
    Download(#[from] DownloadError),

    #[error("loader error: {0}")]
    Loader(#[from] LoaderError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // For HTTP calls outside the downloader (version manifest, asset index, etc.)
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid data: {0}")]
    InvalidData(String),
}

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("installer download failed: {0}")]
    InstallerDownloadFailed(String),

    #[error("installer JAR not found at: {0}")]
    InstallerNotFound(String),

    // install_profile.json missing from the installer JAR
    #[error("install_profile.json not found in installer JAR")]
    ProfileNotFound,

    #[error("forge processor '{processor}' failed with exit code {code:?}")]
    ProcessorFailed { processor: String, code: Option<i32> },

    #[error("loader API error: {0}")]
    ApiError(String),

    #[error("loader version not found: {0}")]
    VersionNotFound(String),

    #[error("archive error: {0}")]
    Archive(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("checksum mismatch for '{file}': expected {expected}, got {actual}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("no mirror available for: {0}")]
    NoMirrorAvailable(String),

    #[error("request timed out")]
    Timeout,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}
