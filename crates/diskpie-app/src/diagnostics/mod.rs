//! Pure diagnostic schema, privacy redaction, and bounded export policy.
//!
//! This module deliberately owns no files, logger, network client, or tracing
//! subscriber. Platform adapters may feed it source bytes and persist the
//! returned artifact, but all privacy and size decisions remain testable here.

#![forbid(unsafe_code)]

pub mod events;
pub mod export;
pub mod redaction;

pub use events::{
    AllocationClass, AppVersion, Architecture, CoarseOsVersion, DiagnosticCode, DiagnosticEvent,
    DiagnosticField, DiagnosticLevel, ErrorClass, EventBuildError, MAX_EVENT_FIELDS,
    OperatingSystem, Outcome, PolicyValue, Target,
};
pub use export::{
    AggregateMetrics, AllowlistedSettings, DIAGNOSTICS_POLICY_VERSION, DiagnosticCounters,
    DiagnosticExport, EXPORT_SCHEMA_VERSION, ExportArtifact, ExportMetadata, ExportPreview,
    ExportRequest, ExportStats, LINE_TRUNCATION_MARKER, LogSource, MAX_EXPORT_BYTES,
    MAX_INPUT_LINE_BYTES, PreviewCategories, SizePreference, UiLocale, UiTheme, build_export,
};
pub use redaction::{
    REDACTED_ASSIGNMENT, REDACTED_CONTROL, REDACTED_PATH, REDACTED_QUERY, REDACTED_SECRET,
    REDACTED_URL, RedactedText, RedactionCounts, RedactionPolicy, redact,
};
