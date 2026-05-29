/// All observable events emitted during a launch session.
///
/// Replaces the Node.js EventEmitter pattern. Callers receive a
/// `tokio::sync::mpsc::Receiver<LaunchEvent>` and the library holds
/// the matching `Sender`, cloning it into every sub-module that needs
/// to report progress.
#[derive(Debug, Clone)]
pub enum LaunchEvent {
    /// A batch of files is being downloaded.
    /// `kind` identifies the category (e.g. "libraries", "assets", "java").
    Progress {
        downloaded: u64,
        total: u64,
        kind: String,
    },

    /// Current download speed in bytes per second.
    Speed(f64),

    /// Estimated seconds remaining for the current download batch.
    Estimated(f64),

    /// A bundle integrity check is in progress.
    /// `kind` identifies the category being checked.
    Check {
        current: usize,
        total: usize,
        kind: String,
    },

    /// A file is being extracted from an archive. Carries the file name.
    Extract(String),

    /// A Forge processor is running. Carries the processor class name.
    Patch(String),

    /// All game files have been downloaded and verified.
    GameDownloadFinished,

    /// A line of stdout/stderr from the running Minecraft process.
    Data(String),

    /// The Minecraft process has exited. Carries the exit code.
    Close(i32),

    /// A non-fatal error message (fatal errors are returned as `Err`).
    Error(String),
}
