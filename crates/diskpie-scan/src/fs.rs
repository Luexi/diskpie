//! Portable filesystem-provider seam consumed by the scan workers.

use diskpie_core::{EntryKind, FileIdentity, OwnMetrics};
use std::{error::Error, ffi::OsString, fmt, path::Path};
use std::{path::PathBuf, result::Result};

/// Metadata for one child returned by a directory provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsEntry {
    pub name: OsString,
    pub kind: EntryKind,
    pub own_metrics: OwnMetrics,
    pub file_identity: Option<FileIdentity>,
}

impl FsEntry {
    #[must_use]
    pub fn new(name: impl Into<OsString>, kind: EntryKind, own_metrics: OwnMetrics) -> Self {
        Self { name: name.into(), kind, own_metrics, file_identity: None }
    }

    #[must_use]
    pub fn with_file_identity(mut self, identity: FileIdentity) -> Self {
        self.file_identity = Some(identity);
        self
    }
}

/// Filesystem operation that produced an omission or provider error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FsOperation {
    EnumerateDirectory,
    ReadMetadata,
    ReadIdentity,
}

/// Portable classification of a filesystem-provider failure.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FsErrorKind {
    AccessDenied,
    NotFound,
    NotSupported,
    Interrupted,
    Io,
    ProviderFailure,
}

impl FsErrorKind {
    /// Provider failures violate the seam contract; ordinary filesystem errors
    /// are omissions and do not abort the whole scan.
    #[must_use]
    pub const fn is_fatal(self) -> bool {
        matches!(self, Self::ProviderFailure)
    }
}

/// Cloneable error value safe to move through the scan protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsError {
    pub path: PathBuf,
    pub operation: FsOperation,
    pub kind: FsErrorKind,
    pub os_code: Option<i32>,
    pub detail: Option<String>,
}

impl FsError {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, operation: FsOperation, kind: FsErrorKind) -> Self {
        Self { path: path.into(), operation, kind, os_code: None, detail: None }
    }

    #[must_use]
    pub const fn with_os_code(mut self, os_code: i32) -> Self {
        self.os_code = Some(os_code);
        self
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} failed for {}: {:?}",
            self.operation,
            self.path.display(),
            self.kind
        )?;
        if let Some(code) = self.os_code {
            write!(formatter, " (OS error {code})")?;
        }
        if let Some(detail) = &self.detail {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

impl Error for FsError {}

/// Recoverable entry/directory failure retained in partial scan output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOmission {
    pub error: FsError,
}

impl ScanOmission {
    #[must_use]
    pub const fn new(error: FsError) -> Self {
        Self { error }
    }
}

impl From<FsError> for ScanOmission {
    fn from(error: FsError) -> Self {
        Self::new(error)
    }
}

/// One callback value from [`ScanFs::visit_directory`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryItem {
    Entry(FsEntry),
    Omission(ScanOmission),
}

/// Allows a worker to stop an enumeration cooperatively on cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisitControl {
    Continue,
    Stop,
}

/// Object-safe portable directory-enumeration seam implemented by the platform.
///
/// Implementations must close directory handles before returning and must stop
/// calling `visitor` after it returns [`VisitControl::Stop`]. Metadata needed by
/// the model is returned with each entry; scan workers perform no platform I/O.
pub trait ScanFs: Send + Sync + 'static {
    fn visit_directory(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
    ) -> Result<(), FsError>;
}
