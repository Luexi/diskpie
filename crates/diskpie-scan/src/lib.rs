//! Bounded, cancellable filesystem scan coordination.

#![forbid(unsafe_code)]

mod coordinator;
mod fs;
mod protocol;

pub use coordinator::{CancelToken, ScanSession, start_scan, start_scan_with_cancel};
pub use fs::{
    DirectoryItem, FsEntry, FsError, FsErrorKind, FsOperation, ScanFs, ScanOmission, VisitControl,
};
pub use protocol::{
    CounterField, DirectoryCompletion, DirectoryState, PermitCaps, ScanBatch, ScanCounters,
    ScanEvent, ScanFailure, ScanGeneration, ScanNodeId, ScanOptions, ScanReceiveError, ScanRequest,
    ScanRoot, ScanTerminal, ScannedEntry, StartedRoot, StorageClass, TerminalState, ThreadRole,
    WorkerId,
};

pub use diskpie_core::PRODUCT_NAME;

#[cfg(test)]
mod tests;
