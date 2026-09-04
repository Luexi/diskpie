//! Bounded, preview-exact diagnostic export composition.

#![forbid(unsafe_code)]

use super::{
    events::{AppVersion, CoarseOsVersion, DiagnosticLevel, Target},
    redaction::{RedactionCounts, RedactionPolicy, redact},
};
use std::{fmt, fmt::Write as _, sync::Arc};

pub const EXPORT_SCHEMA_VERSION: u32 = 1;
pub const DIAGNOSTICS_POLICY_VERSION: u32 = 1;
pub const MAX_EXPORT_BYTES: usize = 8 * 1_024 * 1_024;
pub const MAX_INPUT_LINE_BYTES: usize = 16 * 1_024;
pub const LINE_TRUNCATION_MARKER: &str = "<truncated:line>";

const EXPORT_HEADER: &str = "DISKPIE_DIAGNOSTIC_EXPORT";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiLocale {
    #[default]
    EnglishUnitedStates,
    SpanishMexico,
}

impl UiLocale {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EnglishUnitedStates => "en-US",
            Self::SpanishMexico => "es-MX",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiTheme {
    #[default]
    System,
    Dark,
    Light,
}

impl UiTheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SizePreference {
    Logical,
    #[default]
    Allocated,
}

impl SizePreference {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Logical => "logical",
            Self::Allocated => "allocated",
        }
    }
}

/// Explicit path-free settings allowlist. Scales are fixed-point so NaN and
/// arbitrary serialized settings can never enter the export boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllowlistedSettings {
    pub locale: UiLocale,
    pub theme: UiTheme,
    pub size_preference: SizePreference,
    pub show_both_sizes: bool,
    pub ui_scale_milli: Option<u16>,
    pub layout_max_sectors: u32,
    pub layout_max_depth: u8,
    pub layout_minimum_sweep_millionths: u32,
    pub scan_workers: u16,
    pub diagnostic_level: DiagnosticLevel,
    pub explorer_integration: bool,
}

impl Default for AllowlistedSettings {
    fn default() -> Self {
        Self {
            locale: UiLocale::EnglishUnitedStates,
            theme: UiTheme::System,
            size_preference: SizePreference::Allocated,
            show_both_sizes: true,
            ui_scale_milli: None,
            layout_max_sectors: 10_000,
            layout_max_depth: 8,
            layout_minimum_sweep_millionths: 2_000,
            scan_workers: 0,
            diagnostic_level: DiagnosticLevel::Info,
            explorer_integration: false,
        }
    }
}

impl From<&crate::settings::Settings> for AllowlistedSettings {
    fn from(settings: &crate::settings::Settings) -> Self {
        use crate::settings::UiTheme as StoredTheme;
        use crate::settings::{SizePreference as StoredSize, UiLocale as StoredLocale};

        let (settings, _corrections) = settings.clone().validated();
        Self {
            locale: match settings.locale {
                StoredLocale::EnglishUnitedStates => UiLocale::EnglishUnitedStates,
                StoredLocale::SpanishMexico => UiLocale::SpanishMexico,
            },
            theme: match settings.theme {
                StoredTheme::System => UiTheme::System,
                StoredTheme::Dark => UiTheme::Dark,
                StoredTheme::Light => UiTheme::Light,
            },
            size_preference: match settings.size_preference {
                StoredSize::Logical => SizePreference::Logical,
                StoredSize::Allocated => SizePreference::Allocated,
            },
            show_both_sizes: settings.show_both_sizes,
            ui_scale_milli: settings.ui_scale.map(|scale| (scale * 1_000.0).round() as u16),
            layout_max_sectors: settings.layout_max_sectors,
            layout_max_depth: settings.layout_max_depth,
            layout_minimum_sweep_millionths: (settings.layout_minimum_sweep * 1_000_000.0).round()
                as u32,
            scan_workers: settings.scan_workers,
            diagnostic_level: settings.diagnostic_level.into(),
            explorer_integration: settings.explorer_integration,
        }
    }
}

/// Path-free aggregate support metrics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AggregateMetrics {
    pub scan_generations: u64,
    pub completed_scans: u64,
    pub failed_scans: u64,
    pub cancelled_scans: u64,
    pub items_observed: u64,
    pub bytes_observed: u64,
    pub scan_omissions: u64,
    pub total_scan_duration_millis: u64,
}

/// Health counters supplied by the logging adapter. The policy itself neither
/// creates a writer nor reads these from tracing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticCounters {
    pub dropped_events: u64,
    pub write_failures: u64,
    pub sink_truncated_lines: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportMetadata {
    pub app_version: AppVersion,
    pub target: Target,
    pub coarse_os_version: CoarseOsVersion,
    pub panic_marker_present: bool,
}

/// One source ordered from oldest to newest in [`ExportRequest::log_sources`].
/// It intentionally has no filename or path field.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct LogSource<'a> {
    bytes: Option<&'a [u8]>,
}

impl fmt::Debug for LogSource<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("LogSource");
        match self.bytes {
            Some(bytes) => {
                debug.field("available", &true).field("byte_len", &bytes.len());
            }
            None => {
                debug.field("available", &false);
            }
        }
        debug.finish()
    }
}

impl<'a> LogSource<'a> {
    #[must_use]
    pub const fn available(bytes: &'a [u8]) -> Self {
        Self { bytes: Some(bytes) }
    }

    /// Represents a source the I/O adapter could not read, without exposing
    /// its name or error message.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self { bytes: None }
    }
}

/// Immutable inputs to pure export composition.
#[derive(Clone, Copy, Debug)]
pub struct ExportRequest<'a> {
    pub metadata: ExportMetadata,
    pub settings: AllowlistedSettings,
    pub aggregates: AggregateMetrics,
    pub counters: DiagnosticCounters,
    pub log_sources: &'a [LogSource<'a>],
    /// Per-export consent; callers should default this to `false` every time.
    pub include_paths: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportStats {
    pub artifact_bytes: u64,
    pub source_files: u64,
    pub unreadable_source_files: u64,
    pub included_source_files: u64,
    pub omitted_source_files: u64,
    pub complete_input_lines: u64,
    pub included_lines: u64,
    pub omitted_lines: u64,
    pub partial_lines_omitted: u64,
    pub capacity_lines_omitted: u64,
    pub truncated_input_lines: u64,
    pub invalid_utf8_lines: u64,
    pub dropped_events: u64,
    pub write_failures: u64,
    pub sink_truncated_lines: u64,
    pub redactions: RedactionCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreviewCategories {
    pub fixed_header: bool,
    pub system_versions: bool,
    pub allowlisted_settings: bool,
    pub aggregate_metrics: bool,
    pub omission_and_health_statistics: bool,
    pub panic_marker_presence: bool,
    pub sanitized_recent_logs: bool,
    pub filesystem_paths_by_consent: bool,
}

/// Bytes to persist. Construction is private so the 8 MiB invariant cannot be
/// bypassed accidentally.
#[derive(Clone)]
pub struct ExportArtifact {
    bytes: Arc<[u8]>,
}

impl ExportArtifact {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    #[must_use]
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("export construction emits only valid UTF-8")
    }
}

impl fmt::Debug for ExportArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ExportArtifact").field("byte_len", &self.bytes.len()).finish()
    }
}

/// Inspect-before-save view over the exact artifact allocation.
#[derive(Clone)]
pub struct ExportPreview {
    bytes: Arc<[u8]>,
    categories: PreviewCategories,
    stats: ExportStats,
}

impl ExportPreview {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    #[must_use]
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("export construction emits only valid UTF-8")
    }

    #[must_use]
    pub const fn categories(&self) -> PreviewCategories {
        self.categories
    }

    #[must_use]
    pub const fn stats(&self) -> ExportStats {
        self.stats
    }
}

impl fmt::Debug for ExportPreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExportPreview")
            .field("byte_len", &self.bytes.len())
            .field("categories", &self.categories)
            .field("stats", &self.stats)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct DiagnosticExport {
    artifact: ExportArtifact,
    preview: ExportPreview,
}

impl DiagnosticExport {
    #[must_use]
    pub const fn artifact(&self) -> &ExportArtifact {
        &self.artifact
    }

    #[must_use]
    pub const fn preview(&self) -> &ExportPreview {
        &self.preview
    }

    #[must_use]
    pub const fn stats(&self) -> ExportStats {
        self.preview.stats
    }

    /// Stronger than byte equality: preview and save use the same allocation.
    #[must_use]
    pub fn preview_is_exact_artifact(&self) -> bool {
        Arc::ptr_eq(&self.artifact.bytes, &self.preview.bytes)
    }
}

/// Produces a complete UTF-8 export without performing I/O or initializing a
/// logger/network client. Only complete newline-terminated input records are
/// candidates, and sources must be ordered oldest to newest.
#[must_use]
pub fn build_export(request: &ExportRequest<'_>) -> DiagnosticExport {
    let (lines, preparation) = prepare_lines(request);
    let selection = choose_newest_suffix(request, &lines, preparation);
    let mut stats = selection.stats;
    let mut artifact = render_prefix(request, stats);
    for line in &lines[selection.start..] {
        artifact.extend_from_slice(line.text.as_bytes());
        artifact.push(b'\n');
    }
    debug_assert!(artifact.len() <= MAX_EXPORT_BYTES);
    debug_assert!(std::str::from_utf8(&artifact).is_ok());

    stats.artifact_bytes = usize_to_u64(artifact.len());
    let bytes: Arc<[u8]> = Arc::from(artifact.into_boxed_slice());
    let categories = PreviewCategories {
        fixed_header: true,
        system_versions: true,
        allowlisted_settings: true,
        aggregate_metrics: true,
        omission_and_health_statistics: true,
        panic_marker_presence: true,
        sanitized_recent_logs: stats.included_lines != 0,
        filesystem_paths_by_consent: request.include_paths,
    };
    DiagnosticExport {
        artifact: ExportArtifact { bytes: Arc::clone(&bytes) },
        preview: ExportPreview { bytes, categories, stats },
    }
}

#[derive(Clone, Debug)]
struct PreparedLine {
    source_index: usize,
    text: String,
    redactions: RedactionCounts,
}

#[derive(Clone, Copy, Debug, Default)]
struct PreparationStats {
    source_files: u64,
    unreadable_source_files: u64,
    complete_input_lines: u64,
    partial_lines_omitted: u64,
    truncated_input_lines: u64,
    invalid_utf8_lines: u64,
}

fn prepare_lines(request: &ExportRequest<'_>) -> (Vec<PreparedLine>, PreparationStats) {
    let mut lines = Vec::new();
    let mut stats = PreparationStats {
        source_files: usize_to_u64(request.log_sources.len()),
        ..PreparationStats::default()
    };
    let policy = RedactionPolicy { include_paths: request.include_paths };

    for (source_index, source) in request.log_sources.iter().enumerate() {
        let Some(bytes) = source.bytes else {
            stats.unreadable_source_files = stats.unreadable_source_files.saturating_add(1);
            continue;
        };
        let mut start = 0;
        for (end, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let mut raw_line = &bytes[start..end];
            if raw_line.ends_with(b"\r") {
                raw_line = &raw_line[..raw_line.len() - 1];
            }
            let prepared = prepare_line(raw_line, policy);
            stats.complete_input_lines = stats.complete_input_lines.saturating_add(1);
            if prepared.truncated {
                stats.truncated_input_lines = stats.truncated_input_lines.saturating_add(1);
            }
            if prepared.invalid_utf8 {
                stats.invalid_utf8_lines = stats.invalid_utf8_lines.saturating_add(1);
            }
            lines.push(PreparedLine {
                source_index,
                text: prepared.text,
                redactions: prepared.redactions,
            });
            start = end + 1;
        }
        if start < bytes.len() {
            stats.partial_lines_omitted = stats.partial_lines_omitted.saturating_add(1);
        }
    }
    (lines, stats)
}

struct PreparedText {
    text: String,
    redactions: RedactionCounts,
    truncated: bool,
    invalid_utf8: bool,
}

fn prepare_line(raw_line: &[u8], policy: RedactionPolicy) -> PreparedText {
    let invalid_utf8 = std::str::from_utf8(raw_line).is_err();
    let mut truncated = raw_line.len() > MAX_INPUT_LINE_BYTES;
    let decoded = if truncated {
        let prefix_budget = MAX_INPUT_LINE_BYTES.saturating_sub(LINE_TRUNCATION_MARKER.len());
        let end = utf8_prefix_boundary(raw_line, prefix_budget);
        let mut text = String::from_utf8_lossy(&raw_line[..end]).into_owned();
        text.push_str(LINE_TRUNCATION_MARKER);
        text
    } else {
        String::from_utf8_lossy(raw_line).into_owned()
    };
    let redacted = redact(&decoded, policy);
    let redactions = redacted.counts();
    let mut text = redacted.into_string();
    if text.len() > MAX_INPUT_LINE_BYTES {
        truncate_utf8_with_marker(&mut text, MAX_INPUT_LINE_BYTES);
        truncated = true;
    }
    PreparedText { text, redactions, truncated, invalid_utf8 }
}

fn utf8_prefix_boundary(bytes: &[u8], maximum: usize) -> usize {
    let mut end = bytes.len().min(maximum);
    while end > 0 && bytes.get(end).is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000) {
        end -= 1;
    }
    end
}

fn truncate_utf8_with_marker(text: &mut String, maximum: usize) {
    let mut end = maximum.saturating_sub(LINE_TRUNCATION_MARKER.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(LINE_TRUNCATION_MARKER);
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    start: usize,
    stats: ExportStats,
}

fn choose_newest_suffix(
    request: &ExportRequest<'_>,
    lines: &[PreparedLine],
    preparation: PreparationStats,
) -> Selection {
    let mut included_sources = vec![false; request.log_sources.len()];
    let mut included_source_count = 0_u64;
    let mut suffix_bytes = 0_usize;
    let mut suffix_redactions = RedactionCounts::default();
    let mut best = Selection {
        start: lines.len(),
        stats: make_stats(request, preparation, 0, 0, RedactionCounts::default()),
    };

    // The no-log artifact is fixed and comfortably below the cap.
    debug_assert!(render_prefix(request, best.stats).len() <= MAX_EXPORT_BYTES);

    for start in (0..lines.len()).rev() {
        let line = &lines[start];
        suffix_bytes = suffix_bytes.saturating_add(line.text.len().saturating_add(1));
        suffix_redactions.merge(line.redactions);
        if !included_sources[line.source_index] {
            included_sources[line.source_index] = true;
            included_source_count = included_source_count.saturating_add(1);
        }
        let included_lines = usize_to_u64(lines.len() - start);
        let stats = make_stats(
            request,
            preparation,
            included_lines,
            included_source_count,
            suffix_redactions,
        );
        let prefix_len = render_prefix(request, stats).len();
        if prefix_len.saturating_add(suffix_bytes) <= MAX_EXPORT_BYTES {
            best = Selection { start, stats };
        }
    }
    best
}

fn make_stats(
    request: &ExportRequest<'_>,
    preparation: PreparationStats,
    included_lines: u64,
    included_source_files: u64,
    redactions: RedactionCounts,
) -> ExportStats {
    let capacity_lines_omitted = preparation.complete_input_lines.saturating_sub(included_lines);
    ExportStats {
        artifact_bytes: 0,
        source_files: preparation.source_files,
        unreadable_source_files: preparation.unreadable_source_files,
        included_source_files,
        omitted_source_files: preparation.source_files.saturating_sub(included_source_files),
        complete_input_lines: preparation.complete_input_lines,
        included_lines,
        omitted_lines: capacity_lines_omitted.saturating_add(preparation.partial_lines_omitted),
        partial_lines_omitted: preparation.partial_lines_omitted,
        capacity_lines_omitted,
        truncated_input_lines: preparation.truncated_input_lines,
        invalid_utf8_lines: preparation.invalid_utf8_lines,
        dropped_events: request.counters.dropped_events,
        write_failures: request.counters.write_failures,
        sink_truncated_lines: request.counters.sink_truncated_lines,
        redactions,
    }
}

fn render_prefix(request: &ExportRequest<'_>, stats: ExportStats) -> Vec<u8> {
    let mut output = String::with_capacity(2_048);
    writeln!(output, "{EXPORT_HEADER}").expect("writing to String cannot fail");
    writeln!(output, "schema_version={EXPORT_SCHEMA_VERSION}")
        .expect("writing to String cannot fail");
    writeln!(output, "diagnostics_policy_version={DIAGNOSTICS_POLICY_VERSION}")
        .expect("writing to String cannot fail");
    writeln!(output, "encoding=utf-8").expect("writing to String cannot fail");
    writeln!(output, "paths_included_by_consent={}", request.include_paths)
        .expect("writing to String cannot fail");

    writeln!(output, "\n[system]").expect("writing to String cannot fail");
    writeln!(output, "app_version={}", request.metadata.app_version)
        .expect("writing to String cannot fail");
    writeln!(output, "target={}", request.metadata.target).expect("writing to String cannot fail");
    writeln!(output, "coarse_os_version={}", request.metadata.coarse_os_version)
        .expect("writing to String cannot fail");

    let settings = request.settings;
    writeln!(output, "\n[settings]").expect("writing to String cannot fail");
    writeln!(output, "locale={}", settings.locale.as_str()).expect("writing to String cannot fail");
    writeln!(output, "theme={}", settings.theme.as_str()).expect("writing to String cannot fail");
    writeln!(output, "size_preference={}", settings.size_preference.as_str())
        .expect("writing to String cannot fail");
    writeln!(output, "show_both_sizes={}", settings.show_both_sizes)
        .expect("writing to String cannot fail");
    match settings.ui_scale_milli {
        Some(scale) => writeln!(output, "ui_scale_milli={scale}"),
        None => writeln!(output, "ui_scale_milli=automatic"),
    }
    .expect("writing to String cannot fail");
    writeln!(output, "layout_max_sectors={}", settings.layout_max_sectors)
        .expect("writing to String cannot fail");
    writeln!(output, "layout_max_depth={}", settings.layout_max_depth)
        .expect("writing to String cannot fail");
    writeln!(
        output,
        "layout_minimum_sweep_millionths={}",
        settings.layout_minimum_sweep_millionths
    )
    .expect("writing to String cannot fail");
    writeln!(output, "scan_workers={}", settings.scan_workers)
        .expect("writing to String cannot fail");
    writeln!(output, "diagnostic_level={}", settings.diagnostic_level)
        .expect("writing to String cannot fail");
    writeln!(output, "explorer_integration={}", settings.explorer_integration)
        .expect("writing to String cannot fail");

    let aggregates = request.aggregates;
    writeln!(output, "\n[aggregates]").expect("writing to String cannot fail");
    writeln!(output, "scan_generations={}", aggregates.scan_generations)
        .expect("writing to String cannot fail");
    writeln!(output, "completed_scans={}", aggregates.completed_scans)
        .expect("writing to String cannot fail");
    writeln!(output, "failed_scans={}", aggregates.failed_scans)
        .expect("writing to String cannot fail");
    writeln!(output, "cancelled_scans={}", aggregates.cancelled_scans)
        .expect("writing to String cannot fail");
    writeln!(output, "items_observed={}", aggregates.items_observed)
        .expect("writing to String cannot fail");
    writeln!(output, "bytes_observed={}", aggregates.bytes_observed)
        .expect("writing to String cannot fail");
    writeln!(output, "scan_omissions={}", aggregates.scan_omissions)
        .expect("writing to String cannot fail");
    writeln!(output, "total_scan_duration_ms={}", aggregates.total_scan_duration_millis)
        .expect("writing to String cannot fail");

    writeln!(output, "\n[statistics]").expect("writing to String cannot fail");
    writeln!(output, "source_files={}", stats.source_files).expect("writing to String cannot fail");
    writeln!(output, "unreadable_source_files={}", stats.unreadable_source_files)
        .expect("writing to String cannot fail");
    writeln!(output, "included_source_files={}", stats.included_source_files)
        .expect("writing to String cannot fail");
    writeln!(output, "omitted_source_files={}", stats.omitted_source_files)
        .expect("writing to String cannot fail");
    writeln!(output, "complete_input_lines={}", stats.complete_input_lines)
        .expect("writing to String cannot fail");
    writeln!(output, "included_lines={}", stats.included_lines)
        .expect("writing to String cannot fail");
    writeln!(output, "omitted_lines={}", stats.omitted_lines)
        .expect("writing to String cannot fail");
    writeln!(output, "partial_lines_omitted={}", stats.partial_lines_omitted)
        .expect("writing to String cannot fail");
    writeln!(output, "capacity_lines_omitted={}", stats.capacity_lines_omitted)
        .expect("writing to String cannot fail");
    writeln!(output, "truncated_input_lines={}", stats.truncated_input_lines)
        .expect("writing to String cannot fail");
    writeln!(output, "invalid_utf8_lines={}", stats.invalid_utf8_lines)
        .expect("writing to String cannot fail");
    writeln!(output, "appender_dropped_events={}", stats.dropped_events)
        .expect("writing to String cannot fail");
    writeln!(output, "appender_write_failures={}", stats.write_failures)
        .expect("writing to String cannot fail");
    writeln!(output, "appender_truncated_lines={}", stats.sink_truncated_lines)
        .expect("writing to String cannot fail");
    writeln!(output, "redacted_paths={}", stats.redactions.paths)
        .expect("writing to String cannot fail");
    writeln!(output, "redacted_urls={}", stats.redactions.urls)
        .expect("writing to String cannot fail");
    writeln!(output, "redacted_query_strings={}", stats.redactions.query_strings)
        .expect("writing to String cannot fail");
    writeln!(output, "redacted_assignments={}", stats.redactions.assignments)
        .expect("writing to String cannot fail");
    writeln!(output, "redacted_secrets={}", stats.redactions.secrets)
        .expect("writing to String cannot fail");
    writeln!(output, "redacted_control_runs={}", stats.redactions.control_runs)
        .expect("writing to String cannot fail");

    writeln!(output, "\n[notices]").expect("writing to String cannot fail");
    let mut any_notice = false;
    if stats.omitted_source_files != 0 || stats.omitted_lines != 0 {
        writeln!(
            output,
            "NOTICE omission files={} lines={} partial={} capacity={}",
            stats.omitted_source_files,
            stats.omitted_lines,
            stats.partial_lines_omitted,
            stats.capacity_lines_omitted
        )
        .expect("writing to String cannot fail");
        any_notice = true;
    }
    if stats.dropped_events != 0 {
        writeln!(output, "NOTICE appender_drop count={}", stats.dropped_events)
            .expect("writing to String cannot fail");
        any_notice = true;
    }
    if stats.write_failures != 0 {
        writeln!(output, "NOTICE appender_write_failure count={}", stats.write_failures)
            .expect("writing to String cannot fail");
        any_notice = true;
    }
    if stats.truncated_input_lines != 0 || stats.sink_truncated_lines != 0 {
        writeln!(
            output,
            "NOTICE line_truncation input={} appender={}",
            stats.truncated_input_lines, stats.sink_truncated_lines
        )
        .expect("writing to String cannot fail");
        any_notice = true;
    }
    if stats.invalid_utf8_lines != 0 {
        writeln!(output, "NOTICE invalid_utf8 lines={}", stats.invalid_utf8_lines)
            .expect("writing to String cannot fail");
        any_notice = true;
    }
    if !any_notice {
        writeln!(output, "notice=none").expect("writing to String cannot fail");
    }

    writeln!(output, "\n[panic]").expect("writing to String cannot fail");
    writeln!(output, "marker_present={}", request.metadata.panic_marker_present)
        .expect("writing to String cannot fail");
    writeln!(output, "\n[recent_logs]").expect("writing to String cannot fail");
    output.into_bytes()
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::super::events::{Architecture, OperatingSystem};
    use super::*;

    fn metadata() -> ExportMetadata {
        ExportMetadata {
            app_version: AppVersion::new(0, 1, 0),
            target: Target::new(OperatingSystem::Windows, Architecture::X86_64),
            coarse_os_version: CoarseOsVersion::new(10, 0, Some(26_100)),
            panic_marker_present: false,
        }
    }

    fn request<'a>(sources: &'a [LogSource<'a>]) -> ExportRequest<'a> {
        ExportRequest {
            metadata: metadata(),
            settings: AllowlistedSettings::default(),
            aggregates: AggregateMetrics::default(),
            counters: DiagnosticCounters::default(),
            log_sources: sources,
            include_paths: false,
        }
    }

    #[test]
    fn settings_allowlist_is_derived_only_from_validated_path_free_fields() {
        let stored = crate::settings::Settings {
            locale: crate::settings::UiLocale::SpanishMexico,
            theme: crate::settings::UiTheme::Dark,
            size_preference: crate::settings::SizePreference::Logical,
            show_both_sizes: false,
            ui_scale: Some(f32::INFINITY),
            layout_max_sectors: u32::MAX,
            layout_max_depth: 0,
            layout_minimum_sweep: f64::NAN,
            scan_workers: u16::MAX,
            diagnostic_level: crate::settings::DiagnosticLevel::Warn,
            explorer_integration: true,
        };

        let allowlisted = AllowlistedSettings::from(&stored);
        assert_eq!(allowlisted.locale, UiLocale::SpanishMexico);
        assert_eq!(allowlisted.theme, UiTheme::Dark);
        assert_eq!(allowlisted.size_preference, SizePreference::Logical);
        assert!(!allowlisted.show_both_sizes);
        assert_eq!(allowlisted.ui_scale_milli, None);
        assert_eq!(allowlisted.layout_max_sectors, 50_000);
        assert_eq!(allowlisted.layout_max_depth, 1);
        assert_eq!(allowlisted.layout_minimum_sweep_millionths, 2_000);
        assert_eq!(allowlisted.scan_workers, 64);
        assert_eq!(allowlisted.diagnostic_level, DiagnosticLevel::Warn);
        assert!(allowlisted.explorer_integration);
    }

    #[test]
    fn preview_and_artifact_are_the_exact_same_arc() {
        let sources = [LogSource::available(b"line\n")];
        let export = build_export(&request(&sources));

        assert!(export.preview_is_exact_artifact());
        assert_eq!(export.artifact().bytes(), export.preview().bytes());
        assert!(Arc::ptr_eq(&export.artifact().shared_bytes(), &export.preview().shared_bytes()));
        assert!(export.artifact().bytes().len() <= MAX_EXPORT_BYTES);
        assert!(std::str::from_utf8(export.artifact().bytes()).is_ok());
    }

    #[test]
    fn log_source_debug_exposes_only_availability_and_length() {
        let source = LogSource::available(b"token=private");
        let debug = format!("{source:?}");

        assert_eq!(debug, "LogSource { available: true, byte_len: 13 }");
        assert!(!debug.contains("private"));
    }

    #[test]
    fn golden_export_is_stable_and_records_notices() {
        let sources = [
            LogSource::unavailable(),
            LogSource::available(b"old line\npartial"),
            LogSource::available(b"open C:\\Users\\Luis\\private.txt\ntoken=private\n"),
        ];
        let mut request = request(&sources);
        request.aggregates = AggregateMetrics {
            scan_generations: 2,
            completed_scans: 1,
            failed_scans: 1,
            cancelled_scans: 0,
            items_observed: 42,
            bytes_observed: 1_024,
            scan_omissions: 3,
            total_scan_duration_millis: 250,
        };
        request.counters =
            DiagnosticCounters { dropped_events: 4, write_failures: 2, sink_truncated_lines: 1 };
        request.metadata.panic_marker_present = true;

        let export = build_export(&request);
        let expected = concat!(
            "DISKPIE_DIAGNOSTIC_EXPORT\n",
            "schema_version=1\n",
            "diagnostics_policy_version=1\n",
            "encoding=utf-8\n",
            "paths_included_by_consent=false\n",
            "\n[system]\n",
            "app_version=0.1.0\n",
            "target=windows-x86_64\n",
            "coarse_os_version=10.0.26100\n",
            "\n[settings]\n",
            "locale=en-US\n",
            "theme=system\n",
            "size_preference=allocated\n",
            "show_both_sizes=true\n",
            "ui_scale_milli=automatic\n",
            "layout_max_sectors=10000\n",
            "layout_max_depth=8\n",
            "layout_minimum_sweep_millionths=2000\n",
            "scan_workers=0\n",
            "diagnostic_level=info\n",
            "explorer_integration=false\n",
            "\n[aggregates]\n",
            "scan_generations=2\n",
            "completed_scans=1\n",
            "failed_scans=1\n",
            "cancelled_scans=0\n",
            "items_observed=42\n",
            "bytes_observed=1024\n",
            "scan_omissions=3\n",
            "total_scan_duration_ms=250\n",
            "\n[statistics]\n",
            "source_files=3\n",
            "unreadable_source_files=1\n",
            "included_source_files=2\n",
            "omitted_source_files=1\n",
            "complete_input_lines=3\n",
            "included_lines=3\n",
            "omitted_lines=1\n",
            "partial_lines_omitted=1\n",
            "capacity_lines_omitted=0\n",
            "truncated_input_lines=0\n",
            "invalid_utf8_lines=0\n",
            "appender_dropped_events=4\n",
            "appender_write_failures=2\n",
            "appender_truncated_lines=1\n",
            "redacted_paths=1\n",
            "redacted_urls=0\n",
            "redacted_query_strings=0\n",
            "redacted_assignments=0\n",
            "redacted_secrets=1\n",
            "redacted_control_runs=0\n",
            "\n[notices]\n",
            "NOTICE omission files=1 lines=1 partial=1 capacity=0\n",
            "NOTICE appender_drop count=4\n",
            "NOTICE appender_write_failure count=2\n",
            "NOTICE line_truncation input=0 appender=1\n",
            "\n[panic]\n",
            "marker_present=true\n",
            "\n[recent_logs]\n",
            "old line\n",
            "open <redacted:path>\n",
            "<redacted:secret>\n",
        );
        assert_eq!(export.artifact().text(), expected);
    }

    #[test]
    fn only_complete_lines_are_exported_and_crlf_is_normalized() {
        let sources = [LogSource::available(b"first\r\nsecond\nsecret partial")];
        let export = build_export(&request(&sources));
        let text = export.artifact().text();

        assert!(text.ends_with("[recent_logs]\nfirst\nsecond\n"));
        assert!(!text.contains("secret partial"));
        assert_eq!(export.stats().complete_input_lines, 2);
        assert_eq!(export.stats().partial_lines_omitted, 1);
        assert_eq!(export.stats().omitted_lines, 1);
        assert_eq!(export.stats().redactions.control_runs, 0);
    }

    #[test]
    fn oversized_utf8_lines_are_cut_only_at_boundaries() {
        let exact = format!("{}🙂\n", "a".repeat(MAX_INPUT_LINE_BYTES - 4));
        let oversized = format!("{}🙂\n", "b".repeat(MAX_INPUT_LINE_BYTES - 2));
        let mut bytes = exact.into_bytes();
        bytes.extend_from_slice(oversized.as_bytes());
        let sources = [LogSource::available(&bytes)];
        let export = build_export(&request(&sources));
        let recent = export.artifact().text().split("[recent_logs]\n").nth(1).expect("section");
        let lines: Vec<_> = recent.lines().collect();

        assert_eq!(lines[0].len(), MAX_INPUT_LINE_BYTES);
        assert!(lines[0].ends_with('🙂'));
        assert!(lines[1].ends_with(LINE_TRUNCATION_MARKER));
        assert!(lines[1].len() <= MAX_INPUT_LINE_BYTES);
        assert!(!lines[1].contains('\u{fffd}'));
        assert_eq!(export.stats().truncated_input_lines, 1);
    }

    #[test]
    fn invalid_utf8_is_made_valid_and_reported() {
        let bytes = [b'o', 0xff, b'k', b'\n'];
        let sources = [LogSource::available(&bytes)];
        let export = build_export(&request(&sources));

        assert!(export.artifact().text().ends_with("[recent_logs]\no�k\n"));
        assert_eq!(export.stats().invalid_utf8_lines, 1);
        assert!(export.artifact().text().contains("NOTICE invalid_utf8 lines=1"));
    }

    #[test]
    fn total_cap_selects_the_newest_chronological_suffix() {
        let payload = "x".repeat(MAX_INPUT_LINE_BYTES - 16);
        let mut bytes = Vec::new();
        for index in 0..540_u16 {
            bytes.extend_from_slice(format!("{index:04}:{payload}\n").as_bytes());
        }
        let sources = [LogSource::available(&bytes)];
        let export = build_export(&request(&sources));
        let text = export.artifact().text();

        assert!(export.artifact().bytes().len() <= MAX_EXPORT_BYTES);
        assert!(text.contains("0539:"));
        assert!(!text.contains("0000:"));
        assert!(export.stats().capacity_lines_omitted > 0);
        assert!(text.contains("NOTICE omission"));
        let recent = text.split("[recent_logs]\n").nth(1).expect("section");
        let mut previous = None;
        for line in recent.lines() {
            assert!(line.len() <= MAX_INPUT_LINE_BYTES);
            let index: u16 = line[..4].parse().expect("numeric prefix");
            if let Some(previous) = previous {
                assert_eq!(index, previous + 1);
            }
            previous = Some(index);
        }
    }

    #[test]
    fn path_consent_never_weakens_mandatory_scrubs() {
        let bytes =
            b"C:\\Users\\Luis\\private.txt\nhttps://private.test/?q=1\ntoken=abc\nHOME=secret\n";
        let sources = [LogSource::available(bytes)];
        let mut request = request(&sources);
        request.include_paths = true;
        let export = build_export(&request);
        let text = export.artifact().text();

        assert!(text.ends_with(concat!(
            "[recent_logs]\n",
            "C:\\Users\\Luis\\private.txt\n",
            "<redacted:url>\n",
            "<redacted:secret>\n",
            "<redacted:assignment>\n",
        )));
        assert!(export.preview().categories().filesystem_paths_by_consent);
        assert_eq!(export.stats().redactions.paths, 0);
        assert_eq!(export.stats().redactions.urls, 1);
        assert_eq!(export.stats().redactions.secrets, 1);
        assert_eq!(export.stats().redactions.assignments, 1);
    }

    #[test]
    fn output_contains_no_control_characters_except_line_feeds() {
        let bytes = b"a\0b\t\x1bc\n";
        let sources = [LogSource::available(bytes)];
        let export = build_export(&request(&sources));

        assert!(
            export
                .artifact()
                .text()
                .chars()
                .all(|character| character == '\n' || !character.is_control())
        );
        assert_eq!(export.stats().redactions.control_runs, 2);
    }
}
