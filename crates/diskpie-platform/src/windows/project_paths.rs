//! Windows project-path resolution through Known Folder APIs.
//!
//! Resolution is read-only and has no fallback to environment variables, the
//! current directory, the executable directory, a drive root, or temporary
//! storage. Directory creation is available only through the separate,
//! explicit [`ProjectPaths::create_directory`] operation.

use std::{
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    marker::PhantomData,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{
            E_ACCESSDENIED, E_UNEXPECTED, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND,
            ERROR_INVALID_DATA, ERROR_INVALID_NAME, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
            ERROR_PATH_NOT_FOUND, HANDLE, RPC_E_CHANGED_MODE,
        },
        System::Com::{
            COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
        UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, KNOWN_FOLDER_FLAG,
        },
    },
    core::{GUID, HRESULT, PWSTR},
};

/// Persistent on-disk namespace selected by ADR 0018.
pub const APPLICATION_IDENTIFIER: &str = "io.github.luexi.diskpie";

const SETTINGS_DIRECTORY_COMPONENT: &str = "data";
const SETTINGS_FILE_COMPONENT: &str = "app.ron";
const LOGS_DIRECTORY_COMPONENT: &str = "logs";
const CRASHES_DIRECTORY_COMPONENT: &str = "crashes";

/// Optional application facility backed by a project path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProjectPathFacility {
    Settings,
    Logs,
    Crashes,
}

/// Operation that produced a sanitized project-path error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProjectPathOperation {
    InitializeCom,
    ResolveKnownFolder,
    CreateDirectory,
}

/// Stable error category that does not expose an absolute path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProjectPathErrorKind {
    AccessDenied,
    NotFound,
    InvalidData,
    Unsupported,
    Unavailable,
    Other,
}

/// Plain, sanitized failure information for an optional project-path facility.
///
/// Exact native codes are retained without a localized system message or the
/// absolute path. Known Folder failures use `hresult`; filesystem creation
/// failures use `os_code` when Windows supplied one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectPathError {
    pub facility: ProjectPathFacility,
    pub operation: ProjectPathOperation,
    pub kind: ProjectPathErrorKind,
    pub hresult: Option<i32>,
    pub os_code: Option<i32>,
}

impl ProjectPathError {
    const fn from_com_hresult(facility: ProjectPathFacility, hresult: i32) -> Self {
        Self {
            facility,
            operation: ProjectPathOperation::InitializeCom,
            kind: error_kind_from_hresult(hresult),
            hresult: Some(hresult),
            os_code: None,
        }
    }

    const fn from_hresult(facility: ProjectPathFacility, hresult: i32) -> Self {
        Self {
            facility,
            operation: ProjectPathOperation::ResolveKnownFolder,
            kind: error_kind_from_hresult(hresult),
            hresult: Some(hresult),
            os_code: None,
        }
    }

    fn from_io(facility: ProjectPathFacility, error: &io::Error) -> Self {
        Self {
            facility,
            operation: ProjectPathOperation::CreateDirectory,
            kind: error_kind_from_io(error),
            hresult: None,
            os_code: error.raw_os_error(),
        }
    }

    const fn unavailable(facility: ProjectPathFacility) -> Self {
        Self {
            facility,
            operation: ProjectPathOperation::CreateDirectory,
            kind: ProjectPathErrorKind::Unavailable,
            hresult: None,
            os_code: None,
        }
    }

    const fn with_facility(mut self, facility: ProjectPathFacility) -> Self {
        self.facility = facility;
        self
    }
}

impl fmt::Display for ProjectPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "project path {:?} failed during {:?}: {:?}",
            self.facility, self.operation, self.kind
        )?;
        if let Some(hresult) = self.hresult {
            write!(formatter, " (HRESULT 0x{:08X})", hresult as u32)?;
        }
        if let Some(os_code) = self.os_code {
            write!(formatter, " (OS code {os_code})")?;
        }
        Ok(())
    }
}

impl Error for ProjectPathError {}

/// Independently available project locations for preferences and diagnostics.
///
/// A failure resolving Roaming AppData leaves local diagnostics available, and
/// a Local AppData failure leaves roaming settings available. `issues` contains
/// at most one settings issue and one issue for each local facility.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct ProjectPaths {
    pub settings_file: Option<PathBuf>,
    pub logs_directory: Option<PathBuf>,
    pub crashes_directory: Option<PathBuf>,
    pub issues: Vec<ProjectPathError>,
}

impl fmt::Debug for ProjectPaths {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectPaths")
            .field("settings_available", &self.settings_file.is_some())
            .field("logs_available", &self.logs_directory.is_some())
            .field("crashes_available", &self.crashes_directory.is_some())
            .field("issues", &self.issues)
            .finish()
    }
}

impl ProjectPaths {
    /// Resolves both Known Folders without creating directories or files.
    pub fn resolve() -> Self {
        let com = match ComScope::enter(ProjectPathFacility::Settings) {
            Ok(com) => com,
            Err(error) => {
                return Self::from_roots(
                    Err(error),
                    Err(error.with_facility(ProjectPathFacility::Logs)),
                );
            }
        };
        let roaming =
            resolve_known_folder(&com, ProjectPathFacility::Settings, &FOLDERID_RoamingAppData);
        let local = resolve_known_folder(&com, ProjectPathFacility::Logs, &FOLDERID_LocalAppData);
        Self::from_roots(roaming, local)
    }

    fn from_roots(
        roaming: Result<PathBuf, ProjectPathError>,
        local: Result<PathBuf, ProjectPathError>,
    ) -> Self {
        let mut paths = Self::default();

        match roaming {
            Ok(root) => paths.settings_file = Some(settings_file_from_root(&root)),
            Err(error) => paths.issues.push(error),
        }

        match local {
            Ok(root) => {
                let application_root = application_root(&root);
                paths.logs_directory = Some(application_root.join(LOGS_DIRECTORY_COMPONENT));
                paths.crashes_directory = Some(application_root.join(CRASHES_DIRECTORY_COMPONENT));
            }
            Err(error) => {
                paths.issues.push(error);
                paths.issues.push(error.with_facility(ProjectPathFacility::Crashes));
            }
        }

        paths
    }

    /// Returns whether every optional facility has a resolved native path.
    pub fn is_complete(&self) -> bool {
        self.settings_file.is_some()
            && self.logs_directory.is_some()
            && self.crashes_directory.is_some()
    }

    /// Returns the directory used by a facility, without touching the filesystem.
    ///
    /// For settings this is the `data` parent of `app.ron`.
    pub fn directory(&self, facility: ProjectPathFacility) -> Option<&Path> {
        match facility {
            ProjectPathFacility::Settings => self.settings_file.as_deref()?.parent(),
            ProjectPathFacility::Logs => self.logs_directory.as_deref(),
            ProjectPathFacility::Crashes => self.crashes_directory.as_deref(),
        }
    }

    /// Explicitly creates the one requested facility directory and its missing parents.
    ///
    /// Calling [`Self::resolve`] never invokes this method implicitly. A caller
    /// should request only the facility it is actually enabling.
    pub fn create_directory(&self, facility: ProjectPathFacility) -> Result<(), ProjectPathError> {
        self.create_directory_with(facility, |path| fs::create_dir_all(path))
    }

    fn create_directory_with<F>(
        &self,
        facility: ProjectPathFacility,
        create: F,
    ) -> Result<(), ProjectPathError>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        let Some(directory) = self.directory(facility) else {
            return Err(self
                .issues
                .iter()
                .copied()
                .find(|error| error.facility == facility)
                .unwrap_or_else(|| ProjectPathError::unavailable(facility)));
        };

        create(directory).map_err(|error| ProjectPathError::from_io(facility, &error))
    }
}

/// Resolves project paths without creating any filesystem object.
pub fn resolve_project_paths() -> ProjectPaths {
    ProjectPaths::resolve()
}

fn application_root(known_folder: &Path) -> PathBuf {
    known_folder.join(APPLICATION_IDENTIFIER)
}

fn settings_file_from_root(roaming: &Path) -> PathBuf {
    application_root(roaming).join(SETTINGS_DIRECTORY_COMPONENT).join(SETTINGS_FILE_COMPONENT)
}

const fn error_kind_from_hresult(hresult: i32) -> ProjectPathErrorKind {
    if hresult == E_ACCESSDENIED.0 || hresult == HRESULT::from_win32(ERROR_ACCESS_DENIED.0).0 {
        ProjectPathErrorKind::AccessDenied
    } else if hresult == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0).0
        || hresult == HRESULT::from_win32(ERROR_PATH_NOT_FOUND.0).0
    {
        ProjectPathErrorKind::NotFound
    } else if hresult == HRESULT::from_win32(ERROR_INVALID_DATA.0).0
        || hresult == HRESULT::from_win32(ERROR_INVALID_NAME.0).0
        || hresult == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0).0
        || hresult == E_UNEXPECTED.0
    {
        ProjectPathErrorKind::InvalidData
    } else if hresult == HRESULT::from_win32(ERROR_NOT_SUPPORTED.0).0 {
        ProjectPathErrorKind::Unsupported
    } else {
        ProjectPathErrorKind::Other
    }
}

fn error_kind_from_io(error: &io::Error) -> ProjectPathErrorKind {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ProjectPathErrorKind::AccessDenied,
        io::ErrorKind::NotFound => ProjectPathErrorKind::NotFound,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::AlreadyExists => {
            ProjectPathErrorKind::InvalidData
        }
        io::ErrorKind::Unsupported => ProjectPathErrorKind::Unsupported,
        _ => ProjectPathErrorKind::Other,
    }
}

struct ComScope {
    must_uninitialize: bool,
    thread_affine: PhantomData<*mut ()>,
}

impl ComScope {
    fn enter(facility: ProjectPathFacility) -> Result<Self, ProjectPathError> {
        // SAFETY: the reserved pointer is null. Known Folder resolution does
        // not retain COM objects, so a temporary MTA is sufficient. A
        // successful call is balanced by this scope on the same thread.
        let hresult =
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE) };
        match classify_com_initialization(hresult.0) {
            Ok(must_uninitialize) => Ok(Self { must_uninitialize, thread_affine: PhantomData }),
            Err(hresult) => Err(ProjectPathError::from_com_hresult(facility, hresult)),
        }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.must_uninitialize {
            // SAFETY: this balances this scope's successful `CoInitializeEx`
            // call on the identical thread, including an S_FALSE result.
            unsafe { CoUninitialize() };
        }
    }
}

fn classify_com_initialization(hresult: i32) -> Result<bool, i32> {
    if HRESULT(hresult).is_ok() {
        Ok(true)
    } else if hresult == RPC_E_CHANGED_MODE.0 {
        // The caller already owns an incompatible apartment. COM is therefore
        // initialized, but this failed call must not be balanced.
        Ok(false)
    } else {
        Err(hresult)
    }
}

fn resolve_known_folder(
    com: &ComScope,
    facility: ProjectPathFacility,
    folder_id: &GUID,
) -> Result<PathBuf, ProjectPathError> {
    resolve_known_folder_with(
        facility,
        |output| raw_sh_get_known_folder_path(com, folder_id, KF_FLAG_DEFAULT, output),
        |allocation| {
            // SAFETY: the non-null pointer was returned through
            // `SHGetKnownFolderPath`'s CoTaskMem output slot and is released
            // exactly once by `WideOutput`.
            unsafe { CoTaskMemFree(Some(allocation.as_ptr().cast())) };
        },
    )
}

fn raw_sh_get_known_folder_path(
    _com: &ComScope,
    folder_id: &GUID,
    flags: KNOWN_FOLDER_FLAG,
    output: *mut PWSTR,
) -> i32 {
    windows::core::link!(
        "shell32.dll" "system" fn SHGetKnownFolderPath(
            rfid: *const GUID,
            dwflags: u32,
            htoken: HANDLE,
            ppszpath: *mut PWSTR,
        ) -> HRESULT
    );

    // SAFETY: `folder_id` is a live GUID, `output` points to the guarded PWSTR
    // slot for the duration of the call, the flags are a projected Known Folder
    // value, and a null token requests the current user.
    unsafe { SHGetKnownFolderPath(folder_id, flags.0 as u32, HANDLE::default(), output).0 }
}

struct WideOutput<R>
where
    R: FnMut(PWSTR),
{
    value: PWSTR,
    release: R,
}

impl<R> WideOutput<R>
where
    R: FnMut(PWSTR),
{
    fn new(release: R) -> Self {
        Self { value: PWSTR::null(), release }
    }

    fn output_slot(&mut self) -> *mut PWSTR {
        &mut self.value
    }

    fn copy_path(&self, facility: ProjectPathFacility) -> Result<PathBuf, ProjectPathError> {
        if self.value.is_null() {
            return Err(ProjectPathError::from_hresult(facility, E_UNEXPECTED.0));
        }

        // SAFETY: on a successful Known Folder HRESULT the API contract makes
        // this a live, NUL-terminated UTF-16 allocation until the guard drops.
        let units = unsafe { self.value.as_wide() };
        let path = PathBuf::from(os_string_from_wide(units));
        if !path.is_absolute() {
            return Err(ProjectPathError::from_hresult(facility, E_UNEXPECTED.0));
        }
        Ok(path)
    }
}

impl<R> Drop for WideOutput<R>
where
    R: FnMut(PWSTR),
{
    fn drop(&mut self) {
        if !self.value.is_null() {
            (self.release)(self.value);
            self.value = PWSTR::null();
        }
    }
}

fn resolve_known_folder_with<C, R>(
    facility: ProjectPathFacility,
    call: C,
    release: R,
) -> Result<PathBuf, ProjectPathError>
where
    C: FnOnce(*mut PWSTR) -> i32,
    R: FnMut(PWSTR),
{
    // The guard exists before the ABI call. A non-null failure output is thus
    // released before this function returns, just like a success output.
    let mut output = WideOutput::new(release);
    let hresult = call(output.output_slot());
    if HRESULT(hresult).is_err() {
        return Err(ProjectPathError::from_hresult(facility, hresult));
    }
    output.copy_path(facility)
}

fn os_string_from_wide(units: &[u16]) -> OsString {
    OsString::from_wide(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, os::windows::ffi::OsStrExt, ptr, rc::Rc};
    use windows::Win32::Foundation::{E_FAIL, S_FALSE, S_OK};
    use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;

    fn allocated_units(units: &[u16]) -> (*mut u16, usize) {
        let mut terminated = units.to_vec();
        terminated.push(0);
        let boxed = terminated.into_boxed_slice();
        let length = boxed.len();
        let raw = Box::into_raw(boxed);
        (raw.cast::<u16>(), length)
    }

    unsafe fn release_boxed_units(pointer: PWSTR, length: usize) {
        let slice = ptr::slice_from_raw_parts_mut(pointer.as_ptr(), length);
        // SAFETY: the test call leaked exactly one `Box<[u16]>` with this data
        // pointer and length, and the injected guard releases it once.
        drop(unsafe { Box::from_raw(slice) });
    }

    #[test]
    fn layout_uses_exact_stable_native_components() {
        let roaming = PathBuf::from(r"C:\native-roaming");
        let local = PathBuf::from(r"D:\native-local");
        let paths = ProjectPaths::from_roots(Ok(roaming.clone()), Ok(local.clone()));

        assert_eq!(
            paths.settings_file,
            Some(roaming.join(APPLICATION_IDENTIFIER).join("data").join("app.ron"))
        );
        assert_eq!(paths.logs_directory, Some(local.join(APPLICATION_IDENTIFIER).join("logs")));
        assert_eq!(
            paths.crashes_directory,
            Some(local.join(APPLICATION_IDENTIFIER).join("crashes"))
        );
        assert!(paths.is_complete());
        assert!(paths.issues.is_empty());
    }

    #[test]
    fn debug_output_reports_availability_without_absolute_paths() {
        let paths = ProjectPaths::from_roots(
            Ok(PathBuf::from(r"C:\private-roaming\customer")),
            Ok(PathBuf::from(r"D:\private-local\customer")),
        );

        let rendered = format!("{paths:?}");
        assert!(rendered.contains("settings_available: true"));
        assert!(rendered.contains("logs_available: true"));
        assert!(!rendered.contains("private-roaming"));
        assert!(!rendered.contains("private-local"));
        assert!(!rendered.contains("customer"));
    }

    #[test]
    fn independent_root_failures_keep_other_facilities_available() {
        let denied =
            ProjectPathError::from_hresult(ProjectPathFacility::Settings, E_ACCESSDENIED.0);
        let local = PathBuf::from(r"C:\local");
        let paths = ProjectPaths::from_roots(Err(denied), Ok(local));

        assert!(paths.settings_file.is_none());
        assert!(paths.logs_directory.is_some());
        assert!(paths.crashes_directory.is_some());
        assert_eq!(paths.issues, vec![denied]);

        let roaming = PathBuf::from(r"C:\roaming");
        let local_error = denied.with_facility(ProjectPathFacility::Logs);
        let paths = ProjectPaths::from_roots(Ok(roaming), Err(local_error));
        assert!(paths.settings_file.is_some());
        assert!(paths.logs_directory.is_none());
        assert!(paths.crashes_directory.is_none());
        assert_eq!(paths.issues.len(), 2);
        assert_eq!(paths.issues[0].facility, ProjectPathFacility::Logs);
        assert_eq!(paths.issues[1].facility, ProjectPathFacility::Crashes);
    }

    #[test]
    fn com_scope_classification_balances_success_but_not_existing_apartment() {
        assert_eq!(classify_com_initialization(S_OK.0), Ok(true));
        assert_eq!(classify_com_initialization(S_FALSE.0), Ok(true));
        assert_eq!(classify_com_initialization(RPC_E_CHANGED_MODE.0), Ok(false));
        assert_eq!(classify_com_initialization(E_FAIL.0), Err(E_FAIL.0));
    }

    #[test]
    fn real_resolution_uses_an_existing_sta_without_changing_its_mode() {
        struct TestApartment;
        impl Drop for TestApartment {
            fn drop(&mut self) {
                // SAFETY: this guard remains on the spawned thread and balances
                // its successful test-only `CoInitializeEx` call.
                unsafe { CoUninitialize() };
            }
        }

        std::thread::spawn(|| {
            // SAFETY: this is a fresh test thread with a null reserved pointer.
            let hresult =
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
            assert!(hresult.is_ok(), "fresh test STA initializes");
            let _apartment = TestApartment;

            let paths = ProjectPaths::resolve();
            assert!(paths.is_complete());
            assert!(paths.issues.is_empty());
        })
        .join()
        .expect("existing-STA resolution test does not panic");
    }

    #[test]
    fn failure_output_is_released_before_hresult_is_returned() {
        let released = Rc::new(Cell::new(0));
        let released_by_guard = Rc::clone(&released);
        let (pointer, length) = allocated_units(&[b'C' as u16, b':' as u16, b'\\' as u16]);

        let result = resolve_known_folder_with(
            ProjectPathFacility::Settings,
            |output| {
                // SAFETY: `output` is the live PWSTR slot owned by `WideOutput`.
                unsafe { output.write(PWSTR(pointer)) };
                E_ACCESSDENIED.0
            },
            move |allocation| {
                released_by_guard.set(released_by_guard.get() + 1);
                // SAFETY: this is the matching releaser for `allocated_units`.
                unsafe { release_boxed_units(allocation, length) };
            },
        );

        assert_eq!(
            result,
            Err(ProjectPathError::from_hresult(ProjectPathFacility::Settings, E_ACCESSDENIED.0))
        );
        assert_eq!(released.get(), 1);
    }

    #[test]
    fn successful_utf16_copy_is_lossless_and_allocation_is_released() {
        let native_units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16];
        let (pointer, length) = allocated_units(&native_units);
        let released = Rc::new(Cell::new(0));
        let released_by_guard = Rc::clone(&released);

        let path = resolve_known_folder_with(
            ProjectPathFacility::Logs,
            |output| {
                // SAFETY: `output` is the live PWSTR slot owned by `WideOutput`.
                unsafe { output.write(PWSTR(pointer)) };
                S_OK.0
            },
            move |allocation| {
                released_by_guard.set(released_by_guard.get() + 1);
                // SAFETY: this is the matching releaser for `allocated_units`.
                unsafe { release_boxed_units(allocation, length) };
            },
        )
        .expect("the native fixture is an absolute Windows path");

        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), native_units);
        assert_eq!(released.get(), 1);
    }

    #[test]
    fn successful_null_or_relative_output_is_invalid_native_data() {
        let released = Rc::new(Cell::new(0));
        let released_by_guard = Rc::clone(&released);
        let null_result = resolve_known_folder_with(
            ProjectPathFacility::Settings,
            |_| S_OK.0,
            move |_| released_by_guard.set(released_by_guard.get() + 1),
        );
        assert!(matches!(
            null_result,
            Err(ProjectPathError {
                kind: ProjectPathErrorKind::InvalidData,
                hresult: Some(value),
                ..
            }) if value == E_UNEXPECTED.0
        ));
        assert_eq!(released.get(), 0);

        let (pointer, length) = allocated_units(&[b'r' as u16, b'e' as u16, b'l' as u16]);
        let relative_result = resolve_known_folder_with(
            ProjectPathFacility::Logs,
            |output| {
                // SAFETY: `output` is the live PWSTR slot owned by `WideOutput`.
                unsafe { output.write(PWSTR(pointer)) };
                S_OK.0
            },
            move |allocation| {
                // SAFETY: this is the matching releaser for `allocated_units`.
                unsafe { release_boxed_units(allocation, length) };
            },
        );
        assert!(matches!(
            relative_result,
            Err(ProjectPathError { kind: ProjectPathErrorKind::InvalidData, .. })
        ));
    }

    #[test]
    fn directory_creation_is_explicit_scoped_and_injectable() {
        let paths = ProjectPaths::from_roots(
            Ok(PathBuf::from(r"C:\roaming")),
            Ok(PathBuf::from(r"D:\local")),
        );
        let observed = Cell::new(None::<PathBuf>);

        paths
            .create_directory_with(ProjectPathFacility::Settings, |path| {
                observed.set(Some(path.to_owned()));
                Ok(())
            })
            .expect("injected creation succeeds");

        assert_eq!(
            observed.take(),
            paths.settings_file.as_deref().and_then(Path::parent).map(Path::to_owned)
        );
    }

    #[test]
    fn unavailable_and_io_creation_errors_are_sanitized_without_paths() {
        let missing = ProjectPaths::default();
        let called = Cell::new(false);
        let error = missing
            .create_directory_with(ProjectPathFacility::Crashes, |_| {
                called.set(true);
                Ok(())
            })
            .expect_err("an unavailable facility cannot be created");
        assert_eq!(error.kind, ProjectPathErrorKind::Unavailable);
        assert!(!called.get());

        let paths = ProjectPaths::from_roots(
            Ok(PathBuf::from(r"C:\roaming")),
            Ok(PathBuf::from(r"C:\local")),
        );
        let error = paths
            .create_directory_with(ProjectPathFacility::Logs, |_| {
                Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.0 as i32))
            })
            .expect_err("injected access denial is returned");
        assert_eq!(error.operation, ProjectPathOperation::CreateDirectory);
        assert_eq!(error.kind, ProjectPathErrorKind::AccessDenied);
        assert_eq!(error.os_code, Some(ERROR_ACCESS_DENIED.0 as i32));
        assert!(!error.to_string().contains(r"C:\local"));

        let crash_called = Cell::new(false);
        paths
            .create_directory_with(ProjectPathFacility::Crashes, |_| {
                crash_called.set(true);
                Ok(())
            })
            .expect("a logs failure does not disable crash-directory creation");
        assert!(crash_called.get());
    }

    #[test]
    fn real_resolution_is_absolute_read_only_and_matches_known_folder_layout() {
        let before_current = std::env::current_dir().expect("current directory is readable");
        let paths = resolve_project_paths();
        let after_current = std::env::current_dir().expect("current directory remains readable");

        assert_eq!(before_current, after_current);
        assert!(paths.issues.is_empty(), "standard-user Known Folders resolve");
        assert!(paths.is_complete());
        let settings = paths.settings_file.expect("settings path resolved");
        let logs = paths.logs_directory.expect("logs path resolved");
        let crashes = paths.crashes_directory.expect("crashes path resolved");
        let com = ComScope::enter(ProjectPathFacility::Settings)
            .expect("test thread can use an existing or temporary COM apartment");
        let roaming =
            resolve_known_folder(&com, ProjectPathFacility::Settings, &FOLDERID_RoamingAppData)
                .expect("Roaming AppData resolves through the guarded raw call");
        let local = resolve_known_folder(&com, ProjectPathFacility::Logs, &FOLDERID_LocalAppData)
            .expect("Local AppData resolves through the guarded raw call");
        assert!(settings.is_absolute());
        assert!(logs.is_absolute());
        assert!(crashes.is_absolute());
        assert_eq!(settings, settings_file_from_root(&roaming));
        assert_eq!(logs, application_root(&local).join(LOGS_DIRECTORY_COMPONENT));
        assert_eq!(crashes, application_root(&local).join(CRASHES_DIRECTORY_COMPONENT));
    }

    #[test]
    fn errors_retain_exact_hresult_category_without_absolute_path() {
        let error = ProjectPathError::from_hresult(ProjectPathFacility::Settings, E_FAIL.0);
        assert_eq!(error.hresult, Some(E_FAIL.0));
        assert_eq!(error.kind, ProjectPathErrorKind::Other);
        assert!(!error.to_string().contains('\\'));
    }
}
