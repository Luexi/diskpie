//! Conventional Windows filesystem adapter.
//!
//! The adapter deliberately performs a one-directory-at-a-time Win32 walk. It
//! is the correctness baseline for every Windows filesystem; it does not make
//! NTFS assumptions or use an MFT/USN shortcut. Names and paths remain native
//! UTF-16 all the way to the portable `OsString`/`PathBuf` seam.
//!
//! Reparse-point directories and directories with risky Cloud Files attributes
//! are returned as non-traversable boundaries. Reparse-point files and offline
//! or partial placeholders are never metadata-opened, so their allocation and
//! identity remain explicitly unknown. Ordinary files use
//! `FILE_STANDARD_INFO` and `FILE_ID_INFO` through a zero-access,
//! non-inheritable handle opened with full sharing and
//! `FILE_FLAG_OPEN_REPARSE_POINT`.
//!
//! Limitations:
//! - the baseline opens every ordinary file and therefore favors correctness
//!   and broad filesystem support over NTFS-specific throughput;
//! - `WIN32_FIND_DATAW` has no allocation or 128-bit identity field, so policy-
//!   blocked files cannot provide those values without weakening no-follow and
//!   no-hydration guarantees;
//! - cancellation is checked between Win32 operations and every returned
//!   entry, but an in-flight filesystem-driver call may still block;
//! - reparse tags are retained in typed omission diagnostics because the
//!   current portable `FsEntry` model has no raw-tag field; and
//! - filesystem-reported allocation is not a promise of exclusively
//!   reclaimable sectors (for example with ReFS block cloning or deduplication).

use diskpie_core::{
    EntryKind, FileIdentity, MetricSource, OwnMetrics, ReparseKind, SizeMetric, UnknownReason,
    VolumeKey,
};
use diskpie_scan::{
    CancelToken, DirectoryItem, FsEntry, FsError, FsErrorKind, FsOperation, ScanFs, VisitControl,
};
use std::{
    ffi::{OsString, c_void},
    fs::{File, OpenOptions},
    io,
    mem::size_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Storage::{
            CloudFilters::{
                CF_PLACEHOLDER_STATE, CF_PLACEHOLDER_STATE_INVALID, CF_PLACEHOLDER_STATE_NO_STATES,
                CF_PLACEHOLDER_STATE_PARTIAL, CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK,
                CfGetPlaceholderStateFromAttributeTag,
            },
            FileSystem::{
                FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE,
                FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
                FIND_FIRST_EX_FLAGS, FIND_FIRST_EX_LARGE_FETCH, FileIdInfo, FileStandardInfo,
                FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
                GetFileAttributesW, GetFileInformationByHandleEx, INVALID_FILE_ATTRIBUTES,
                WIN32_FIND_DATAW,
            },
        },
    },
    core::PCWSTR,
};

const ERROR_INVALID_FUNCTION_CODE: i32 = 1;
const ERROR_FILE_NOT_FOUND_CODE: i32 = 2;
const ERROR_PATH_NOT_FOUND_CODE: i32 = 3;
const ERROR_ACCESS_DENIED_CODE: i32 = 5;
const ERROR_NO_MORE_FILES_CODE: i32 = 18;
const ERROR_NOT_SUPPORTED_CODE: i32 = 50;
const ERROR_INVALID_PARAMETER_CODE: i32 = 87;
const ERROR_OPERATION_ABORTED_CODE: i32 = 995;
const ERROR_CANCELLED_CODE: i32 = 1_223;

const IO_REPARSE_TAG_MOUNT_POINT_VALUE: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK_VALUE: u32 = 0xA000_000C;
const IO_REPARSE_TAG_CLOUD_VALUE: u32 = 0x9000_001A;
const IO_REPARSE_TAG_CLOUD_VARIANT_MASK: u32 = 0xFFFF_0FFF;

const WIDE_BACKSLASH: u16 = b'\\' as u16;
const WIDE_SLASH: u16 = b'/' as u16;
const WIDE_COLON: u16 = b':' as u16;
const WIDE_QUESTION: u16 = b'?' as u16;
const WIDE_STAR: u16 = b'*' as u16;

/// Conventional, filesystem-agnostic Windows implementation of [`ScanFs`].
///
/// A cancellation token is optional because the portable visitor already acts
/// as a cancellation checkpoint after every emitted item. Supplying the same
/// token to this adapter and `start_scan_with_cancel` adds checkpoints between
/// metadata operations as well.
#[derive(Clone, Debug, Default)]
pub struct WindowsFileSystem {
    cancel: Option<CancelToken>,
}

impl WindowsFileSystem {
    /// Creates an adapter that observes cancellation through the scan visitor.
    #[must_use]
    pub const fn new() -> Self {
        Self { cancel: None }
    }

    /// Creates an adapter with direct cancellation checkpoints between Win32
    /// operations.
    #[must_use]
    pub fn with_cancel_token(cancel: CancelToken) -> Self {
        Self { cancel: Some(cancel) }
    }

    fn check_cancel(&self, path: &Path, operation: FsOperation) -> Result<(), FsError> {
        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(FsError::new(path, operation, FsErrorKind::Interrupted)
                .with_detail("scan cancellation observed before a Windows filesystem operation"));
        }
        Ok(())
    }

    fn open_enumeration(
        &self,
        directory: &Path,
        search: &WideCString,
    ) -> Result<Option<(FindHandle, WIN32_FIND_DATAW)>, FsError> {
        let mut use_large_fetch = true;
        loop {
            self.check_cancel(directory, FsOperation::EnumerateDirectory)?;
            let mut data = WIN32_FIND_DATAW::default();
            let flags =
                if use_large_fetch { FIND_FIRST_EX_LARGE_FETCH } else { FIND_FIRST_EX_FLAGS(0) };

            // SAFETY: `search` is NUL-terminated for the duration of the call;
            // `data` is a correctly sized writable `WIN32_FIND_DATAW`; the
            // returned owned search handle is immediately put under RAII.
            let result = unsafe {
                FindFirstFileExW(
                    search.as_pcwstr(),
                    FindExInfoBasic,
                    (&raw mut data).cast::<c_void>(),
                    FindExSearchNameMatch,
                    None,
                    flags,
                )
            };

            match result {
                Ok(handle) => return Ok(Some((FindHandle(handle), data))),
                Err(error) => {
                    let code = win32_code(&error);
                    if code == ERROR_FILE_NOT_FOUND_CODE {
                        // `directory\\*` reports this for an empty directory.
                        return Ok(None);
                    }
                    if use_large_fetch && large_fetch_can_fallback(code) {
                        use_large_fetch = false;
                        continue;
                    }
                    return Err(fs_error_from_windows(
                        directory,
                        FsOperation::EnumerateDirectory,
                        error,
                    ));
                }
            }
        }
    }

    fn process_entry(
        &self,
        directory: &Path,
        directory_wide: &ExtendedPath,
        data: &WIN32_FIND_DATAW,
        name_units: &[u16],
    ) -> Result<ProcessedEntry, FsError> {
        let name = OsString::from_wide(name_units);
        let path = directory.join(&name);
        self.check_cancel(&path, FsOperation::ReadMetadata)?;

        let attributes = data.dwFileAttributes;
        let reparse_tag =
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 { data.dwReserved0 } else { 0 };
        let cloud_state = placeholder_state(attributes, reparse_tag);
        let classification = classify_entry(attributes, reparse_tag, cloud_state);
        let logical_size = find_data_size(data);

        match classification.metadata {
            MetadataDisposition::DirectoryZero => {
                let mut omissions = Vec::new();
                if classification.boundary {
                    omissions.push(policy_omission(&path, classification));
                }
                Ok(ProcessedEntry {
                    entry: FsEntry::new(name, classification.kind, OwnMetrics::ZERO_BY_POLICY),
                    omissions,
                })
            }
            MetadataDisposition::EnumerationOnly => Ok(ProcessedEntry {
                entry: FsEntry::new(
                    name,
                    classification.kind,
                    OwnMetrics::new(
                        SizeMetric::known(logical_size, MetricSource::PortableMetadata),
                        SizeMetric::unknown(UnknownReason::NotAvailable, MetricSource::Policy),
                    ),
                ),
                omissions: vec![policy_omission(&path, classification)],
            }),
            MetadataDisposition::QueryFile => {
                let child_wide = directory_wide.join_component(name_units);
                let queried = self.query_file_metadata(&path, &child_wide, logical_size)?;
                let mut entry = FsEntry::new(name, classification.kind, queried.metrics);
                if let Some(identity) = queried.identity {
                    entry = entry.with_file_identity(identity);
                }
                Ok(ProcessedEntry { entry, omissions: queried.omissions })
            }
        }
    }

    fn query_file_metadata(
        &self,
        path: &Path,
        wide_path: &ExtendedPath,
        enumeration_logical_size: u64,
    ) -> Result<QueriedFileMetadata, FsError> {
        self.check_cancel(path, FsOperation::ReadMetadata)?;
        let file = match open_metadata_file(wide_path) {
            Ok(file) => file,
            Err(error) => {
                let omission = fs_error_from_io(path, FsOperation::ReadMetadata, error);
                return Ok(QueriedFileMetadata {
                    metrics: OwnMetrics::new(
                        SizeMetric::known(enumeration_logical_size, MetricSource::PortableMetadata),
                        SizeMetric::unknown(
                            unknown_reason(omission.kind),
                            MetricSource::FilesystemAllocation,
                        ),
                    ),
                    identity: None,
                    omissions: vec![omission.with_detail(
                        "metadata open failed; allocation and hard-link identity are unavailable",
                    )],
                });
            }
        };

        self.check_cancel(path, FsOperation::ReadMetadata)?;
        let standard_result = query_standard_info(&file);
        self.check_cancel(path, FsOperation::ReadIdentity)?;
        let identity_result = query_file_identity(&file);
        self.check_cancel(path, FsOperation::ReadIdentity)?;

        let mut omissions = Vec::new();
        let (logical, allocated) = match standard_result {
            Ok(info) => {
                metrics_from_standard_info(path, enumeration_logical_size, info, &mut omissions)
            }
            Err(error) => {
                let omission = fs_error_from_windows(path, FsOperation::ReadMetadata, error);
                let reason = unknown_reason(omission.kind);
                omissions.push(omission);
                (
                    SizeMetric::known(enumeration_logical_size, MetricSource::PortableMetadata),
                    SizeMetric::unknown(reason, MetricSource::FilesystemAllocation),
                )
            }
        };

        let identity = match identity_result {
            Ok(info) => Some(file_identity(info)),
            Err(error) => {
                omissions.push(fs_error_from_windows(path, FsOperation::ReadIdentity, error));
                None
            }
        };

        Ok(QueriedFileMetadata {
            metrics: OwnMetrics::new(logical, allocated),
            identity,
            omissions,
        })
    }
}

impl ScanFs for WindowsFileSystem {
    fn visit_directory(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(DirectoryItem) -> VisitControl,
    ) -> Result<(), FsError> {
        let directory_wide = ExtendedPath::from_path(directory).map_err(|error| {
            path_conversion_error(directory, FsOperation::EnumerateDirectory, error)
        })?;

        self.check_cancel(directory, FsOperation::EnumerateDirectory)?;
        let attributes = query_path_attributes(&directory_wide)
            .map_err(|error| fs_error_from_io(directory, FsOperation::EnumerateDirectory, error))?;

        if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            return Err(FsError::new(directory, FsOperation::EnumerateDirectory, FsErrorKind::Io)
                .with_detail("scan work item is not a directory"));
        }
        if root_is_policy_boundary(attributes) {
            return Err(FsError::new(
                directory,
                FsOperation::EnumerateDirectory,
                FsErrorKind::NotSupported,
            )
            .with_detail(format!(
                "selected directory is a reparse/cloud boundary (attributes 0x{attributes:08X}); \
                 it was not followed or enumerated"
            )));
        }

        let search = directory_wide.search_pattern();
        let Some((handle, mut data)) = self.open_enumeration(directory, &search)? else {
            return Ok(());
        };

        loop {
            self.check_cancel(directory, FsOperation::EnumerateDirectory)?;
            let name_units = find_name_units(&data);
            if !is_dot_entry(name_units) {
                let processed =
                    self.process_entry(directory, &directory_wide, &data, name_units)?;
                if visitor(DirectoryItem::Entry(processed.entry)) == VisitControl::Stop {
                    return Ok(());
                }
                for omission in processed.omissions {
                    if visitor(DirectoryItem::Omission(omission.into())) == VisitControl::Stop {
                        return Ok(());
                    }
                }
            }

            self.check_cancel(directory, FsOperation::EnumerateDirectory)?;
            // SAFETY: `handle` owns a live find handle and `data` is a correctly
            // sized writable record. No pointer escapes this call.
            match unsafe { FindNextFileW(handle.raw(), &raw mut data) } {
                Ok(()) => {}
                Err(error) if win32_code(&error) == ERROR_NO_MORE_FILES_CODE => return Ok(()),
                Err(error) => {
                    return Err(fs_error_from_windows(
                        directory,
                        FsOperation::EnumerateDirectory,
                        error,
                    ));
                }
            }
        }
    }
}

#[derive(Debug)]
struct FindHandle(HANDLE);

impl FindHandle {
    const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for FindHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper is constructed only from a successful
        // `FindFirstFileExW` call and owns that handle exactly once.
        let _ = unsafe { FindClose(self.0) };
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtendedPath {
    units: Vec<u16>,
}

impl ExtendedPath {
    fn from_path(path: &Path) -> Result<Self, PathConversionError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::absolute(path).map_err(PathConversionError::MakeAbsolute)?
        };
        let units = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
        prefix_extended_absolute(&units).map(|units| Self { units })
    }

    fn join_component(&self, component: &[u16]) -> Self {
        let mut units = Vec::with_capacity(self.units.len() + component.len() + 1);
        units.extend_from_slice(&self.units);
        if units.last().copied() != Some(WIDE_BACKSLASH) {
            units.push(WIDE_BACKSLASH);
        }
        units.extend_from_slice(component);
        Self { units }
    }

    fn search_pattern(&self) -> WideCString {
        let mut units = Vec::with_capacity(self.units.len() + 2);
        units.extend_from_slice(&self.units);
        if units.last().copied() != Some(WIDE_BACKSLASH) {
            units.push(WIDE_BACKSLASH);
        }
        units.push(WIDE_STAR);
        WideCString::new(units)
    }

    fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(OsString::from_wide(&self.units))
    }

    fn as_wide_c_string(&self) -> WideCString {
        WideCString::new(self.units.clone())
    }
}

#[derive(Clone, Debug)]
struct WideCString(Vec<u16>);

impl WideCString {
    fn new(mut units: Vec<u16>) -> Self {
        units.push(0);
        Self(units)
    }

    const fn as_pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

#[derive(Debug)]
enum PathConversionError {
    MakeAbsolute(io::Error),
    EmbeddedNul,
    UnsupportedNamespace,
    InvalidDosOrUnc,
}

fn prefix_extended_absolute(units: &[u16]) -> Result<Vec<u16>, PathConversionError> {
    if units.contains(&0) {
        return Err(PathConversionError::EmbeddedNul);
    }

    // Extended syntax accepts only backslashes. Replacing separator spelling
    // does not resolve `.`/`..`, links, case, or any filesystem component.
    let mut path = units
        .iter()
        .copied()
        .map(|unit| if unit == WIDE_SLASH { WIDE_BACKSLASH } else { unit })
        .collect::<Vec<_>>();

    const VERBATIM_PREFIX: [u16; 4] =
        [WIDE_BACKSLASH, WIDE_BACKSLASH, WIDE_QUESTION, WIDE_BACKSLASH];
    const DEVICE_PREFIX: [u16; 4] = [WIDE_BACKSLASH, WIDE_BACKSLASH, b'.' as u16, WIDE_BACKSLASH];
    const VERBATIM_UNC_PREFIX: [u16; 8] = [
        WIDE_BACKSLASH,
        WIDE_BACKSLASH,
        WIDE_QUESTION,
        WIDE_BACKSLASH,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        WIDE_BACKSLASH,
    ];

    if starts_with_ascii_case_insensitive(&path, &VERBATIM_UNC_PREFIX) {
        return if valid_unc_tail(&path[VERBATIM_UNC_PREFIX.len()..]) {
            Ok(path)
        } else {
            Err(PathConversionError::InvalidDosOrUnc)
        };
    }
    if path.starts_with(&VERBATIM_PREFIX) {
        return if is_drive_absolute(&path[VERBATIM_PREFIX.len()..]) {
            Ok(path)
        } else {
            Err(PathConversionError::UnsupportedNamespace)
        };
    }
    if path.starts_with(&DEVICE_PREFIX) {
        return Err(PathConversionError::UnsupportedNamespace);
    }
    if path.starts_with(&[WIDE_BACKSLASH, WIDE_BACKSLASH]) {
        if !valid_unc_tail(&path[2..]) {
            return Err(PathConversionError::InvalidDosOrUnc);
        }
        let mut extended = Vec::with_capacity(path.len() + 6);
        extended.extend_from_slice(&VERBATIM_UNC_PREFIX);
        extended.extend_from_slice(&path[2..]);
        return Ok(extended);
    }
    if is_drive_absolute(&path) {
        let mut extended = Vec::with_capacity(path.len() + VERBATIM_PREFIX.len());
        extended.extend_from_slice(&VERBATIM_PREFIX);
        extended.append(&mut path);
        return Ok(extended);
    }

    Err(PathConversionError::InvalidDosOrUnc)
}

fn starts_with_ascii_case_insensitive(value: &[u16], prefix: &[u16]) -> bool {
    value.len() >= prefix.len()
        && value[..prefix.len()]
            .iter()
            .zip(prefix)
            .all(|(&left, &right)| ascii_upper(left) == ascii_upper(right))
}

const fn ascii_upper(unit: u16) -> u16 {
    if unit >= b'a' as u16 && unit <= b'z' as u16 { unit - (b'a' - b'A') as u16 } else { unit }
}

fn is_drive_absolute(units: &[u16]) -> bool {
    units.len() >= 3
        && ascii_upper(units[0]) >= b'A' as u16
        && ascii_upper(units[0]) <= b'Z' as u16
        && units[1] == WIDE_COLON
        && units[2] == WIDE_BACKSLASH
}

fn valid_unc_tail(tail: &[u16]) -> bool {
    let Some(server_end) = tail.iter().position(|&unit| unit == WIDE_BACKSLASH) else {
        return false;
    };
    if server_end == 0 {
        return false;
    }
    let share_and_rest = &tail[server_end + 1..];
    !share_and_rest.is_empty()
        && share_and_rest.iter().position(|&unit| unit == WIDE_BACKSLASH) != Some(0)
}

fn path_conversion_error(
    path: &Path,
    operation: FsOperation,
    error: PathConversionError,
) -> FsError {
    match error {
        PathConversionError::MakeAbsolute(error) => fs_error_from_io(path, operation, error),
        PathConversionError::EmbeddedNul => FsError::new(path, operation, FsErrorKind::Io)
            .with_detail("path contains a NUL code unit"),
        PathConversionError::UnsupportedNamespace => {
            FsError::new(path, operation, FsErrorKind::NotSupported)
                .with_detail("only absolute DOS and UNC filesystem paths are supported")
        }
        PathConversionError::InvalidDosOrUnc => FsError::new(path, operation, FsErrorKind::Io)
            .with_detail("path could not be represented as an absolute DOS or UNC path"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataDisposition {
    DirectoryZero,
    EnumerationOnly,
    QueryFile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EntryClassification {
    kind: EntryKind,
    metadata: MetadataDisposition,
    boundary: bool,
    attributes: u32,
    reparse_tag: u32,
    cloud_state: u32,
    is_reparse: bool,
    cloud_risk: bool,
}

fn classify_entry(
    attributes: u32,
    reparse_tag: u32,
    cloud_state: CF_PLACEHOLDER_STATE,
) -> EntryClassification {
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let is_reparse = attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
    let valid_cloud_state = cloud_state != CF_PLACEHOLDER_STATE_INVALID;
    let cloud_risk = attributes & risky_cloud_attributes() != 0
        || valid_cloud_state
            && (cloud_state.contains(CF_PLACEHOLDER_STATE_PARTIAL)
                || cloud_state.contains(CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK));
    let boundary = is_reparse || cloud_risk;

    let kind = match (is_directory, boundary, is_reparse) {
        (true, true, _) => EntryKind::ReparsePoint(ReparseKind::Directory),
        (true, false, _) => EntryKind::Directory,
        (false, _, true) => EntryKind::ReparsePoint(ReparseKind::File),
        (false, _, false) => EntryKind::File,
    };
    let metadata = if is_directory {
        MetadataDisposition::DirectoryZero
    } else if boundary {
        MetadataDisposition::EnumerationOnly
    } else {
        MetadataDisposition::QueryFile
    };

    EntryClassification {
        kind,
        metadata,
        boundary,
        attributes,
        reparse_tag,
        cloud_state: cloud_state.0,
        is_reparse,
        cloud_risk,
    }
}

const fn risky_cloud_attributes() -> u32 {
    FILE_ATTRIBUTE_OFFLINE.0
        | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
}

fn root_is_policy_boundary(attributes: u32) -> bool {
    attributes & (FILE_ATTRIBUTE_REPARSE_POINT.0 | risky_cloud_attributes()) != 0
}

fn placeholder_state(attributes: u32, reparse_tag: u32) -> CF_PLACEHOLDER_STATE {
    if attributes & (FILE_ATTRIBUTE_REPARSE_POINT.0 | risky_cloud_attributes()) == 0 {
        return CF_PLACEHOLDER_STATE_NO_STATES;
    }
    // SAFETY: this Cloud Files classification API accepts two integer values,
    // dereferences no caller pointers, opens no file, and has no ownership
    // transfer. Both values came directly from `WIN32_FIND_DATAW`.
    unsafe { CfGetPlaceholderStateFromAttributeTag(attributes, reparse_tag) }
}

fn policy_omission(path: &Path, classification: EntryClassification) -> FsError {
    let operation =
        if matches!(classification.kind, EntryKind::ReparsePoint(ReparseKind::Directory)) {
            FsOperation::EnumerateDirectory
        } else {
            FsOperation::ReadMetadata
        };
    let detail = if classification.is_reparse {
        let tag_name = reparse_tag_name(classification.reparse_tag);
        if operation == FsOperation::EnumerateDirectory {
            format!(
                "directory reparse boundary ({tag_name}, tag 0x{:08X}, cloud state 0x{:08X}); \
                 target content was not followed",
                classification.reparse_tag, classification.cloud_state
            )
        } else {
            format!(
                "file reparse boundary ({tag_name}, tag 0x{:08X}, cloud state 0x{:08X}); \
                 target metadata was not opened, so allocation and identity are unavailable",
                classification.reparse_tag, classification.cloud_state
            )
        }
    } else if classification.cloud_risk && operation == FsOperation::EnumerateDirectory {
        format!(
            "cloud/offline directory boundary (attributes 0x{:08X}, state 0x{:08X}); \
             content was not enumerated",
            classification.attributes, classification.cloud_state
        )
    } else {
        format!(
            "cloud/offline policy prevented a metadata open (attributes 0x{:08X}, \
             state 0x{:08X}); allocation and identity are unavailable",
            classification.attributes, classification.cloud_state
        )
    };
    FsError::new(path, operation, FsErrorKind::NotSupported).with_detail(detail)
}

fn reparse_tag_name(tag: u32) -> &'static str {
    match tag {
        IO_REPARSE_TAG_MOUNT_POINT_VALUE => "mount point or junction",
        IO_REPARSE_TAG_SYMLINK_VALUE => "symbolic link",
        _ if tag & IO_REPARSE_TAG_CLOUD_VARIANT_MASK == IO_REPARSE_TAG_CLOUD_VALUE => {
            "Cloud Files placeholder"
        }
        _ => "unknown reparse tag",
    }
}

struct ProcessedEntry {
    entry: FsEntry,
    omissions: Vec<FsError>,
}

struct QueriedFileMetadata {
    metrics: OwnMetrics,
    identity: Option<FileIdentity>,
    omissions: Vec<FsError>,
}

fn open_metadata_file(path: &ExtendedPath) -> io::Result<File> {
    let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    let mut options = OpenOptions::new();
    options.access_mode(0).share_mode(share).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    options.open(path.to_path_buf())
}

fn query_standard_info(file: &File) -> windows::core::Result<FILE_STANDARD_INFO> {
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: the `File` keeps a valid owned handle alive, `info` is a
    // correctly sized writable buffer for `FileStandardInfo`, and the pointer
    // is confined to this synchronous call.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileStandardInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )?;
    }
    Ok(info)
}

fn query_file_identity(file: &File) -> windows::core::Result<FILE_ID_INFO> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the `File` keeps a valid owned handle alive, `info` is a
    // correctly sized writable buffer for `FileIdInfo`, and the pointer is
    // confined to this synchronous call.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )?;
    }
    Ok(info)
}

fn metrics_from_standard_info(
    path: &Path,
    enumeration_logical_size: u64,
    info: FILE_STANDARD_INFO,
    omissions: &mut Vec<FsError>,
) -> (SizeMetric, SizeMetric) {
    let logical = match u64::try_from(info.EndOfFile) {
        Ok(bytes) => SizeMetric::known(bytes, MetricSource::NativeFileInformation),
        Err(_) => {
            omissions.push(
                FsError::new(path, FsOperation::ReadMetadata, FsErrorKind::Io).with_detail(
                    "FILE_STANDARD_INFO returned a negative end-of-file; enumeration size retained",
                ),
            );
            SizeMetric::known(enumeration_logical_size, MetricSource::PortableMetadata)
        }
    };
    let allocated = match u64::try_from(info.AllocationSize) {
        Ok(bytes) => SizeMetric::known(bytes, MetricSource::FilesystemAllocation),
        Err(_) => {
            omissions.push(
                FsError::new(path, FsOperation::ReadMetadata, FsErrorKind::Io)
                    .with_detail("FILE_STANDARD_INFO returned a negative allocation size"),
            );
            SizeMetric::unknown(UnknownReason::IoError, MetricSource::FilesystemAllocation)
        }
    };
    (logical, allocated)
}

fn file_identity(info: FILE_ID_INFO) -> FileIdentity {
    // `FILE_ID_128` is opaque. Interpreting its bytes little-endian gives the
    // portable model a stable equality/hash value without assigning semantics
    // to individual fields.
    FileIdentity::new(
        VolumeKey::new(u128::from(info.VolumeSerialNumber)),
        u128::from_le_bytes(info.FileId.Identifier),
    )
}

fn query_path_attributes(path: &ExtendedPath) -> io::Result<u32> {
    let wide = path.as_wide_c_string();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path for the duration of
    // the call. The API reads no caller-owned output buffer.
    let attributes = unsafe { GetFileAttributesW(wide.as_pcwstr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        Err(io::Error::last_os_error())
    } else {
        Ok(attributes)
    }
}

fn find_name_units(data: &WIN32_FIND_DATAW) -> &[u16] {
    let length = data.cFileName.iter().position(|&unit| unit == 0).unwrap_or(data.cFileName.len());
    &data.cFileName[..length]
}

fn is_dot_entry(name: &[u16]) -> bool {
    name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16]
}

const fn find_data_size(data: &WIN32_FIND_DATAW) -> u64 {
    (data.nFileSizeHigh as u64) << 32 | data.nFileSizeLow as u64
}

fn large_fetch_can_fallback(code: i32) -> bool {
    matches!(
        code,
        ERROR_INVALID_FUNCTION_CODE | ERROR_NOT_SUPPORTED_CODE | ERROR_INVALID_PARAMETER_CODE
    )
}

fn fs_error_from_io(path: &Path, operation: FsOperation, error: io::Error) -> FsError {
    let code = error.raw_os_error();
    let mut result = FsError::new(path, operation, error_kind(code)).with_detail(error.to_string());
    if let Some(code) = code {
        result = result.with_os_code(code);
    }
    result
}

fn fs_error_from_windows(
    path: &Path,
    operation: FsOperation,
    error: windows::core::Error,
) -> FsError {
    let code = win32_code(&error);
    FsError::new(path, operation, error_kind(Some(code)))
        .with_os_code(code)
        .with_detail(error.to_string())
}

fn win32_code(error: &windows::core::Error) -> i32 {
    let hresult = error.code().0;
    let raw = hresult as u32;
    if raw & 0xFFFF_0000 == 0x8007_0000 { (raw & 0xFFFF) as i32 } else { hresult }
}

fn error_kind(code: Option<i32>) -> FsErrorKind {
    match code {
        Some(ERROR_ACCESS_DENIED_CODE) => FsErrorKind::AccessDenied,
        Some(ERROR_FILE_NOT_FOUND_CODE | ERROR_PATH_NOT_FOUND_CODE) => FsErrorKind::NotFound,
        Some(
            ERROR_INVALID_FUNCTION_CODE | ERROR_NOT_SUPPORTED_CODE | ERROR_INVALID_PARAMETER_CODE,
        ) => FsErrorKind::NotSupported,
        Some(ERROR_OPERATION_ABORTED_CODE | ERROR_CANCELLED_CODE) => FsErrorKind::Interrupted,
        _ => FsErrorKind::Io,
    }
}

const fn unknown_reason(kind: FsErrorKind) -> UnknownReason {
    match kind {
        FsErrorKind::AccessDenied => UnknownReason::AccessDenied,
        FsErrorKind::NotSupported => UnknownReason::NotSupported,
        FsErrorKind::NotFound => UnknownReason::NotAvailable,
        FsErrorKind::Interrupted => UnknownReason::Cancelled,
        FsErrorKind::Io | FsErrorKind::ProviderFailure => UnknownReason::IoError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_scan::ScanOmission;
    use std::{
        ffi::OsStr,
        fs,
        os::windows::fs::{symlink_dir, symlink_file},
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diskpie-windows-fs-{label}-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn encode(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().collect()
    }

    fn decode(value: &[u16]) -> OsString {
        OsString::from_wide(value)
    }

    fn collect(directory: &Path) -> Result<Vec<DirectoryItem>, FsError> {
        let mut items = Vec::new();
        WindowsFileSystem::new().visit_directory(directory, &mut |item| {
            items.push(item);
            VisitControl::Continue
        })?;
        Ok(items)
    }

    fn entries(items: &[DirectoryItem]) -> Vec<&FsEntry> {
        items
            .iter()
            .filter_map(|item| match item {
                DirectoryItem::Entry(entry) => Some(entry),
                DirectoryItem::Omission(_) => None,
            })
            .collect()
    }

    fn omissions(items: &[DirectoryItem]) -> Vec<&ScanOmission> {
        items
            .iter()
            .filter_map(|item| match item {
                DirectoryItem::Entry(_) => None,
                DirectoryItem::Omission(omission) => Some(omission),
            })
            .collect()
    }

    #[test]
    fn prefixes_absolute_dos_paths_without_resolving_components() {
        let input = encode(r"C:\one\..\two\file.bin");
        let output = prefix_extended_absolute(&input).expect("DOS path");
        assert_eq!(decode(&output), OsString::from(r"\\?\C:\one\..\two\file.bin"));
    }

    #[test]
    fn converts_unc_and_preserves_existing_extended_paths() {
        let unc = prefix_extended_absolute(&encode(r"\\server\share\folder")).expect("UNC path");
        assert_eq!(decode(&unc), OsString::from(r"\\?\UNC\server\share\folder"));

        let extended = encode(r"\\?\C:\already\long");
        assert_eq!(prefix_extended_absolute(&extended).expect("extended DOS path"), extended);
        let extended_unc = encode(r"\\?\UNC\server\share\already");
        assert_eq!(
            prefix_extended_absolute(&extended_unc).expect("extended UNC path"),
            extended_unc
        );
    }

    #[test]
    fn path_conversion_preserves_non_scalar_utf16_and_rejects_device_namespaces() {
        let mut raw = encode(r"C:\raw\");
        raw.extend_from_slice(&[0xD800, b'x' as u16]);
        let output = prefix_extended_absolute(&raw).expect("raw UTF-16 DOS path");
        assert_eq!(&output[output.len() - 2..], &[0xD800, b'x' as u16]);

        assert!(matches!(
            prefix_extended_absolute(&encode(r"\\.\PhysicalDrive0")),
            Err(PathConversionError::UnsupportedNamespace)
        ));
        assert!(matches!(
            prefix_extended_absolute(&encode(r"\\server-only")),
            Err(PathConversionError::InvalidDosOrUnc)
        ));
    }

    #[test]
    fn pure_classification_fails_closed_for_reparse_and_cloud_boundaries() {
        let ordinary = classify_entry(0, 0, CF_PLACEHOLDER_STATE_NO_STATES);
        assert_eq!(ordinary.kind, EntryKind::File);
        assert_eq!(ordinary.metadata, MetadataDisposition::QueryFile);
        assert!(!ordinary.boundary);

        let directory =
            classify_entry(FILE_ATTRIBUTE_DIRECTORY.0, 0, CF_PLACEHOLDER_STATE_NO_STATES);
        assert_eq!(directory.kind, EntryKind::Directory);
        assert_eq!(directory.metadata, MetadataDisposition::DirectoryZero);

        let junction = classify_entry(
            FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0,
            IO_REPARSE_TAG_MOUNT_POINT_VALUE,
            CF_PLACEHOLDER_STATE_NO_STATES,
        );
        assert_eq!(junction.kind, EntryKind::ReparsePoint(ReparseKind::Directory));
        assert!(junction.boundary);

        let partial = classify_entry(0, 0, CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK);
        assert_eq!(partial.kind, EntryKind::File);
        assert_eq!(partial.metadata, MetadataDisposition::EnumerationOnly);
        assert!(partial.cloud_risk);

        let recall_directory = classify_entry(
            FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0,
            0,
            CF_PLACEHOLDER_STATE_NO_STATES,
        );
        assert_eq!(recall_directory.kind, EntryKind::ReparsePoint(ReparseKind::Directory));
        assert!(recall_directory.boundary);
    }

    #[test]
    fn enumerates_one_level_with_unicode_and_emoji_names() {
        let temp = TestDirectory::new("unicode");
        let file_name = OsString::from("r\u{e9}sum\u{e9}-\u{1f680}.bin");
        let directory_name = OsString::from("\u{65e5}\u{672c}\u{8a9e}-\u{1f4c1}");
        fs::write(temp.path().join(&file_name), b"seven!!").expect("write Unicode file");
        fs::create_dir(temp.path().join(&directory_name)).expect("create Unicode directory");
        fs::write(temp.path().join(&directory_name).join("nested.bin"), b"hidden")
            .expect("write nested file");

        let items = collect(temp.path()).expect("enumerate temp directory");
        let found = entries(&items);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|entry| entry.name == file_name));
        assert!(found.iter().any(|entry| entry.name == directory_name));
        assert!(!found.iter().any(|entry| entry.name == OsStr::new("nested.bin")));

        let file = found.iter().find(|entry| entry.name == file_name).expect("Unicode file entry");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.own_metrics.logical().known_bytes(), Some(7));
        assert!(file.own_metrics.allocated().known_bytes().is_some());
        assert!(file.file_identity.is_some());
    }

    #[test]
    fn hard_links_share_volume_and_128_bit_file_identity() {
        let temp = TestDirectory::new("hardlinks");
        let first = temp.path().join("first.bin");
        let second = temp.path().join("second.bin");
        fs::write(&first, vec![0x5A; 8_192]).expect("write hard-link source");
        fs::hard_link(&first, &second).expect("create hard link");

        let items = collect(temp.path()).expect("enumerate hard links");
        let found = entries(&items);
        let first =
            found.iter().find(|entry| entry.name == OsStr::new("first.bin")).expect("first link");
        let second =
            found.iter().find(|entry| entry.name == OsStr::new("second.bin")).expect("second link");
        assert_eq!(first.file_identity, second.file_identity);
        assert!(first.file_identity.is_some());
        assert_eq!(
            first.own_metrics.allocated().known_bytes(),
            second.own_metrics.allocated().known_bytes()
        );
    }

    #[test]
    fn reparse_files_are_reported_but_never_target_opened() {
        let temp = TestDirectory::new("reparse-file");
        let target = temp.path().join("target.bin");
        let link = temp.path().join("link.bin");
        fs::write(&target, vec![0xA5; 4_096]).expect("write symlink target");
        if let Err(error) = symlink_file(&target, &link) {
            if matches!(error.raw_os_error(), Some(5 | 1_314)) {
                return;
            }
            panic!("create file symlink: {error}");
        }

        let items = collect(temp.path()).expect("enumerate symlink");
        let link = entries(&items)
            .into_iter()
            .find(|entry| entry.name == OsStr::new("link.bin"))
            .expect("symlink entry");
        assert_eq!(link.kind, EntryKind::ReparsePoint(ReparseKind::File));
        assert_eq!(link.own_metrics.allocated().known_bytes(), None);
        assert_eq!(link.own_metrics.allocated().source(), MetricSource::Policy);
        assert!(link.file_identity.is_none());
        assert!(omissions(&items).iter().any(|omission| {
            omission.error.path.ends_with("link.bin")
                && omission.error.kind == FsErrorKind::NotSupported
        }));
    }

    #[test]
    fn directory_reparse_points_are_boundaries_and_reparse_roots_are_rejected() {
        let temp = TestDirectory::new("reparse-directory");
        let target = temp.path().join("target");
        let link = temp.path().join("linked-directory");
        fs::create_dir(&target).expect("create symlink target directory");
        fs::write(target.join("nested.bin"), b"must not be followed").expect("write target child");
        if let Err(error) = symlink_dir(&target, &link) {
            if matches!(error.raw_os_error(), Some(5 | 1_314)) {
                return;
            }
            panic!("create directory symlink: {error}");
        }

        let items = collect(temp.path()).expect("enumerate directory symlink");
        let link_entry = entries(&items)
            .into_iter()
            .find(|entry| entry.name == OsStr::new("linked-directory"))
            .expect("directory symlink entry");
        assert_eq!(link_entry.kind, EntryKind::ReparsePoint(ReparseKind::Directory));
        assert!(
            omissions(&items)
                .iter()
                .any(|omission| omission.error.path.ends_with("linked-directory"))
        );

        let root_error = collect(&link).expect_err("reparse root must not be followed");
        assert_eq!(root_error.operation, FsOperation::EnumerateDirectory);
        assert_eq!(root_error.kind, FsErrorKind::NotSupported);
    }

    #[test]
    fn disappearing_entries_do_not_abort_or_panic() {
        let temp = TestDirectory::new("disappearing");
        fs::write(temp.path().join("000-trigger.bin"), b"trigger").expect("write trigger");
        for index in 1..64 {
            fs::write(temp.path().join(format!("{index:03}.bin")), b"middle")
                .expect("write middle entry");
        }
        let disappearing = temp.path().join("999-gone.bin");
        fs::write(&disappearing, b"gone").expect("write disappearing entry");

        let mut items = Vec::new();
        WindowsFileSystem::new()
            .visit_directory(temp.path(), &mut |item| {
                let remove_now = matches!(
                    &item,
                    DirectoryItem::Entry(entry) if entry.name == OsStr::new("000-trigger.bin")
                );
                items.push(item);
                if remove_now {
                    fs::remove_file(&disappearing).expect("remove future entry");
                }
                VisitControl::Continue
            })
            .expect("a disappearing child is recoverable");

        for omission in omissions(&items) {
            if omission.error.path.ends_with("999-gone.bin") {
                assert_eq!(omission.error.kind, FsErrorKind::NotFound);
            }
        }
    }

    #[test]
    fn direct_cancel_token_stops_before_opening_directory() {
        let temp = TestDirectory::new("cancel");
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut called = false;
        let error = WindowsFileSystem::with_cancel_token(cancel)
            .visit_directory(temp.path(), &mut |_| {
                called = true;
                VisitControl::Continue
            })
            .expect_err("cancelled scan");
        assert!(!called);
        assert_eq!(error.kind, FsErrorKind::Interrupted);
    }
}
