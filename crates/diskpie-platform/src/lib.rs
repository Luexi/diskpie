//! Filesystem and operating-system adapters for DiskPie.
//!
//! Project-authored unsafe code is permitted only in narrowly reviewed modules
//! under this crate's Windows adapter. Every unsafe block must document its
//! invariants and translate raw resources into RAII wrappers immediately.

#![deny(unsafe_op_in_unsafe_fn)]

pub use diskpie_core::PRODUCT_NAME;

#[cfg(windows)]
pub mod windows {
    pub mod filesystem;
    pub mod shell_service;
    pub mod volumes;
}

#[cfg(windows)]
pub use windows::filesystem::WindowsFileSystem;
#[cfg(windows)]
pub use windows::shell_service::{
    DialogError, DialogErrorStage, DialogEvent, DialogRequestId, FolderDialogRequest, OwnerWindow,
    ShellRequest, ShellService, ShellServiceConfig, ShellServiceShutdownError,
    ShellServiceStartError, ShellServiceStatus, SubmitError, TryReceiveError,
};
#[cfg(windows)]
pub use windows::volumes::{
    ResolvedScanRoot, VolumeDiscovery, VolumeError, WindowsVolume, discover_volumes,
    resolve_scan_root, scan_root_from_path,
};
