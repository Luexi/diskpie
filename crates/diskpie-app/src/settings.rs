//! Versioned, path-free application preferences and a storage-neutral policy.

use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
pub const MAX_SERIALIZED_SETTINGS_BYTES: usize = 64 * 1_024;

const MIN_UI_SCALE: f32 = 0.75;
const MAX_UI_SCALE: f32 = 3.0;
const MIN_LAYOUT_SECTORS: u32 = 256;
const MAX_LAYOUT_SECTORS: u32 = 50_000;
const MIN_LAYOUT_DEPTH: u8 = 1;
const MAX_LAYOUT_DEPTH: u8 = 16;
const MAX_MINIMUM_SWEEP: f64 = 0.1;
const MAX_WORKERS: u16 = 64;
pub const MIN_ITEM_LIST_WIDTH_POINTS: u16 = 260;
pub const MAX_ITEM_LIST_WIDTH_POINTS: u16 = 400;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiLocale {
    #[default]
    EnglishUnitedStates,
    SpanishMexico,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SizePreference {
    Logical,
    #[default]
    Allocated,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

/// Current path-free preference payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Settings {
    pub locale: UiLocale,
    pub theme: UiTheme,
    pub size_preference: SizePreference,
    pub show_both_sizes: bool,
    /// The textual companion is available on demand beside the chart.
    pub show_item_list: bool,
    pub item_list_width_points: u16,
    pub ui_scale: Option<f32>,
    pub layout_max_sectors: u32,
    pub layout_max_depth: u8,
    pub layout_minimum_sweep: f64,
    /// Zero selects automatic bounded worker count.
    pub scan_workers: u16,
    pub diagnostic_level: DiagnosticLevel,
    pub explorer_integration: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            locale: UiLocale::EnglishUnitedStates,
            theme: UiTheme::System,
            size_preference: SizePreference::Allocated,
            show_both_sizes: true,
            show_item_list: false,
            item_list_width_points: 300,
            ui_scale: None,
            layout_max_sectors: 10_000,
            layout_max_depth: 8,
            layout_minimum_sweep: 0.002,
            scan_workers: 0,
            diagnostic_level: DiagnosticLevel::Info,
            explorer_integration: false,
        }
    }
}

impl Settings {
    /// Clamps untrusted persisted values and reports which domains changed.
    #[must_use]
    pub fn validated(mut self) -> (Self, SettingsCorrections) {
        let mut corrections = SettingsCorrections::NONE;

        if self.ui_scale.is_some_and(|scale| !scale.is_finite()) {
            self.ui_scale = None;
            corrections.insert(SettingsCorrections::UI_SCALE);
        } else if let Some(scale) = &mut self.ui_scale {
            let clamped = scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE);
            if *scale != clamped {
                *scale = clamped;
                corrections.insert(SettingsCorrections::UI_SCALE);
            }
        }

        let sectors = self.layout_max_sectors.clamp(MIN_LAYOUT_SECTORS, MAX_LAYOUT_SECTORS);
        if self.layout_max_sectors != sectors {
            self.layout_max_sectors = sectors;
            corrections.insert(SettingsCorrections::LAYOUT);
        }
        let depth = self.layout_max_depth.clamp(MIN_LAYOUT_DEPTH, MAX_LAYOUT_DEPTH);
        if self.layout_max_depth != depth {
            self.layout_max_depth = depth;
            corrections.insert(SettingsCorrections::LAYOUT);
        }
        let minimum_sweep = if self.layout_minimum_sweep.is_finite() {
            self.layout_minimum_sweep.clamp(0.0, MAX_MINIMUM_SWEEP)
        } else {
            Self::default().layout_minimum_sweep
        };
        if self.layout_minimum_sweep != minimum_sweep {
            self.layout_minimum_sweep = minimum_sweep;
            corrections.insert(SettingsCorrections::LAYOUT);
        }

        if self.scan_workers > MAX_WORKERS {
            self.scan_workers = MAX_WORKERS;
            corrections.insert(SettingsCorrections::SCAN_WORKERS);
        }
        let width = self
            .item_list_width_points
            .clamp(MIN_ITEM_LIST_WIDTH_POINTS, MAX_ITEM_LIST_WIDTH_POINTS);
        if self.item_list_width_points != width {
            self.item_list_width_points = width;
            corrections.insert(SettingsCorrections::ITEM_LIST);
        }
        (self, corrections)
    }
}

/// Stable version envelope serialized by the frontend adapter.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SettingsEnvelope {
    pub schema_version: u32,
    pub settings: Settings,
}

impl SettingsEnvelope {
    #[must_use]
    pub const fn current(settings: Settings) -> Self {
        Self { schema_version: SETTINGS_SCHEMA_VERSION, settings }
    }
}

/// Bitset describing validation repairs without another dependency.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct SettingsCorrections(u8);

impl SettingsCorrections {
    pub const NONE: Self = Self(0);
    pub const UI_SCALE: Self = Self(1 << 0);
    pub const LAYOUT: Self = Self(1 << 1);
    pub const SCAN_WORKERS: Self = Self(1 << 2);
    pub const ITEM_LIST: Self = Self(1 << 3);

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, correction: Self) -> bool {
        self.0 & correction.0 == correction.0
    }

    const fn insert(&mut self, correction: Self) {
        self.0 |= correction.0;
    }
}

/// Plain result supplied by a framework-specific storage adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsRead {
    Missing,
    Envelope(SettingsEnvelope),
    UnsupportedVersion { found: u32 },
    Malformed,
    Oversized { bytes: usize },
    Unavailable { code: &'static str },
}

/// Sanitized persistence failure. Paths and serialized values are forbidden.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingsStoreError {
    code: &'static str,
}

impl SettingsStoreError {
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }
}

impl fmt::Display for SettingsStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "settings storage failed ({})", self.code)
    }
}

impl Error for SettingsStoreError {}

/// Consumer-owned port; only the executable adapter knows eframe or RON.
pub trait SettingsStore {
    fn read(&mut self) -> SettingsRead;
    fn write(&mut self, envelope: &SettingsEnvelope) -> Result<(), SettingsStoreError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsWarning {
    UnsupportedVersion { found: u32 },
    Malformed,
    Oversized { bytes: usize },
    Unavailable { code: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsSaveOutcome {
    NotNeeded,
    Saved,
}

/// Session policy that preserves incompatible raw storage until explicit edit.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsSession {
    settings: Settings,
    corrections: SettingsCorrections,
    warning: Option<SettingsWarning>,
    dirty: bool,
    overwrite_allowed: bool,
}

impl SettingsSession {
    #[must_use]
    pub fn load(store: &mut dyn SettingsStore) -> Self {
        match store.read() {
            SettingsRead::Missing => Self::defaults(None, true),
            SettingsRead::Envelope(envelope)
                if envelope.schema_version == SETTINGS_SCHEMA_VERSION =>
            {
                let (settings, corrections) = envelope.settings.validated();
                Self { settings, corrections, warning: None, dirty: false, overwrite_allowed: true }
            }
            SettingsRead::Envelope(envelope) => Self::defaults(
                Some(SettingsWarning::UnsupportedVersion { found: envelope.schema_version }),
                false,
            ),
            SettingsRead::UnsupportedVersion { found } => {
                Self::defaults(Some(SettingsWarning::UnsupportedVersion { found }), false)
            }
            SettingsRead::Malformed => Self::defaults(Some(SettingsWarning::Malformed), false),
            SettingsRead::Oversized { bytes } => {
                Self::defaults(Some(SettingsWarning::Oversized { bytes }), false)
            }
            SettingsRead::Unavailable { code } => {
                Self::defaults(Some(SettingsWarning::Unavailable { code }), false)
            }
        }
    }

    fn defaults(warning: Option<SettingsWarning>, overwrite_allowed: bool) -> Self {
        Self {
            settings: Settings::default(),
            corrections: SettingsCorrections::NONE,
            warning,
            dirty: false,
            overwrite_allowed,
        }
    }

    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    #[must_use]
    pub const fn corrections(&self) -> SettingsCorrections {
        self.corrections
    }

    #[must_use]
    pub const fn warning(&self) -> Option<SettingsWarning> {
        self.warning
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub const fn overwrite_allowed(&self) -> bool {
        self.overwrite_allowed
    }

    /// Applies an explicit user edit, authorizing replacement of protected data.
    pub fn replace(&mut self, settings: Settings) -> SettingsCorrections {
        let (settings, corrections) = settings.validated();
        if self.settings != settings || self.warning.is_some() {
            self.settings = settings;
            self.corrections = corrections;
            self.warning = None;
            self.dirty = true;
            self.overwrite_allowed = true;
        }
        corrections
    }

    /// Explicitly resets every field and authorizes a clean current envelope.
    pub fn reset(&mut self) {
        self.settings = Settings::default();
        self.corrections = SettingsCorrections::NONE;
        self.warning = None;
        self.dirty = true;
        self.overwrite_allowed = true;
    }

    pub fn save(
        &mut self,
        store: &mut dyn SettingsStore,
    ) -> Result<SettingsSaveOutcome, SettingsStoreError> {
        if !self.dirty || !self.overwrite_allowed {
            return Ok(SettingsSaveOutcome::NotNeeded);
        }
        store.write(&SettingsEnvelope::current(self.settings.clone()))?;
        self.dirty = false;
        Ok(SettingsSaveOutcome::Saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeStore {
        read: Option<SettingsRead>,
        writes: Vec<SettingsEnvelope>,
        write_error: Option<SettingsStoreError>,
    }

    impl SettingsStore for FakeStore {
        fn read(&mut self) -> SettingsRead {
            self.read.take().unwrap_or(SettingsRead::Missing)
        }

        fn write(&mut self, envelope: &SettingsEnvelope) -> Result<(), SettingsStoreError> {
            if let Some(error) = self.write_error {
                return Err(error);
            }
            self.writes.push(envelope.clone());
            Ok(())
        }
    }

    #[test]
    fn validates_every_untrusted_numeric_domain() {
        let invalid = Settings {
            ui_scale: Some(f32::NAN),
            layout_max_sectors: u32::MAX,
            layout_max_depth: 0,
            layout_minimum_sweep: f64::INFINITY,
            scan_workers: u16::MAX,
            item_list_width_points: u16::MAX,
            ..Settings::default()
        };
        let (settings, corrections) = invalid.validated();

        assert_eq!(settings.ui_scale, None);
        assert_eq!(settings.layout_max_sectors, MAX_LAYOUT_SECTORS);
        assert_eq!(settings.layout_max_depth, MIN_LAYOUT_DEPTH);
        assert_eq!(settings.layout_minimum_sweep, Settings::default().layout_minimum_sweep);
        assert_eq!(settings.scan_workers, MAX_WORKERS);
        assert_eq!(settings.item_list_width_points, MAX_ITEM_LIST_WIDTH_POINTS);
        assert!(corrections.contains(SettingsCorrections::UI_SCALE));
        assert!(corrections.contains(SettingsCorrections::LAYOUT));
        assert!(corrections.contains(SettingsCorrections::SCAN_WORKERS));
        assert!(corrections.contains(SettingsCorrections::ITEM_LIST));
    }

    #[test]
    fn additive_list_preferences_preserve_old_settings_and_default_to_hidden() {
        let old = serde::de::value::MapDeserializer::<_, serde::de::value::Error>::new(
            [("theme", "dark"), ("size_preference", "logical")].into_iter(),
        );
        let settings = Settings::deserialize(old).unwrap();
        assert_eq!(settings.theme, UiTheme::Dark);
        assert_eq!(settings.size_preference, SizePreference::Logical);
        assert!(!settings.show_item_list);
        assert_eq!(settings.item_list_width_points, 300);
        let defaults = Settings::default();
        assert_eq!(defaults.theme, UiTheme::System);
        assert_eq!(defaults.size_preference, SizePreference::Allocated);
        for (width, expected) in [(0, 260), (260, 260), (300, 300), (400, 400), (u16::MAX, 400)] {
            let (validated, corrections) =
                Settings { item_list_width_points: width, ..defaults.clone() }.validated();
            assert_eq!(validated.item_list_width_points, expected);
            assert_eq!(corrections.contains(SettingsCorrections::ITEM_LIST), width != expected);
        }
    }

    #[test]
    fn item_list_preferences_persist_only_after_an_explicit_edit() {
        let mut store = FakeStore::default();
        let mut session = SettingsSession::load(&mut store);
        let edited = Settings {
            show_item_list: true,
            item_list_width_points: 360,
            ..session.settings().clone()
        };
        session.replace(edited.clone());
        assert_eq!(session.save(&mut store), Ok(SettingsSaveOutcome::Saved));
        assert_eq!(store.writes[0].settings, edited);
        assert_eq!(store.writes[0].schema_version, 1);
    }

    #[test]
    fn current_settings_load_without_implicit_rewrite() {
        let mut store = FakeStore {
            read: Some(SettingsRead::Envelope(SettingsEnvelope::current(Settings {
                layout_max_sectors: 1,
                ..Settings::default()
            }))),
            ..FakeStore::default()
        };
        let mut session = SettingsSession::load(&mut store);

        assert_eq!(session.settings().layout_max_sectors, MIN_LAYOUT_SECTORS);
        assert!(session.corrections().contains(SettingsCorrections::LAYOUT));
        assert_eq!(session.save(&mut store), Ok(SettingsSaveOutcome::NotNeeded));
        assert!(store.writes.is_empty());
    }

    #[test]
    fn newer_malformed_and_oversized_values_are_preserved_until_explicit_edit() {
        for read in [
            SettingsRead::UnsupportedVersion { found: 99 },
            SettingsRead::Malformed,
            SettingsRead::Oversized { bytes: MAX_SERIALIZED_SETTINGS_BYTES + 1 },
        ] {
            let mut store = FakeStore { read: Some(read), ..FakeStore::default() };
            let mut session = SettingsSession::load(&mut store);
            assert!(!session.overwrite_allowed());
            assert_eq!(session.save(&mut store), Ok(SettingsSaveOutcome::NotNeeded));
            assert!(store.writes.is_empty());

            let mut edited = session.settings().clone();
            edited.show_both_sizes = !edited.show_both_sizes;
            session.replace(edited);
            assert!(session.overwrite_allowed());
            assert_eq!(session.save(&mut store), Ok(SettingsSaveOutcome::Saved));
            assert_eq!(store.writes.len(), 1);
            assert_eq!(store.writes[0].schema_version, SETTINGS_SCHEMA_VERSION);
        }
    }

    #[test]
    fn failed_save_remains_dirty_for_a_later_retry() {
        let mut store = FakeStore {
            write_error: Some(SettingsStoreError::new("settings.write.denied")),
            ..FakeStore::default()
        };
        let mut session = SettingsSession::load(&mut store);
        session.reset();

        assert_eq!(session.save(&mut store), Err(SettingsStoreError::new("settings.write.denied")));
        assert!(session.is_dirty());
    }

    #[test]
    fn payload_schema_has_no_path_or_history_fields() {
        let settings = Settings::default();
        let debug = format!("{settings:?}");
        for forbidden in ["path", "folder", "root", "recent", "argument", "secret"] {
            assert!(!debug.to_ascii_lowercase().contains(forbidden));
        }
    }
}
