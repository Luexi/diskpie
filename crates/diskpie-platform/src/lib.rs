//! Filesystem and operating-system adapters for DiskPie.
//!
//! Project-authored unsafe code is permitted only in narrowly reviewed modules
//! under this crate's Windows adapter. Every unsafe block must document its
//! invariants and translate raw resources into RAII wrappers immediately.

#![deny(unsafe_op_in_unsafe_fn)]

pub use diskpie_core::PRODUCT_NAME;

#[cfg(windows)]
pub mod windows {
    pub mod diagnostic_files;
    pub mod filesystem;
    pub mod project_paths;
    pub mod settings_file;
    pub mod shell_service;
    pub mod startup_feedback;
    pub mod volumes;
}

#[cfg(windows)]
pub use windows::diagnostic_files::{
    DiagnosticFileError, DiagnosticFileErrorKind, DiagnosticFileOperation,
    DiagnosticRetentionReport, ExportDestinationKind, ExportWriteOutcome, MAX_DAILY_LOG_BYTES,
    MAX_DIAGNOSTIC_EXPORT_BYTES, MAX_PANIC_MARKER_BYTES, MAX_RETAINED_LOG_FILES,
    MarkerCommitStatus, MarkerReconcileAction, MarkerReconcileOutcome, MarkerSlotState,
    WindowsAtomicMarkerWriter, WindowsDailyLogHealth, WindowsDailyLogWriter, WindowsMarkerSession,
    classify_export_destination, prune_diagnostic_logs, write_diagnostic_export,
};
#[cfg(windows)]
pub use windows::filesystem::WindowsFileSystem;
#[cfg(windows)]
pub use windows::project_paths::{
    APPLICATION_IDENTIFIER, ProjectPathError, ProjectPathErrorKind, ProjectPathFacility,
    ProjectPathOperation, ProjectPaths, resolve_project_paths,
};
#[cfg(windows)]
pub use windows::settings_file::{
    MAX_SETTINGS_DOCUMENT_BYTES, SETTINGS_FILE_NAME, SettingsCommitOutcome, SettingsFileError,
    SettingsFileErrorKind, SettingsFileOperation, SettingsFileRead, WindowsSettingsFile,
};
#[cfg(windows)]
pub use windows::shell_service::{
    DialogError, DialogErrorStage, DialogEvent, DialogRequestId, FolderDialogRequest, OwnerWindow,
    ShellRequest, ShellService, ShellServiceConfig, ShellServiceShutdownError,
    ShellServiceStartError, ShellServiceStatus, SubmitError, TryReceiveError,
};
#[cfg(windows)]
pub use windows::startup_feedback::{attach_parent_console, show_startup_failure};
#[cfg(windows)]
pub use windows::volumes::{
    ResolvedScanRoot, VolumeDiscovery, VolumeError, WindowsVolume, discover_volumes,
    resolve_scan_root, scan_root_from_path,
};
