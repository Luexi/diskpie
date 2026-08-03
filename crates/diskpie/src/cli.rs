//! Minimal, native-path-preserving startup command line.

use std::{
    error::Error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use lexopt::{Arg, Parser};

/// Stable command-line synopsis shared by help and sanitized usage errors.
pub const USAGE: &str = "Usage: diskpie [--] [PATH]";

/// Fixed ASCII help protocol for the intentionally small startup grammar.
pub const HELP_TEXT: &str = concat!(
    "DiskPie - visual disk usage scanner\n\n",
    "Usage: diskpie [--] [PATH]\n\n",
    "Arguments:\n",
    "  PATH            Optional folder or drive to scan\n\n",
    "Options:\n",
    "  -h, --help      Show this help\n",
    "  -V, --version   Show the version\n",
    "  --              Treat the following value as PATH\n",
);

/// Fixed version protocol; it never contains filesystem or environment data.
pub const VERSION_TEXT: &str = concat!("DiskPie ", env!("CARGO_PKG_VERSION"));

/// Native startup data passed to the application without display conversion.
#[derive(Clone, Eq, PartialEq)]
pub struct StartupRequest {
    initial_path: Option<PathBuf>,
}

impl StartupRequest {
    /// Returns the optional path exactly as supplied by the operating system.
    pub fn initial_path(&self) -> Option<&Path> {
        self.initial_path.as_deref()
    }

    /// Moves the optional native path into the application runtime.
    pub fn into_initial_path(self) -> Option<PathBuf> {
        self.initial_path
    }
}

impl fmt::Debug for StartupRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartupRequest")
            .field("initial_path_present", &self.initial_path.is_some())
            .finish()
    }
}

/// Mutually exclusive action selected before any UI is constructed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupAction {
    Run(StartupRequest),
    Help,
    Version,
}

/// Stable, path-free category for a command-line usage failure.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UsageErrorCode {
    UnknownOption,
    OptionValueNotAllowed,
    MissingOptionValue,
    TooManyPaths,
    ConflictingAction,
    InvalidArguments,
}

/// Sanitized command-line failure suitable for UI and diagnostics.
///
/// The raw option, path, and parser error are intentionally not retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageError {
    code: UsageErrorCode,
    argument_ordinal: usize,
}

impl UsageError {
    const fn new(code: UsageErrorCode, argument_ordinal: usize) -> Self {
        Self { code, argument_ordinal }
    }

    pub const fn code(self) -> UsageErrorCode {
        self.code
    }

    /// One-based parser item at which the failure was detected.
    pub const fn argument_ordinal(self) -> usize {
        self.argument_ordinal
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.code {
            UsageErrorCode::UnknownOption => "unknown command-line option",
            UsageErrorCode::OptionValueNotAllowed => "this option does not accept a value",
            UsageErrorCode::MissingOptionValue => "an option value is missing",
            UsageErrorCode::TooManyPaths => "only one startup path is allowed",
            UsageErrorCode::ConflictingAction => {
                "help, version, and a startup path cannot be combined"
            }
            UsageErrorCode::InvalidArguments => "the command line is malformed",
        };
        write!(formatter, "{message} (argument {})", self.argument_ordinal)
    }
}

impl Error for UsageError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolAction {
    Help,
    Version,
}

/// Parses the process command line directly from `args_os` through lexopt.
pub fn parse_env() -> Result<StartupAction, UsageError> {
    parse_parser(Parser::from_env())
}

/// Parses an injected complete argv, including its binary name.
pub fn parse_from<I>(arguments: I) -> Result<StartupAction, UsageError>
where
    I: IntoIterator,
    I::Item: Into<OsString>,
{
    parse_parser(Parser::from_iter(arguments))
}

fn parse_parser(mut parser: Parser) -> Result<StartupAction, UsageError> {
    let mut initial_path = None;
    let mut protocol = None;
    let mut ordinal = 0_usize;

    loop {
        let argument =
            parser.next().map_err(|error| map_parser_error(error, ordinal.saturating_add(1)))?;
        let Some(argument) = argument else {
            break;
        };
        ordinal = ordinal.saturating_add(1);

        match argument {
            Arg::Short('h') | Arg::Long("help") => select_protocol(
                &mut protocol,
                ProtocolAction::Help,
                initial_path.is_some(),
                ordinal,
            )?,
            Arg::Short('V') | Arg::Long("version") => select_protocol(
                &mut protocol,
                ProtocolAction::Version,
                initial_path.is_some(),
                ordinal,
            )?,
            Arg::Short(_) | Arg::Long(_) => {
                return Err(UsageError::new(UsageErrorCode::UnknownOption, ordinal));
            }
            Arg::Value(value) => {
                if protocol.is_some() {
                    return Err(UsageError::new(UsageErrorCode::ConflictingAction, ordinal));
                }
                if initial_path.replace(PathBuf::from(value)).is_some() {
                    return Err(UsageError::new(UsageErrorCode::TooManyPaths, ordinal));
                }
            }
        }
    }

    match protocol {
        Some(ProtocolAction::Help) => Ok(StartupAction::Help),
        Some(ProtocolAction::Version) => Ok(StartupAction::Version),
        None => Ok(StartupAction::Run(StartupRequest { initial_path })),
    }
}

fn select_protocol(
    selected: &mut Option<ProtocolAction>,
    action: ProtocolAction,
    has_path: bool,
    ordinal: usize,
) -> Result<(), UsageError> {
    if has_path || selected.replace(action).is_some() {
        return Err(UsageError::new(UsageErrorCode::ConflictingAction, ordinal));
    }
    Ok(())
}

fn map_parser_error(error: lexopt::Error, ordinal: usize) -> UsageError {
    let code = match error {
        lexopt::Error::MissingValue { .. } => UsageErrorCode::MissingOptionValue,
        lexopt::Error::UnexpectedOption(_) => UsageErrorCode::UnknownOption,
        lexopt::Error::UnexpectedArgument(_) => UsageErrorCode::TooManyPaths,
        lexopt::Error::UnexpectedValue { .. } => UsageErrorCode::OptionValueNotAllowed,
        lexopt::Error::ParsingFailed { .. }
        | lexopt::Error::NonUnicodeValue(_)
        | lexopt::Error::Custom(_) => UsageErrorCode::InvalidArguments,
    };
    UsageError::new(code, ordinal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_path(action: StartupAction) -> Option<PathBuf> {
        match action {
            StartupAction::Run(request) => request.into_initial_path(),
            StartupAction::Help | StartupAction::Version => panic!("expected run action"),
        }
    }

    #[test]
    fn accepts_zero_or_one_native_path() {
        assert_eq!(run_path(parse_from(["diskpie"]).expect("zero paths are valid")), None);

        let expected = PathBuf::from(r"C:\Data with spaces\100% & ready");
        let action = parse_from([OsString::from("diskpie"), expected.clone().into_os_string()])
            .expect("one path is valid");
        assert_eq!(run_path(action), Some(expected));
    }

    #[test]
    fn double_dash_preserves_an_option_like_path() {
        let action = parse_from(["diskpie", "--", "--not-an-option"])
            .expect("double dash terminates option parsing");
        assert_eq!(run_path(action), Some(PathBuf::from("--not-an-option")));
    }

    #[test]
    fn rejects_unknown_options_and_excess_paths() {
        let unknown = parse_from(["diskpie", "--scan"]).expect_err("unknown options must fail");
        assert_eq!(unknown.code(), UsageErrorCode::UnknownOption);

        let excess = parse_from(["diskpie", "first", "second-secret"])
            .expect_err("multiple paths must fail");
        assert_eq!(excess.code(), UsageErrorCode::TooManyPaths);
        assert_eq!(excess.argument_ordinal(), 2);
        let rendered = format!("{excess:?} {excess}");
        assert!(!rendered.contains("first"));
        assert!(!rendered.contains("second-secret"));
    }

    #[test]
    fn help_and_version_are_fixed_exclusive_actions() {
        assert_eq!(parse_from(["diskpie", "-h"]), Ok(StartupAction::Help));
        assert_eq!(parse_from(["diskpie", "--version"]), Ok(StartupAction::Version));
        assert_eq!(
            parse_from(["diskpie", "path", "--help"]).expect_err("path plus help conflicts").code(),
            UsageErrorCode::ConflictingAction
        );
        assert_eq!(
            parse_from(["diskpie", "--help=secret"])
                .expect_err("help does not accept values")
                .code(),
            UsageErrorCode::OptionValueNotAllowed
        );
        assert!(!HELP_TEXT.contains(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn request_debug_output_redacts_the_native_path() {
        let action =
            parse_from(["diskpie", r"C:\private\customer-name"]).expect("one path is valid");
        let rendered = format!("{action:?}");
        assert!(rendered.contains("initial_path_present: true"));
        assert!(!rendered.contains("customer-name"));
    }

    #[cfg(windows)]
    #[test]
    fn preserves_non_scalar_utf16_units() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16];
        let native = OsString::from_wide(&units);
        let action = parse_from([OsString::from("diskpie"), native])
            .expect("non-scalar native paths remain positional values");
        let parsed = run_path(action).expect("path remains present");
        assert_eq!(parsed.as_os_str().encode_wide().collect::<Vec<_>>(), units);
    }
}
