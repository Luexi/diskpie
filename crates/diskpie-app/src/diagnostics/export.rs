//! Bounded, preview-exact diagnostic export composition.

#![forbid(unsafe_code)]

use super::{
    events::{AppVersion, CoarseOsVersion, DiagnosticLevel, Target},
    redaction::{RedactionCounts, RedactionPolicy, redact},
};
use std::{collections::VecDeque, fmt, fmt::Write as _, sync::Arc};

pub const EXPORT_SCHEMA_VERSION: u32 = 2;
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

/// Result of the most recently settled scan, not an accumulated session count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    Complete,
    Partial,
    Cancelled,
    Failed,
}

impl ScanOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// Path-free metrics for the most recently settled scan. A branch scan's
/// counters describe that scan, not the complete merged snapshot. Unmeasured
/// fields stay unknown, including when no scan has settled yet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AggregateMetrics {
    pub generation: Option<u64>,
    pub outcome: Option<ScanOutcome>,
    pub items_observed: Option<u64>,
    pub bytes_observed: Option<u64>,
    pub scan_omissions: Option<u64>,
    pub scan_duration_millis: Option<u64>,
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
    let PreparedLogs { mut reversed_bytes, mut stats } = prepare_logs(request, MAX_EXPORT_BYTES);
    let mut artifact = render_prefix(request, stats);
    reversed_bytes.reverse();
    artifact.extend_from_slice(&reversed_bytes);
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

struct RetainedLine {
    source_index: usize,
    byte_len: usize,
    redactions: RedactionCounts,
}

struct PreparedLogs {
    reversed_bytes: Vec<u8>,
    stats: ExportStats,
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

fn prepare_logs(request: &ExportRequest<'_>, byte_limit: usize) -> PreparedLogs {
    let prefix_size = PrefixSize::new(request);
    let metadata_limit = prefix_size.maximum_len();
    debug_assert!(metadata_limit < byte_limit);
    let mut preparation = PreparationStats {
        source_files: usize_to_u64(request.log_sources.len()),
        ..PreparationStats::default()
    };
    let policy = RedactionPolicy { include_paths: request.include_paths };
    let mut reversed_bytes = Vec::new();
    let mut oldest_lines = VecDeque::new();
    let mut source_lines = vec![0_u64; request.log_sources.len()];
    let mut included_lines = 0_u64;
    let mut included_sources = 0_u64;
    let mut redactions = RedactionCounts::default();
    let mut retaining = true;

    // Start at the newest complete record. Once the raw suffix fills the byte
    // cap, older lines cannot be selected, but still contribute input statistics.
    // Stored bytes are reversed so prepending an older line is amortized O(line).
    for (source_index, source) in request.log_sources.iter().enumerate().rev() {
        let Some(bytes) = source.bytes else {
            preparation.unreadable_source_files =
                preparation.unreadable_source_files.saturating_add(1);
            continue;
        };
        let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            preparation.partial_lines_omitted =
                preparation.partial_lines_omitted.saturating_add(u64::from(!bytes.is_empty()));
            continue;
        };
        if last_newline + 1 < bytes.len() {
            preparation.partial_lines_omitted = preparation.partial_lines_omitted.saturating_add(1);
        }
        for mut raw_line in bytes[..last_newline].rsplit(|byte| *byte == b'\n') {
            if raw_line.ends_with(b"\r") {
                raw_line = &raw_line[..raw_line.len() - 1];
            }
            let prepared = prepare_line(raw_line, policy);
            preparation.complete_input_lines = preparation.complete_input_lines.saturating_add(1);
            if prepared.truncated {
                preparation.truncated_input_lines =
                    preparation.truncated_input_lines.saturating_add(1);
            }
            if prepared.invalid_utf8 {
                preparation.invalid_utf8_lines = preparation.invalid_utf8_lines.saturating_add(1);
            }
            let byte_len = prepared.text.len() + 1;
            if retaining && reversed_bytes.len() + byte_len <= byte_limit {
                reversed_bytes.push(b'\n');
                reversed_bytes.extend(prepared.text.as_bytes().iter().rev());
                included_lines += 1;
                if source_lines[source_index] == 0 {
                    included_sources += 1;
                }
                source_lines[source_index] += 1;
                redactions.merge(prepared.redactions);
                oldest_lines.push_front(RetainedLine {
                    source_index,
                    byte_len,
                    redactions: prepared.redactions,
                });
                // Only the oldest header-sized margin can need trimming. Each
                // line includes a newline byte, so at most this many records
                // can be removed to make room for the bounded header.
                if oldest_lines.len() > metadata_limit {
                    oldest_lines.pop_back();
                }
            } else {
                retaining = false;
            }
        }
    }
    let mut stats = make_stats(request, preparation, included_lines, included_sources, redactions);
    while prefix_size.len(stats) + reversed_bytes.len() > byte_limit {
        let oldest = oldest_lines.pop_front().expect("header margin retains every removable line");
        reversed_bytes.truncate(reversed_bytes.len() - oldest.byte_len);
        included_lines -= 1;
        source_lines[oldest.source_index] -= 1;
        if source_lines[oldest.source_index] == 0 {
            included_sources -= 1;
        }
        subtract_redactions(&mut redactions, oldest.redactions);
        stats = make_stats(request, preparation, included_lines, included_sources, redactions);
    }
    PreparedLogs { reversed_bytes, stats }
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

fn subtract_redactions(total: &mut RedactionCounts, removed: RedactionCounts) {
    total.paths -= removed.paths;
    total.urls -= removed.urls;
    total.query_strings -= removed.query_strings;
    total.assignments -= removed.assignments;
    total.secrets -= removed.secrets;
    total.control_runs -= removed.control_runs;
}

/// The request portion is immutable. Statistics change only numeric widths
/// and the five explicit optional notice lines; no prefix allocation or
/// formatting is needed while selecting the suffix.
struct PrefixSize {
    fixed_bytes: usize,
}

impl PrefixSize {
    fn new(request: &ExportRequest<'_>) -> Self {
        let empty = ExportStats::default();
        let variable = statistics_width(empty) + notices_len(empty);
        Self { fixed_bytes: render_prefix(request, empty).len() - variable }
    }

    fn len(&self, stats: ExportStats) -> usize {
        self.fixed_bytes + statistics_width(stats) + notices_len(stats)
    }

    fn maximum_len(&self) -> usize {
        let maximum = ExportStats {
            omitted_source_files: u64::MAX,
            omitted_lines: u64::MAX,
            partial_lines_omitted: u64::MAX,
            capacity_lines_omitted: u64::MAX,
            dropped_events: u64::MAX,
            write_failures: u64::MAX,
            truncated_input_lines: u64::MAX,
            sink_truncated_lines: u64::MAX,
            invalid_utf8_lines: u64::MAX,
            ..ExportStats::default()
        };
        self.fixed_bytes + statistic_fields(maximum).len() * 20 + notices_len(maximum)
    }
}

fn decimal_width(value: u64) -> usize {
    if value == 0 { 1 } else { value.ilog10() as usize + 1 }
}

fn statistics_width(stats: ExportStats) -> usize {
    statistic_fields(stats).iter().map(|(_, value)| decimal_width(*value)).sum()
}

fn notices_len(stats: ExportStats) -> usize {
    let mut bytes = 0;
    if stats.omitted_source_files != 0 || stats.omitted_lines != 0 {
        bytes += "NOTICE omission files= lines= partial= capacity=\n".len()
            + decimal_width(stats.omitted_source_files)
            + decimal_width(stats.omitted_lines)
            + decimal_width(stats.partial_lines_omitted)
            + decimal_width(stats.capacity_lines_omitted);
    }
    if stats.dropped_events != 0 {
        bytes += "NOTICE appender_drop count=\n".len() + decimal_width(stats.dropped_events);
    }
    if stats.write_failures != 0 {
        bytes +=
            "NOTICE appender_write_failure count=\n".len() + decimal_width(stats.write_failures);
    }
    if stats.truncated_input_lines != 0 || stats.sink_truncated_lines != 0 {
        bytes += "NOTICE line_truncation input= appender=\n".len()
            + decimal_width(stats.truncated_input_lines)
            + decimal_width(stats.sink_truncated_lines);
    }
    if stats.invalid_utf8_lines != 0 {
        bytes += "NOTICE invalid_utf8 lines=\n".len() + decimal_width(stats.invalid_utf8_lines);
    }
    if bytes == 0 { "notice=none\n".len() } else { bytes }
}

fn statistic_fields(stats: ExportStats) -> [(&'static str, u64); 20] {
    [
        ("source_files", stats.source_files),
        ("unreadable_source_files", stats.unreadable_source_files),
        ("included_source_files", stats.included_source_files),
        ("omitted_source_files", stats.omitted_source_files),
        ("complete_input_lines", stats.complete_input_lines),
        ("included_lines", stats.included_lines),
        ("omitted_lines", stats.omitted_lines),
        ("partial_lines_omitted", stats.partial_lines_omitted),
        ("capacity_lines_omitted", stats.capacity_lines_omitted),
        ("truncated_input_lines", stats.truncated_input_lines),
        ("invalid_utf8_lines", stats.invalid_utf8_lines),
        ("appender_dropped_events", stats.dropped_events),
        ("appender_write_failures", stats.write_failures),
        ("appender_truncated_lines", stats.sink_truncated_lines),
        ("redacted_paths", stats.redactions.paths),
        ("redacted_urls", stats.redactions.urls),
        ("redacted_query_strings", stats.redactions.query_strings),
        ("redacted_assignments", stats.redactions.assignments),
        ("redacted_secrets", stats.redactions.secrets),
        ("redacted_control_runs", stats.redactions.control_runs),
    ]
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
    writeln!(output, "\n[last_scan]").expect("writing to String cannot fail");
    writeln!(output, "outcome={}", aggregates.outcome.map_or("unknown", ScanOutcome::as_str))
        .expect("writing to String cannot fail");
    for (name, value) in [
        ("generation", aggregates.generation),
        ("items_observed", aggregates.items_observed),
        ("bytes_observed", aggregates.bytes_observed),
        ("scan_omissions", aggregates.scan_omissions),
        ("scan_duration_ms", aggregates.scan_duration_millis),
    ] {
        match value {
            Some(value) => writeln!(output, "{name}={value}"),
            None => writeln!(output, "{name}=unknown"),
        }
        .expect("writing to String cannot fail");
    }

    writeln!(output, "\n[statistics]").expect("writing to String cannot fail");
    for (name, value) in statistic_fields(stats) {
        writeln!(output, "{name}={value}").expect("writing to String cannot fail");
    }

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
            show_item_list: false,
            item_list_width_points: 300,
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
    fn unmeasured_last_scan_values_are_unknown_not_zero_or_session_totals() {
        let mut request = request(&[]);
        let absent = build_export(&request);
        assert!(
            absent.artifact().text().contains("[last_scan]\noutcome=unknown\ngeneration=unknown\n")
        );
        assert!(absent.artifact().text().contains("bytes_observed=unknown\n"));
        assert!(absent.artifact().text().contains("scan_duration_ms=unknown\n"));
        assert!(!absent.artifact().text().contains("completed_scans="));
        request.aggregates = AggregateMetrics {
            generation: Some(42),
            outcome: Some(ScanOutcome::Complete),
            items_observed: Some(0),
            bytes_observed: Some(0),
            scan_duration_millis: Some(0),
            scan_omissions: Some(0),
        };
        let measured = build_export(&request);
        assert!(measured.artifact().text().contains("outcome=complete\ngeneration=42\n"));
        assert!(measured.artifact().text().contains("bytes_observed=0\n"));
        assert!(measured.artifact().text().contains("scan_duration_ms=0\n"));
    }

    #[test]
    fn prefix_size_is_exact_across_numeric_boundaries_and_every_notice_combination() {
        let request = request(&[]);
        let size = PrefixSize::new(&request);
        for value in [0, 1, 9, 10, 99, 100, u64::MAX] {
            for notices in 0..32 {
                let stats = ExportStats {
                    source_files: value,
                    unreadable_source_files: value,
                    included_source_files: value,
                    omitted_source_files: if notices & 1 != 0 { value } else { 0 },
                    complete_input_lines: value,
                    included_lines: value,
                    omitted_lines: if notices & 1 != 0 { value } else { 0 },
                    partial_lines_omitted: value,
                    capacity_lines_omitted: value,
                    dropped_events: if notices & 2 != 0 { value } else { 0 },
                    write_failures: if notices & 4 != 0 { value } else { 0 },
                    truncated_input_lines: if notices & 8 != 0 { value } else { 0 },
                    sink_truncated_lines: if notices & 8 != 0 { value } else { 0 },
                    invalid_utf8_lines: if notices & 16 != 0 { value } else { 0 },
                    redactions: RedactionCounts {
                        paths: value,
                        urls: value,
                        query_strings: value,
                        assignments: value,
                        secrets: value,
                        control_runs: value,
                    },
                    ..ExportStats::default()
                };
                let actual = render_prefix(&request, stats).len();
                assert_eq!(size.len(stats), actual, "value={value} notices={notices}");
                assert!(size.maximum_len() >= actual);
            }
        }
    }

    // Deliberately simple forward/all-lines oracle for the suffix contract.
    // This is the previous selection strategy, restricted to small test inputs.
    fn reference_suffix(request: &ExportRequest<'_>, cap: usize) -> (Vec<u8>, ExportStats) {
        let mut lines = Vec::new();
        let mut preparation = PreparationStats {
            source_files: usize_to_u64(request.log_sources.len()),
            ..Default::default()
        };
        for (source_index, source) in request.log_sources.iter().enumerate() {
            let Some(bytes) = source.bytes else {
                preparation.unreadable_source_files += 1;
                continue;
            };
            for record in bytes.split_inclusive(|byte| *byte == b'\n') {
                if !record.ends_with(b"\n") {
                    preparation.partial_lines_omitted += 1;
                    continue;
                }
                let mut raw = &record[..record.len() - 1];
                if raw.ends_with(b"\r") {
                    raw = &raw[..raw.len() - 1];
                }
                let prepared =
                    prepare_line(raw, RedactionPolicy { include_paths: request.include_paths });
                preparation.complete_input_lines += 1;
                preparation.truncated_input_lines += u64::from(prepared.truncated);
                preparation.invalid_utf8_lines += u64::from(prepared.invalid_utf8);
                lines.push((source_index, prepared));
            }
        }
        let mut source_included = vec![false; request.log_sources.len()];
        let mut source_count = 0;
        let mut bytes = 0;
        let mut redactions = RedactionCounts::default();
        let mut selected = (lines.len(), make_stats(request, preparation, 0, 0, redactions));
        for start in (0..lines.len()).rev() {
            let (source, line) = &lines[start];
            bytes += line.text.len() + 1;
            redactions.merge(line.redactions);
            if !source_included[*source] {
                source_included[*source] = true;
                source_count += 1;
            }
            let stats = make_stats(
                request,
                preparation,
                usize_to_u64(lines.len() - start),
                source_count,
                redactions,
            );
            if render_prefix(request, stats).len() + bytes <= cap {
                selected = (start, stats);
            }
        }
        let mut artifact = render_prefix(request, selected.1);
        for (_, line) in &lines[selected.0..] {
            artifact.extend_from_slice(line.text.as_bytes());
            artifact.push(b'\n');
        }
        (artifact, selected.1)
    }

    #[test]
    fn bounded_suffix_matches_reference_for_short_lines_sources_redaction_and_truncation() {
        let old = b"old\n\n\r\npartial".repeat(30);
        let short = b"x\n\n".repeat(5_000);
        let mixed = b"token=abc\nC:\\private\\path\r\nbad\xff\n".repeat(40);
        let long = format!("{}\n", "z".repeat(MAX_INPUT_LINE_BYTES + 10));
        let sources = [
            LogSource::available(&old),
            LogSource::unavailable(),
            LogSource::available(long.as_bytes()),
            LogSource::available(&short),
            LogSource::available(&mixed),
            LogSource::available(b"last\nunfinished"),
        ];
        for include_paths in [false, true] {
            let mut request = request(&sources);
            request.include_paths = include_paths;
            for cap in [8_192, 16_384, 32_768] {
                let PreparedLogs { mut reversed_bytes, stats } = prepare_logs(&request, cap);
                let mut actual = render_prefix(&request, stats);
                reversed_bytes.reverse();
                actual.extend(reversed_bytes);
                let expected = reference_suffix(&request, cap);
                assert_eq!((actual, stats), expected, "cap={cap} paths={include_paths}");
            }
        }
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
            generation: Some(2),
            outcome: Some(ScanOutcome::Failed),
            items_observed: Some(42),
            bytes_observed: Some(1_024),
            scan_omissions: Some(3),
            scan_duration_millis: Some(250),
        };
        request.counters =
            DiagnosticCounters { dropped_events: 4, write_failures: 2, sink_truncated_lines: 1 };
        request.metadata.panic_marker_present = true;

        let export = build_export(&request);
        let expected = concat!(
            "DISKPIE_DIAGNOSTIC_EXPORT\n",
            "schema_version=2\n",
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
            "\n[last_scan]\n",
            "outcome=failed\n",
            "generation=2\n",
            "items_observed=42\n",
            "bytes_observed=1024\n",
            "scan_omissions=3\n",
            "scan_duration_ms=250\n",
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
