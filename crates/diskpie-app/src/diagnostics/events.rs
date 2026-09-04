//! Closed, path-free application diagnostic events.

#![forbid(unsafe_code)]

use std::{error::Error, fmt};

/// Maximum number of allowlisted fields carried by one event.
pub const MAX_EVENT_FIELDS: usize = 16;

/// A validated maximum diagnostic level.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum DiagnosticLevel {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl DiagnosticLevel {
    /// Returns whether this configured maximum admits `event_level`.
    #[must_use]
    pub const fn allows(self, event_level: Self) -> bool {
        let Some(maximum) = self.rank() else {
            return false;
        };
        let Some(event) = event_level.rank() else {
            return false;
        };
        event <= maximum
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    const fn rank(self) -> Option<u8> {
        match self {
            Self::Off => None,
            Self::Error => Some(0),
            Self::Warn => Some(1),
            Self::Info => Some(2),
            Self::Debug => Some(3),
        }
    }
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<crate::settings::DiagnosticLevel> for DiagnosticLevel {
    fn from(level: crate::settings::DiagnosticLevel) -> Self {
        match level {
            crate::settings::DiagnosticLevel::Off => Self::Off,
            crate::settings::DiagnosticLevel::Error => Self::Error,
            crate::settings::DiagnosticLevel::Warn => Self::Warn,
            crate::settings::DiagnosticLevel::Info => Self::Info,
            crate::settings::DiagnosticLevel::Debug => Self::Debug,
        }
    }
}

/// Stable event codes. Each code owns its production severity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiagnosticCode {
    AppStarted,
    AppStopped,
    ProjectPathsUnavailable,
    SettingsRecovered,
    SettingsUnavailable,
    SettingsPublished,
    SettingsPublishFailed,
    LoggingUnavailable,
    DiagnosticQueueDropped,
    DiagnosticWriteFailed,
    ExportPrepared,
    ScanRequested,
    ScanStarted,
    ScanProgress,
    ScanCompleted,
    ScanCancelled,
    ScanFailed,
    ScanOmission,
    RenderAllocationChanged,
    ShellActionRequested,
    ShellActionFailed,
    PanicMarkerFound,
    PanicMarkerUnavailable,
}

impl DiagnosticCode {
    /// Complete list, used by policy and uniqueness tests.
    pub const ALL: [Self; 23] = [
        Self::AppStarted,
        Self::AppStopped,
        Self::ProjectPathsUnavailable,
        Self::SettingsRecovered,
        Self::SettingsUnavailable,
        Self::SettingsPublished,
        Self::SettingsPublishFailed,
        Self::LoggingUnavailable,
        Self::DiagnosticQueueDropped,
        Self::DiagnosticWriteFailed,
        Self::ExportPrepared,
        Self::ScanRequested,
        Self::ScanStarted,
        Self::ScanProgress,
        Self::ScanCompleted,
        Self::ScanCancelled,
        Self::ScanFailed,
        Self::ScanOmission,
        Self::RenderAllocationChanged,
        Self::ShellActionRequested,
        Self::ShellActionFailed,
        Self::PanicMarkerFound,
        Self::PanicMarkerUnavailable,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppStarted => "app.started",
            Self::AppStopped => "app.stopped",
            Self::ProjectPathsUnavailable => "project_paths.unavailable",
            Self::SettingsRecovered => "settings.recovered",
            Self::SettingsUnavailable => "settings.unavailable",
            Self::SettingsPublished => "settings.published",
            Self::SettingsPublishFailed => "settings.publish_failed",
            Self::LoggingUnavailable => "diagnostics.logging_unavailable",
            Self::DiagnosticQueueDropped => "diagnostics.queue_dropped",
            Self::DiagnosticWriteFailed => "diagnostics.write_failed",
            Self::ExportPrepared => "diagnostics.export_prepared",
            Self::ScanRequested => "scan.requested",
            Self::ScanStarted => "scan.started",
            Self::ScanProgress => "scan.progress",
            Self::ScanCompleted => "scan.completed",
            Self::ScanCancelled => "scan.cancelled",
            Self::ScanFailed => "scan.failed",
            Self::ScanOmission => "scan.omission",
            Self::RenderAllocationChanged => "render.allocation_changed",
            Self::ShellActionRequested => "shell.action_requested",
            Self::ShellActionFailed => "shell.action_failed",
            Self::PanicMarkerFound => "panic.marker_found",
            Self::PanicMarkerUnavailable => "panic.marker_unavailable",
        }
    }

    #[must_use]
    pub const fn level(self) -> DiagnosticLevel {
        match self {
            Self::ScanFailed => DiagnosticLevel::Error,
            Self::ProjectPathsUnavailable
            | Self::SettingsRecovered
            | Self::SettingsUnavailable
            | Self::SettingsPublishFailed
            | Self::LoggingUnavailable
            | Self::DiagnosticQueueDropped
            | Self::DiagnosticWriteFailed
            | Self::ShellActionFailed
            | Self::PanicMarkerFound
            | Self::PanicMarkerUnavailable => DiagnosticLevel::Warn,
            Self::ScanProgress | Self::ScanOmission | Self::RenderAllocationChanged => {
                DiagnosticLevel::Debug
            }
            Self::AppStarted
            | Self::AppStopped
            | Self::SettingsPublished
            | Self::ExportPrepared
            | Self::ScanRequested
            | Self::ScanStarted
            | Self::ScanCompleted
            | Self::ScanCancelled
            | Self::ShellActionRequested => DiagnosticLevel::Info,
        }
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Numeric application version without a free-form prerelease payload.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct AppVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl AppVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }
}

impl fmt::Display for AppVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperatingSystem {
    Windows,
    Linux,
    MacOs,
    Other,
}

impl OperatingSystem {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Architecture {
    X86,
    X86_64,
    Arm,
    Aarch64,
    Other,
}

impl Architecture {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X86_64 => "x86_64",
            Self::Arm => "arm",
            Self::Aarch64 => "aarch64",
            Self::Other => "other",
        }
    }
}

/// Compile-time-closed build target.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Target {
    pub operating_system: OperatingSystem,
    pub architecture: Architecture,
}

impl Target {
    #[must_use]
    pub const fn new(operating_system: OperatingSystem, architecture: Architecture) -> Self {
        Self { operating_system, architecture }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.operating_system.as_str(), self.architecture.as_str())
    }
}

/// Coarse numeric OS version; it cannot carry edition, host, or user text.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct CoarseOsVersion {
    pub major: u16,
    pub minor: u16,
    pub build: Option<u32>,
}

impl CoarseOsVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16, build: Option<u32>) -> Self {
        Self { major, minor, build }
    }
}

impl fmt::Display for CoarseOsVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)?;
        if let Some(build) = self.build {
            write!(formatter, ".{build}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AllocationClass {
    Tiny,
    Small,
    Medium,
    Large,
    Huge,
}

impl AllocationClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Huge => "huge",
        }
    }
}

/// Selected non-path policy values useful to support.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PolicyValue {
    LogicalSize,
    AllocatedSize,
    ReparsePointsSkipped,
    OfflineFilesSkipped,
    NtfsFastBackend,
    PortableBackend,
    PathsExcluded,
    PathsIncludedByConsent,
}

impl PolicyValue {
    const fn as_str(self) -> &'static str {
        match self {
            Self::LogicalSize => "logical_size",
            Self::AllocatedSize => "allocated_size",
            Self::ReparsePointsSkipped => "reparse_points_skipped",
            Self::OfflineFilesSkipped => "offline_files_skipped",
            Self::NtfsFastBackend => "ntfs_fast_backend",
            Self::PortableBackend => "portable_backend",
            Self::PathsExcluded => "paths_excluded",
            Self::PathsIncludedByConsent => "paths_included_by_consent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    Degraded,
    Dropped,
    Disabled,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Degraded => "degraded",
            Self::Dropped => "dropped",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ErrorClass {
    AccessDenied,
    NotFound,
    InvalidData,
    Unsupported,
    ResourceExhausted,
    Busy,
    Io,
    Internal,
}

impl ErrorClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
            Self::NotFound => "not_found",
            Self::InvalidData => "invalid_data",
            Self::Unsupported => "unsupported",
            Self::ResourceExhausted => "resource_exhausted",
            Self::Busy => "busy",
            Self::Io => "io",
            Self::Internal => "internal",
        }
    }
}

/// The complete application-event field allowlist.
///
/// No variant accepts `String`, `str`, `Path`, `OsStr`, command arguments, or
/// another free-form payload. An adapter can therefore format this value
/// without first trusting call-site redaction.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiagnosticField {
    AppVersion(AppVersion),
    Target(Target),
    OsVersion(CoarseOsVersion),
    GenerationId(u64),
    RequestId(u64),
    ItemCount(u64),
    DirectoryCount(u64),
    FileCount(u64),
    OmissionCount(u64),
    ByteCount(u64),
    DurationMillis(u64),
    QueueDepth(u32),
    WorkerCount(u16),
    AllocationClass(AllocationClass),
    Policy(PolicyValue),
    Outcome(Outcome),
    ErrorClass(ErrorClass),
}

impl DiagnosticField {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AppVersion(_) => "app_version",
            Self::Target(_) => "target",
            Self::OsVersion(_) => "os_version",
            Self::GenerationId(_) => "generation_id",
            Self::RequestId(_) => "request_id",
            Self::ItemCount(_) => "item_count",
            Self::DirectoryCount(_) => "directory_count",
            Self::FileCount(_) => "file_count",
            Self::OmissionCount(_) => "omission_count",
            Self::ByteCount(_) => "byte_count",
            Self::DurationMillis(_) => "duration_ms",
            Self::QueueDepth(_) => "queue_depth",
            Self::WorkerCount(_) => "worker_count",
            Self::AllocationClass(_) => "allocation_class",
            Self::Policy(_) => "policy",
            Self::Outcome(_) => "outcome",
            Self::ErrorClass(_) => "error_class",
        }
    }
}

impl fmt::Display for DiagnosticField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppVersion(value) => value.fmt(formatter),
            Self::Target(value) => value.fmt(formatter),
            Self::OsVersion(value) => value.fmt(formatter),
            Self::GenerationId(value)
            | Self::RequestId(value)
            | Self::ItemCount(value)
            | Self::DirectoryCount(value)
            | Self::FileCount(value)
            | Self::OmissionCount(value)
            | Self::ByteCount(value)
            | Self::DurationMillis(value) => value.fmt(formatter),
            Self::QueueDepth(value) => value.fmt(formatter),
            Self::WorkerCount(value) => value.fmt(formatter),
            Self::AllocationClass(value) => formatter.write_str(value.as_str()),
            Self::Policy(value) => formatter.write_str(value.as_str()),
            Self::Outcome(value) => formatter.write_str(value.as_str()),
            Self::ErrorClass(value) => formatter.write_str(value.as_str()),
        }
    }
}

/// A bounded application event with no free-form message slot.
///
/// ```compile_fail
/// use std::path::Path;
/// use diskpie_app::diagnostics::{DiagnosticCode, DiagnosticEvent};
///
/// let path = Path::new(r"C:\\private\\name.txt");
/// let _ = DiagnosticEvent::new(DiagnosticCode::ScanStarted).with_field(path);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticEvent {
    code: DiagnosticCode,
    fields: [Option<DiagnosticField>; MAX_EVENT_FIELDS],
    field_count: u8,
}

impl DiagnosticEvent {
    #[must_use]
    pub const fn new(code: DiagnosticCode) -> Self {
        Self { code, fields: [None; MAX_EVENT_FIELDS], field_count: 0 }
    }

    pub fn from_fields(
        code: DiagnosticCode,
        fields: &[DiagnosticField],
    ) -> Result<Self, EventBuildError> {
        let mut event = Self::new(code);
        for field in fields {
            event.push_field(*field)?;
        }
        Ok(event)
    }

    #[must_use]
    pub const fn code(&self) -> DiagnosticCode {
        self.code
    }

    #[must_use]
    pub const fn level(&self) -> DiagnosticLevel {
        self.code.level()
    }

    #[must_use]
    pub const fn field_count(&self) -> usize {
        self.field_count as usize
    }

    pub fn fields(&self) -> impl DoubleEndedIterator<Item = &DiagnosticField> + ExactSizeIterator {
        self.fields[..self.field_count()]
            .iter()
            .map(|field| field.as_ref().expect("occupied diagnostic field prefix"))
    }

    pub fn push_field(&mut self, field: DiagnosticField) -> Result<(), EventBuildError> {
        let index = self.field_count();
        let Some(slot) = self.fields.get_mut(index) else {
            return Err(EventBuildError::TooManyFields { maximum: MAX_EVENT_FIELDS });
        };
        *slot = Some(field);
        self.field_count += 1;
        Ok(())
    }

    pub fn with_field(mut self, field: DiagnosticField) -> Result<Self, EventBuildError> {
        self.push_field(field)?;
        Ok(self)
    }
}

impl fmt::Display for DiagnosticEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "level={} code={}", self.level(), self.code())?;
        for field in self.fields() {
            write!(formatter, " {}={field}", field.name())?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventBuildError {
    TooManyFields { maximum: usize },
}

impl fmt::Display for EventBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyFields { maximum } => {
                write!(formatter, "diagnostic event exceeds the {maximum}-field limit")
            }
        }
    }
}

impl Error for EventBuildError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_levels_are_fixed_and_monotonic() {
        assert!(!DiagnosticLevel::Off.allows(DiagnosticLevel::Error));
        assert!(DiagnosticLevel::Error.allows(DiagnosticLevel::Error));
        assert!(!DiagnosticLevel::Error.allows(DiagnosticLevel::Warn));
        assert!(DiagnosticLevel::Info.allows(DiagnosticLevel::Error));
        assert!(DiagnosticLevel::Info.allows(DiagnosticLevel::Warn));
        assert!(DiagnosticLevel::Info.allows(DiagnosticLevel::Info));
        assert!(!DiagnosticLevel::Info.allows(DiagnosticLevel::Debug));
        assert!(DiagnosticLevel::Debug.allows(DiagnosticLevel::Debug));
        assert!(!DiagnosticLevel::Debug.allows(DiagnosticLevel::Off));
    }

    #[test]
    fn persisted_levels_map_exhaustively_to_the_runtime_filter() {
        use crate::settings::DiagnosticLevel as Persisted;

        for (persisted, expected) in [
            (Persisted::Off, DiagnosticLevel::Off),
            (Persisted::Error, DiagnosticLevel::Error),
            (Persisted::Warn, DiagnosticLevel::Warn),
            (Persisted::Info, DiagnosticLevel::Info),
            (Persisted::Debug, DiagnosticLevel::Debug),
        ] {
            assert_eq!(DiagnosticLevel::from(persisted), expected);
        }
    }

    #[test]
    fn stable_codes_are_unique_and_nonempty() {
        for (index, code) in DiagnosticCode::ALL.iter().enumerate() {
            assert!(!code.as_str().is_empty());
            for other in &DiagnosticCode::ALL[index + 1..] {
                assert_ne!(code.as_str(), other.as_str());
            }
        }
    }

    #[test]
    fn event_golden_has_only_stable_allowlisted_fields() {
        let event = DiagnosticEvent::from_fields(
            DiagnosticCode::ScanCompleted,
            &[
                DiagnosticField::GenerationId(7),
                DiagnosticField::ItemCount(42),
                DiagnosticField::DurationMillis(125),
                DiagnosticField::Outcome(Outcome::Succeeded),
            ],
        )
        .expect("four fields fit");

        assert_eq!(
            event.to_string(),
            "level=info code=scan.completed generation_id=7 item_count=42 duration_ms=125 outcome=succeeded"
        );
    }

    #[test]
    fn event_field_storage_is_strictly_bounded() {
        let mut event = DiagnosticEvent::new(DiagnosticCode::ScanProgress);
        for value in 0..MAX_EVENT_FIELDS {
            event.push_field(DiagnosticField::ItemCount(value as u64)).expect("field within limit");
        }
        assert_eq!(event.field_count(), MAX_EVENT_FIELDS);
        assert_eq!(
            event.push_field(DiagnosticField::ItemCount(99)),
            Err(EventBuildError::TooManyFields { maximum: MAX_EVENT_FIELDS })
        );
    }

    #[test]
    fn version_target_and_os_are_numeric_or_closed() {
        let fields = [
            DiagnosticField::AppVersion(AppVersion::new(1, 2, 3)),
            DiagnosticField::Target(Target::new(OperatingSystem::Windows, Architecture::X86_64)),
            DiagnosticField::OsVersion(CoarseOsVersion::new(10, 0, Some(26_100))),
        ];
        let event = DiagnosticEvent::from_fields(DiagnosticCode::AppStarted, &fields)
            .expect("three fields fit");

        assert_eq!(
            event.to_string(),
            "level=info code=app.started app_version=1.2.3 target=windows-x86_64 os_version=10.0.26100"
        );
    }
}
