//! Typed localization backed by embedded Fluent resources.

use std::borrow::Cow;

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use thiserror::Error;
use unic_langid::LanguageIdentifier;

const EN_US_SOURCE: &str = include_str!("../i18n/en-US.ftl");
const ES_MX_SOURCE: &str = include_str!("../i18n/es-MX.ftl");

/// A locale shipped as part of the portable executable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Locale {
    #[default]
    EnglishUnitedStates,
    SpanishMexico,
}

impl Locale {
    pub const ALL: [Self; 2] = [Self::EnglishUnitedStates, Self::SpanishMexico];

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::EnglishUnitedStates => "en-US",
            Self::SpanishMexico => "es-MX",
        }
    }

    #[must_use]
    pub const fn native_name(self) -> &'static str {
        match self {
            Self::EnglishUnitedStates => "English (United States)",
            Self::SpanishMexico => "Español (México)",
        }
    }

    const fn source(self) -> &'static str {
        match self {
            Self::EnglishUnitedStates => EN_US_SOURCE,
            Self::SpanishMexico => ES_MX_SOURCE,
        }
    }
}

/// Every user-visible phrase that application logic may request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum MessageId {
    AppName,
    Tagline,
    Back,
    Parent,
    NoBackHistory,
    NoParent,
    ChooseFolder,
    ChooseDrive,
    NoLocation,
    Theme,
    ThemeSystem,
    ThemeDark,
    ThemeLight,
    Language,
    Ready,
    Scanning,
    Cancelling,
    Cancel,
    Rescan,
    RescanBranch,
    Selected,
    Files,
    Folders,
    LogicalSize,
    AllocatedData,
    ShowBothSizes,
    LargestItems,
    EmptyItemsHint,
    CurrentPath,
    NoSelection,
    FileCount,
    FolderCount,
    ScanState,
    NotStarted,
    Safety,
    ActionsUnavailable,
    Open,
    Reveal,
    Recycle,
    DeletePermanently,
    EmptyRecycleBin,
    InstalledApps,
    HideBranch,
    RestoreBranch,
    ShowHidden,
    ChooseFolderCenter,
    OrDrive,
    ChartEmptyTooltip,
    StandardUser,
    NoElevation,
    StatusReady,
    PartialResults,
    Complete,
    Cancelled,
    OmittedItems,
    UnknownSize,
    OtherGroup,
    HiddenGroup,
    ConfirmRecycleTitle,
    ConfirmRecycleBody,
    ConfirmDeleteTitle,
    ConfirmDeleteBody,
    ConfirmDeletePhrase,
    ConfirmEmptyRecycleBinTitle,
    PermissionDenied,
    PathUnavailable,
    DiagnosticsExported,
    ItemCount,
}

impl MessageId {
    pub const ALL: [Self; 68] = [
        Self::AppName,
        Self::Tagline,
        Self::Back,
        Self::Parent,
        Self::NoBackHistory,
        Self::NoParent,
        Self::ChooseFolder,
        Self::ChooseDrive,
        Self::NoLocation,
        Self::Theme,
        Self::ThemeSystem,
        Self::ThemeDark,
        Self::ThemeLight,
        Self::Language,
        Self::Ready,
        Self::Scanning,
        Self::Cancelling,
        Self::Cancel,
        Self::Rescan,
        Self::RescanBranch,
        Self::Selected,
        Self::Files,
        Self::Folders,
        Self::LogicalSize,
        Self::AllocatedData,
        Self::ShowBothSizes,
        Self::LargestItems,
        Self::EmptyItemsHint,
        Self::CurrentPath,
        Self::NoSelection,
        Self::FileCount,
        Self::FolderCount,
        Self::ScanState,
        Self::NotStarted,
        Self::Safety,
        Self::ActionsUnavailable,
        Self::Open,
        Self::Reveal,
        Self::Recycle,
        Self::DeletePermanently,
        Self::EmptyRecycleBin,
        Self::InstalledApps,
        Self::HideBranch,
        Self::RestoreBranch,
        Self::ShowHidden,
        Self::ChooseFolderCenter,
        Self::OrDrive,
        Self::ChartEmptyTooltip,
        Self::StandardUser,
        Self::NoElevation,
        Self::StatusReady,
        Self::PartialResults,
        Self::Complete,
        Self::Cancelled,
        Self::OmittedItems,
        Self::UnknownSize,
        Self::OtherGroup,
        Self::HiddenGroup,
        Self::ConfirmRecycleTitle,
        Self::ConfirmRecycleBody,
        Self::ConfirmDeleteTitle,
        Self::ConfirmDeleteBody,
        Self::ConfirmDeletePhrase,
        Self::ConfirmEmptyRecycleBinTitle,
        Self::PermissionDenied,
        Self::PathUnavailable,
        Self::DiagnosticsExported,
        Self::ItemCount,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::AppName => "app-name",
            Self::Tagline => "tagline",
            Self::Back => "back",
            Self::Parent => "parent",
            Self::NoBackHistory => "no-back-history",
            Self::NoParent => "no-parent",
            Self::ChooseFolder => "choose-folder",
            Self::ChooseDrive => "choose-drive",
            Self::NoLocation => "no-location",
            Self::Theme => "theme",
            Self::ThemeSystem => "theme-system",
            Self::ThemeDark => "theme-dark",
            Self::ThemeLight => "theme-light",
            Self::Language => "language",
            Self::Ready => "ready",
            Self::Scanning => "scanning",
            Self::Cancelling => "cancelling",
            Self::Cancel => "cancel",
            Self::Rescan => "rescan",
            Self::RescanBranch => "rescan-branch",
            Self::Selected => "selected",
            Self::Files => "files",
            Self::Folders => "folders",
            Self::LogicalSize => "logical-size",
            Self::AllocatedData => "allocated-data",
            Self::ShowBothSizes => "show-both-sizes",
            Self::LargestItems => "largest-items",
            Self::EmptyItemsHint => "empty-items-hint",
            Self::CurrentPath => "current-path",
            Self::NoSelection => "no-selection",
            Self::FileCount => "file-count",
            Self::FolderCount => "folder-count",
            Self::ScanState => "scan-state",
            Self::NotStarted => "not-started",
            Self::Safety => "safety",
            Self::ActionsUnavailable => "actions-unavailable",
            Self::Open => "open",
            Self::Reveal => "reveal",
            Self::Recycle => "recycle",
            Self::DeletePermanently => "delete-permanently",
            Self::EmptyRecycleBin => "empty-recycle-bin",
            Self::InstalledApps => "installed-apps",
            Self::HideBranch => "hide-branch",
            Self::RestoreBranch => "restore-branch",
            Self::ShowHidden => "show-hidden",
            Self::ChooseFolderCenter => "choose-folder-center",
            Self::OrDrive => "or-drive",
            Self::ChartEmptyTooltip => "chart-empty-tooltip",
            Self::StandardUser => "standard-user",
            Self::NoElevation => "no-elevation",
            Self::StatusReady => "status-ready",
            Self::PartialResults => "partial-results",
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
            Self::OmittedItems => "omitted-items",
            Self::UnknownSize => "unknown-size",
            Self::OtherGroup => "other-group",
            Self::HiddenGroup => "hidden-group",
            Self::ConfirmRecycleTitle => "confirm-recycle-title",
            Self::ConfirmRecycleBody => "confirm-recycle-body",
            Self::ConfirmDeleteTitle => "confirm-delete-title",
            Self::ConfirmDeleteBody => "confirm-delete-body",
            Self::ConfirmDeletePhrase => "confirm-delete-phrase",
            Self::ConfirmEmptyRecycleBinTitle => "confirm-empty-recycle-bin-title",
            Self::PermissionDenied => "permission-denied",
            Self::PathUnavailable => "path-unavailable",
            Self::DiagnosticsExported => "diagnostics-exported",
            Self::ItemCount => "item-count",
        }
    }
}

/// Errors in embedded language resources or formatting calls.
#[derive(Debug, Error)]
pub enum I18nError {
    #[error("invalid locale identifier {code}: {source}")]
    InvalidLocale { code: &'static str, source: unic_langid::LanguageIdentifierError },
    #[error("invalid {locale} Fluent resource: {details}")]
    InvalidResource { locale: &'static str, details: String },
    #[error("message {key} is missing from {locale}")]
    MissingMessage { locale: &'static str, key: &'static str },
    #[error("message {key} in {locale} has no value")]
    MissingValue { locale: &'static str, key: &'static str },
    #[error("message {key} failed to format in {locale}: {details}")]
    Format { locale: &'static str, key: &'static str, details: String },
}

/// A validated bundle for one selected language.
pub struct I18n {
    locale: Locale,
    bundle: FluentBundle<FluentResource>,
}

impl I18n {
    /// Parse and validate all embedded messages for a locale.
    pub fn new(locale: Locale) -> Result<Self, I18nError> {
        let language: LanguageIdentifier = locale
            .code()
            .parse()
            .map_err(|source| I18nError::InvalidLocale { code: locale.code(), source })?;
        let resource = FluentResource::try_new(locale.source().to_owned()).map_err(
            |(_resource, errors)| I18nError::InvalidResource {
                locale: locale.code(),
                details: format_errors(&errors),
            },
        )?;
        let mut bundle = FluentBundle::new(vec![language]);
        bundle.add_resource(resource).map_err(|errors| I18nError::InvalidResource {
            locale: locale.code(),
            details: format_errors(&errors),
        })?;

        let i18n = Self { locale, bundle };
        let mut validation_arguments = FluentArgs::new();
        validation_arguments.set("count", 2_u64);
        validation_arguments.set("path", r"C:\DiskPie validation\item.bin");
        for id in MessageId::ALL {
            i18n.resolve(id, Some(&validation_arguments))?;
        }
        Ok(i18n)
    }

    #[must_use]
    pub const fn locale(&self) -> Locale {
        self.locale
    }

    /// Resolve a message without interpolation variables.
    pub fn text(&self, id: MessageId) -> Result<Cow<'_, str>, I18nError> {
        self.resolve(id, None)
    }

    /// Resolve a message with Fluent interpolation and plural arguments.
    pub fn format<'a>(
        &'a self,
        id: MessageId,
        arguments: &'a FluentArgs<'a>,
    ) -> Result<Cow<'a, str>, I18nError> {
        self.resolve(id, Some(arguments))
    }

    /// Format the localized item-count plural without exposing Fluent to callers.
    pub fn item_count(&self, count: u64) -> Result<String, I18nError> {
        let mut arguments = FluentArgs::new();
        arguments.set("count", count);
        self.resolve(MessageId::ItemCount, Some(&arguments)).map(Cow::into_owned)
    }

    fn resolve<'a>(
        &'a self,
        id: MessageId,
        arguments: Option<&'a FluentArgs<'a>>,
    ) -> Result<Cow<'a, str>, I18nError> {
        let key = id.key();
        let message = self
            .bundle
            .get_message(key)
            .ok_or(I18nError::MissingMessage { locale: self.locale.code(), key })?;
        let pattern =
            message.value().ok_or(I18nError::MissingValue { locale: self.locale.code(), key })?;
        let mut errors = Vec::new();
        let value = self.bundle.format_pattern(pattern, arguments, &mut errors);
        if errors.is_empty() {
            Ok(value)
        } else {
            Err(I18nError::Format {
                locale: self.locale.code(),
                key,
                details: format_errors(&errors),
            })
        }
    }
}

fn format_errors(errors: &[impl std::fmt::Debug]) -> String {
    errors.iter().map(|error| format!("{error:?}")).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_locale_contains_every_typed_message() {
        for locale in Locale::ALL {
            let bundle = I18n::new(locale).unwrap_or_else(|error| panic!("{error}"));
            let mut arguments = FluentArgs::new();
            arguments.set("count", 2_u64);
            arguments.set("path", r"C:\Prueba\archivo.bin");
            for id in MessageId::ALL {
                let text = bundle
                    .format(id, &arguments)
                    .unwrap_or_else(|error| panic!("{}: {error}", id.key()));
                assert!(!text.trim().is_empty(), "{} is empty in {}", id.key(), locale.code());
            }
        }
    }

    #[test]
    fn message_table_order_matches_discriminants() {
        for (index, id) in MessageId::ALL.into_iter().enumerate() {
            assert_eq!(index, id.index(), "{} has a stale table index", id.key());
        }
    }

    #[test]
    fn plural_item_count_formats_in_both_languages() {
        for locale in Locale::ALL {
            let bundle = I18n::new(locale).unwrap_or_else(|error| panic!("{error}"));
            let mut arguments = FluentArgs::new();
            arguments.set("count", 2_u64);
            let text = bundle
                .format(MessageId::ItemCount, &arguments)
                .unwrap_or_else(|error| panic!("{error}"));
            assert!(text.contains('2'));
        }
    }

    #[test]
    fn spanish_resources_are_independently_authored() {
        let english =
            I18n::new(Locale::EnglishUnitedStates).unwrap_or_else(|error| panic!("{error}"));
        let spanish = I18n::new(Locale::SpanishMexico).unwrap_or_else(|error| panic!("{error}"));
        let arguments = FluentArgs::new();
        for id in [MessageId::ChooseFolder, MessageId::Safety, MessageId::PermissionDenied] {
            assert_ne!(
                english.format(id, &arguments).unwrap_or_default(),
                spanish.format(id, &arguments).unwrap_or_default()
            );
        }
    }
}
