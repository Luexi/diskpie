//! Conventional Windows filesystem adapter.
//!
//! The adapter deliberately performs a one-directory-at-a-time Windows walk. It
//! is the correctness baseline for every Windows filesystem; it does not make
//! NTFS assumptions or use an MFT/USN shortcut. Names and paths remain native
//! UTF-16 all the way to the portable `OsString`/`PathBuf` seam.
//!
//! Reparse-point directories and directories with risky Cloud Files attributes
//! are returned as non-traversable boundaries. Reparse-point files and offline
//! or partial placeholders are never metadata-opened, so their allocation and
//! identity remain explicitly unknown. Ordinary files use
//! `FILE_STANDARD_INFO` and `FILE_ID_INFO` through non-inheritable metadata
//! handles opened relative to the retained directory, with full sharing,
//! no-reparse name resolution and no-recall semantics. Enumeration uses that
//! same directory handle, so renaming an ancestor cannot redirect later opens.
//!
//! Limitations:
//! - the baseline opens every ordinary file and therefore favors correctness
//!   and broad filesystem support over NTFS-specific throughput;
//! - enumeration metadata is used only for names, attributes, tags and logical
//!   fallback sizes; the conventional per-file allocation/identity policy is
//!   unchanged. Providers without extended directory information use the full
//!   directory class; unavailable reparse tags remain explicit boundaries;
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
    fs::File,
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
};
use windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{
            HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
            STATUS_REPARSE_POINT_ENCOUNTERED, UNICODE_STRING,
        },
        Storage::{
            CloudFilters::{
                CF_PLACEHOLDER_STATE, CF_PLACEHOLDER_STATE_INVALID, CF_PLACEHOLDER_STATE_NO_STATES,
                CF_PLACEHOLDER_STATE_PARTIAL, CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK,
                CfGetPlaceholderStateFromAttributeTag,
            },
            FileSystem::{
                FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE,
                FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
                FILE_FLAGS_AND_ATTRIBUTES, FILE_FULL_DIR_INFO, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO,
                FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
                FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo, FileBasicInfo,
                FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdExtdDirectoryInfo,
                FileIdExtdDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
                GetFileInformationByHandleEx, SYNCHRONIZE,
            },
        },
        System::IO::IO_STATUS_BLOCK,
    },
    core::PWSTR,
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
const DIRECTORY_BUFFER_U64S: usize = 8_192;

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

    fn process_entry(
        &self,
        directory: &Path,
        directory_handle: &File,
        data: &EnumeratedEntry,
    ) -> Result<ProcessedEntry, FsError> {
        let name = OsString::from_wide(&data.name);
        let path = directory.join(&name);
        self.check_cancel(&path, FsOperation::ReadMetadata)?;

        let attributes = data.attributes;
        let reparse_tag = data.reparse_tag;
        let cloud_state = placeholder_state(attributes, reparse_tag);
        let classification = classify_entry(attributes, reparse_tag, cloud_state);
        let logical_size = data.logical_size;

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
                let queried =
                    self.query_file_metadata(&path, directory_handle, &data.name, logical_size)?;
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
        directory_handle: &File,
        name: &[u16],
        enumeration_logical_size: u64,
    ) -> Result<QueriedFileMetadata, FsError> {
        self.check_cancel(path, FsOperation::ReadMetadata)?;
        let file = match open_relative_metadata_file(directory_handle, name) {
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
        // A file can become a reparse/cloud object after its directory record
        // was returned. The relative open never follows it and NO_RECALL avoids
        // recalling contents; verify its current attributes before size/ID reads.
        let tag = match query_attribute_tag(&file) {
            Ok(tag) => tag,
            Err(error) => {
                return Ok(QueriedFileMetadata {
                    metrics: OwnMetrics::new(
                        SizeMetric::known(enumeration_logical_size, MetricSource::PortableMetadata),
                        SizeMetric::unknown(UnknownReason::NotAvailable, MetricSource::Policy),
                    ),
                    identity: None,
                    omissions: vec![fs_error_from_windows(path, FsOperation::ReadMetadata, error)],
                });
            }
        };
        let current = classify_entry(
            tag.FileAttributes,
            tag.ReparseTag,
            placeholder_state(tag.FileAttributes, tag.ReparseTag),
        );
        if current.metadata != MetadataDisposition::QueryFile {
            return Ok(QueriedFileMetadata {
                metrics: OwnMetrics::new(
                    SizeMetric::known(enumeration_logical_size, MetricSource::PortableMetadata),
                    SizeMetric::unknown(UnknownReason::NotAvailable, MetricSource::Policy),
                ),
                identity: None,
                omissions: vec![policy_omission(path, current)],
            });
        }
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
        let handle = open_directory(&directory_wide)
            .map_err(|error| fs_error_from_io(directory, FsOperation::EnumerateDirectory, error))?;
        let attributes = query_attribute_tag(&handle).map_err(|error| {
            fs_error_from_windows(directory, FsOperation::EnumerateDirectory, error)
        })?;
        if attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            return Err(FsError::new(directory, FsOperation::EnumerateDirectory, FsErrorKind::Io)
                .with_detail("scan work item is not a directory"));
        }
        if root_is_policy_boundary(attributes.FileAttributes) {
            return Err(FsError::new(
                directory,
                FsOperation::EnumerateDirectory,
                FsErrorKind::NotSupported,
            )
            .with_detail(
                "opened directory is a reparse/cloud boundary; content was not enumerated",
            ));
        }
        let mut cursor = DirectoryCursor::new();

        loop {
            self.check_cancel(directory, FsOperation::EnumerateDirectory)?;
            let Some(entries) = cursor.read(&handle).map_err(|error| {
                fs_error_from_io(directory, FsOperation::EnumerateDirectory, error)
            })?
            else {
                return Ok(());
            };
            for data in entries {
                self.check_cancel(directory, FsOperation::EnumerateDirectory)?;
                let processed = self.process_entry(directory, &handle, &data)?;
                if visitor(DirectoryItem::Entry(processed.entry)) == VisitControl::Stop {
                    return Ok(());
                }
                for omission in processed.omissions {
                    if visitor(DirectoryItem::Omission(omission.into())) == VisitControl::Stop {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// The full class is a provider fallback, not a fallback to pathname traversal.
/// Missing reparse tags use zero (unknown), which the policy rejects closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryClass {
    Extended,
    Full,
}

#[derive(Debug)]
struct EnumeratedEntry {
    name: Vec<u16>,
    attributes: u32,
    reparse_tag: u32,
    logical_size: u64,
}

struct DirectoryCursor {
    storage: Vec<u64>,
    class: DirectoryClass,
    restart: bool,
}

impl DirectoryCursor {
    fn new() -> Self {
        Self {
            storage: vec![0; DIRECTORY_BUFFER_U64S],
            class: DirectoryClass::Extended,
            restart: true,
        }
    }

    fn read(&mut self, directory: &File) -> io::Result<Option<Vec<EnumeratedEntry>>> {
        loop {
            self.storage.fill(0);
            let class = match (self.class, self.restart) {
                (DirectoryClass::Extended, true) => FileIdExtdDirectoryRestartInfo,
                (DirectoryClass::Extended, false) => FileIdExtdDirectoryInfo,
                (DirectoryClass::Full, true) => FileFullDirectoryRestartInfo,
                (DirectoryClass::Full, false) => FileFullDirectoryInfo,
            };
            // SAFETY: the retained directory owns its enumeration cursor. The
            // initialized u64 allocation supplies 64 KiB aligned to eight bytes,
            // as both native directory record formats require. No pointer escapes.
            let result = unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(directory.as_raw_handle()),
                    class,
                    self.storage.as_mut_ptr().cast::<c_void>(),
                    (DIRECTORY_BUFFER_U64S * size_of::<u64>()) as u32,
                )
            };
            match result {
                Ok(()) => {
                    self.restart = false;
                    // SAFETY: every byte of this owned allocation is initialized;
                    // the immutable parser borrow ends before storage is reused.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            self.storage.as_ptr().cast::<u8>(),
                            self.storage.len() * size_of::<u64>(),
                        )
                    };
                    return parse_directory_records(bytes, self.class).map(Some);
                }
                Err(error) => {
                    let code = win32_code(&error);
                    if code == ERROR_NO_MORE_FILES_CODE {
                        return Ok(None);
                    }
                    if self.restart
                        && self.class == DirectoryClass::Extended
                        && information_class_can_fallback(code)
                    {
                        self.class = DirectoryClass::Full;
                        continue;
                    }
                    return Err(io::Error::from_raw_os_error(code));
                }
            }
        }
    }
}

fn invalid_directory_record() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid directory information record")
}

fn parse_directory_records(
    bytes: &[u8],
    class: DirectoryClass,
) -> io::Result<Vec<EnumeratedEntry>> {
    let name_offset = match class {
        DirectoryClass::Extended => offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
        DirectoryClass::Full => offset_of!(FILE_FULL_DIR_INFO, FileName),
    };
    // Both classes have this common header; neither is cast to a native struct.
    const EOF: usize = offset_of!(FILE_FULL_DIR_INFO, EndOfFile);
    const ATTRIBUTES: usize = offset_of!(FILE_FULL_DIR_INFO, FileAttributes);
    const NAME_LENGTH: usize = offset_of!(FILE_FULL_DIR_INFO, FileNameLength);
    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        let record = bytes.get(offset..).ok_or_else(invalid_directory_record)?;
        if record.len() < name_offset {
            return Err(invalid_directory_record());
        }
        let u32_at = |index| {
            u32::from_le_bytes(record[index..index + 4].try_into().expect("validated header"))
        };
        let next = u32_at(0) as usize;
        let name_length = u32_at(NAME_LENGTH) as usize;
        if name_length == 0 || !name_length.is_multiple_of(2) {
            return Err(invalid_directory_record());
        }
        let end = name_offset.checked_add(name_length).ok_or_else(invalid_directory_record)?;
        let name_bytes = record.get(name_offset..end).ok_or_else(invalid_directory_record)?;
        let name = name_bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        if !is_dot_entry(&name) {
            validate_child_name(&name)?;
            let attributes = u32_at(ATTRIBUTES);
            let logical_size = if attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                // Directory EOF is not a file measurement. Providers may leave
                // it unspecified; directories retain the zero-by-policy model.
                0
            } else {
                u64::try_from(i64::from_le_bytes(
                    record[EOF..EOF + 8].try_into().expect("validated header"),
                ))
                .map_err(|_| invalid_directory_record())?
            };
            let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                && class == DirectoryClass::Extended
            {
                u32_at(offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag))
            } else {
                0
            };
            entries.push(EnumeratedEntry { name, attributes, reparse_tag, logical_size });
        }
        if next == 0 {
            return Ok(entries);
        }
        if next < end || !next.is_multiple_of(8) || next >= record.len() {
            return Err(invalid_directory_record());
        }
        offset = offset.checked_add(next).ok_or_else(invalid_directory_record)?;
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

    fn nt_name(&self) -> Vec<u16> {
        // Only DOS/UNC extended paths pass construction. Their NT equivalent
        // changes \\?\ to \??\ without resolving any filesystem component.
        let mut units = self.units.clone();
        units[1] = WIDE_QUESTION;
        units
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

fn open_directory(path: &ExtendedPath) -> io::Result<File> {
    open_no_follow(None, &path.nt_name(), true)
}

fn open_relative_metadata_file(directory: &File, name: &[u16]) -> io::Result<File> {
    validate_child_name(name)?;
    open_no_follow(Some(directory), name, false)
}

/// `RootDirectory` pins relative lookups to the same object being enumerated.
/// OBJ_DONT_REPARSE rejects reparses in the initial absolute path too. No handle
/// is inheritable, all opens are read-only and fully shared, and synchronous
/// options keep the caller-owned IO_STATUS_BLOCK lifetime confined to this call.
fn open_no_follow(parent: Option<&File>, name: &[u16], directory: bool) -> io::Result<File> {
    let bytes = name.len().checked_mul(size_of::<u16>()).ok_or_else(invalid_directory_record)?;
    let length = u16::try_from(bytes).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "native path exceeds UNICODE_STRING capacity")
    })?;
    let mut units = name.to_vec();
    let name =
        UNICODE_STRING { Length: length, MaximumLength: length, Buffer: PWSTR(units.as_mut_ptr()) };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(HANDLE::default(), |file| HANDLE(file.as_raw_handle())),
        ObjectName: &raw const name,
        // The initial DOS/UNC path retains Win32's case-insensitive lookup.
        // Relative children preserve exact enumerated spelling, including in
        // case-sensitive directories, rather than forcing insensitive matching.
        Attributes: if parent.is_some() {
            OBJ_DONT_REPARSE
        } else {
            OBJ_DONT_REPARSE | OBJ_CASE_INSENSITIVE
        },
        ..Default::default()
    };
    let mut handle = HANDLE::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    let access = if directory {
        SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY
    } else {
        SYNCHRONIZE | FILE_READ_ATTRIBUTES
    };
    let kind = if directory {
        // FILE_DIRECTORY_FILE is incompatible with FILE_OPEN_NO_RECALL. The
        // caller validates directory kind and cloud policy on this same handle
        // before performing any enumeration.
        FILE_OPEN_NO_RECALL
    } else {
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_NO_RECALL
    };
    // SAFETY: the name, object attributes and status output remain live for this
    // synchronous call. The optional parent File retains its valid handle. Only
    // FILE_OPEN is used, with no writes, no handle inheritance and no recall.
    // Successful raw ownership is transferred immediately into File below.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            access,
            &raw const attributes,
            &raw mut status_block,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if status == STATUS_REPARSE_POINT_ENCOUNTERED {
        return Err(io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED_CODE));
    }
    if status.is_err() {
        // SAFETY: this pure conversion accepts any NTSTATUS and retains no
        // pointers or handles; it preserves the native error for typed omissions.
        return Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32));
    }
    // SAFETY: NtCreateFile succeeded and returned this newly owned, valid file
    // handle. File is now its unique closer, including unwinding/cancellation.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn validate_child_name(name: &[u16]) -> io::Result<()> {
    if name.is_empty()
        || is_dot_entry(name)
        || name.iter().any(|unit| matches!(*unit, 0 | WIDE_BACKSLASH | WIDE_SLASH | WIDE_COLON))
    {
        return Err(invalid_directory_record());
    }
    Ok(())
}

fn query_attribute_tag(file: &File) -> windows::core::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: File retains a valid handle and `info` is the correctly sized
    // writable output for FileAttributeTagInfo. No pointer escapes this call.
    let result = unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileAttributeTagInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    match result {
        Ok(()) => Ok(info),
        Err(error) if information_class_can_fallback(win32_code(&error)) => {
            let mut basic = FILE_BASIC_INFO::default();
            // SAFETY: the same retained handle is queried into a correctly sized
            // writable FileBasicInfo buffer. The missing reparse tag stays unknown;
            // its reparse attribute still makes the policy fail closed.
            unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(file.as_raw_handle()),
                    FileBasicInfo,
                    (&raw mut basic).cast::<c_void>(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )?;
            }
            Ok(FILE_ATTRIBUTE_TAG_INFO { FileAttributes: basic.FileAttributes, ReparseTag: 0 })
        }
        Err(error) => Err(error),
    }
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

fn is_dot_entry(name: &[u16]) -> bool {
    name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16]
}

fn information_class_can_fallback(code: i32) -> bool {
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
        os::windows::fs::{OpenOptionsExt, symlink_dir, symlink_file},
        path::PathBuf,
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

    /// Exact test-owned junction, removed before its enclosing fixture. The
    /// path is updated by tests that rename the junction itself.
    struct Junction(PathBuf);

    impl Drop for Junction {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }

    fn create_junction(link: &Path, target: &Path) -> io::Result<Junction> {
        use windows::Win32::{
            Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
            System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
        };
        #[repr(C)]
        struct MountPointData {
            tag: u32,
            data_length: u16,
            reserved: u16,
            substitute_offset: u16,
            substitute_length: u16,
            print_offset: u16,
            print_length: u16,
            paths: [u16; 1_024],
        }
        let target = std::path::absolute(target)?;
        let printable = target.as_os_str().encode_wide().collect::<Vec<_>>();
        let substitute = ExtendedPath::from_path(&target).expect("fixture native path").nt_name();
        let units = substitute.len() + printable.len() + 2;
        assert!(units <= 1_024, "test junction path bound");
        let mut data = MountPointData {
            tag: IO_REPARSE_TAG_MOUNT_POINT_VALUE,
            data_length: u16::try_from(8 + units * 2).expect("bounded fixture payload"),
            reserved: 0,
            substitute_offset: 0,
            substitute_length: u16::try_from(substitute.len() * 2).expect("bounded target"),
            print_offset: u16::try_from((substitute.len() + 1) * 2).expect("bounded target"),
            print_length: u16::try_from(printable.len() * 2).expect("bounded target"),
            paths: [0; 1_024],
        };
        data.paths[..substitute.len()].copy_from_slice(&substitute);
        let print_start = substitute.len() + 1;
        data.paths[print_start..print_start + printable.len()].copy_from_slice(&printable);
        fs::create_dir(link)?;
        let owned = Junction(link.to_path_buf());
        let directory = fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(link)?;
        let mut returned = 0;
        // SAFETY: only the directory just created by this fixture is modified.
        // The repr(C) input has the mount-point reparse layout and a checked
        // initialized length; its owned File stays live during this synchronous
        // operation. Neither the input nor output pointers escape the call.
        unsafe {
            DeviceIoControl(
                HANDLE(directory.as_raw_handle()),
                FSCTL_SET_REPARSE_POINT,
                Some((&raw const data).cast::<c_void>()),
                8 + u32::from(data.data_length),
                None,
                0,
                Some(&raw mut returned),
                None,
            )
        }
        .map_err(|error| io::Error::from_raw_os_error(win32_code(&error)))?;
        Ok(owned)
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

    #[test]
    fn retained_directory_handle_supports_policy_and_enumeration_queries() {
        let temp = TestDirectory::new("retained-handle");
        fs::write(temp.path().join("one.bin"), b"one").expect("write own fixture");
        let path = ExtendedPath::from_path(temp.path()).expect("native path");
        let file = open_directory(&path).expect("open retained native directory");
        let tag = query_attribute_tag(&file).expect("query retained directory attributes");
        assert_eq!(tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0, FILE_ATTRIBUTE_DIRECTORY.0);
        let mut cursor = DirectoryCursor::new();
        let entries = cursor.read(&file).expect("read retained directory").expect("first batch");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, encode("one.bin"));
        assert!(cursor.read(&file).expect("complete enumeration").is_none());
    }

    #[test]
    fn full_directory_information_fallback_keeps_names_and_reparse_boundaries() {
        let temp = TestDirectory::new("full-directory-class");
        fs::write(temp.path().join("résumé-🚀.bin"), b"seven!!").expect("write fixture file");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).expect("create fixture directory");
        let _junction = create_junction(&temp.path().join("link"), &outside).expect("junction");
        let handle = open_directory(&ExtendedPath::from_path(temp.path()).unwrap()).unwrap();
        let mut cursor = DirectoryCursor::new();
        cursor.class = DirectoryClass::Full;
        let entries = cursor.read(&handle).unwrap().unwrap();
        let file = entries.iter().find(|entry| entry.name == encode("résumé-🚀.bin")).unwrap();
        assert_eq!(file.logical_size, 7);
        let link = entries.iter().find(|entry| entry.name == encode("link")).unwrap();
        assert_eq!(link.reparse_tag, 0, "fallback cannot fabricate the missing tag");
        let processed = WindowsFileSystem::new().process_entry(temp.path(), &handle, link).unwrap();
        assert_eq!(processed.entry.kind, EntryKind::ReparsePoint(ReparseKind::Directory));
        assert!(!processed.omissions.is_empty());
    }

    fn encoded_record(class: DirectoryClass, name: &[u16], attributes: u32) -> Vec<u8> {
        let offset = match class {
            DirectoryClass::Extended => offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
            DirectoryClass::Full => offset_of!(FILE_FULL_DIR_INFO, FileName),
        };
        let mut bytes = vec![0; (offset + name.len() * 2).next_multiple_of(8)];
        let name_length = offset_of!(FILE_FULL_DIR_INFO, FileNameLength);
        bytes[name_length..name_length + 4].copy_from_slice(&(name.len() as u32 * 2).to_le_bytes());
        let attribute_offset = offset_of!(FILE_FULL_DIR_INFO, FileAttributes);
        bytes[attribute_offset..attribute_offset + 4].copy_from_slice(&attributes.to_le_bytes());
        for (index, unit) in name.iter().enumerate() {
            bytes[offset + index * 2..offset + index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn directory_parser_preserves_native_names_and_rejects_malformed_buffers() {
        for class in [DirectoryClass::Extended, DirectoryClass::Full] {
            let raw_name = [0xd800, b'x' as u16];
            let valid = encoded_record(class, &raw_name, 0);
            assert_eq!(parse_directory_records(&valid, class).unwrap()[0].name, raw_name);
            assert!(parse_directory_records(&[], class).is_err());
            assert!(parse_directory_records(&valid[..8], class).is_err());
            for next in [1_u32, 8, u32::MAX, valid.len() as u32] {
                let mut malformed = valid.clone();
                malformed[..4].copy_from_slice(&next.to_le_bytes());
                assert!(parse_directory_records(&malformed, class).is_err());
            }
            for length in [0_u32, 1, u32::MAX] {
                let mut malformed = valid.clone();
                let offset = offset_of!(FILE_FULL_DIR_INFO, FileNameLength);
                malformed[offset..offset + 4].copy_from_slice(&length.to_le_bytes());
                assert!(parse_directory_records(&malformed, class).is_err());
            }
            for name in [encode("a\\b"), encode("a/b"), encode("a:b"), vec![0]] {
                assert!(parse_directory_records(&encoded_record(class, &name, 0), class).is_err());
            }
            let mut negative = valid;
            let offset = offset_of!(FILE_FULL_DIR_INFO, EndOfFile);
            negative[offset..offset + 8].copy_from_slice(&(-1_i64).to_le_bytes());
            assert!(parse_directory_records(&negative, class).is_err());
            let attributes = offset_of!(FILE_FULL_DIR_INFO, FileAttributes);
            negative[attributes..attributes + 4]
                .copy_from_slice(&FILE_ATTRIBUTE_DIRECTORY.0.to_le_bytes());
            assert_eq!(parse_directory_records(&negative, class).unwrap()[0].logical_size, 0);
        }
    }

    #[test]
    fn empty_directory_finishes_and_file_roots_are_rejected() {
        let temp = TestDirectory::new("empty");
        assert!(collect(temp.path()).unwrap().is_empty());
        let file = temp.path().join("file.bin");
        fs::write(&file, b"one").expect("write own fixture");
        assert!(collect(&file).is_err());
    }

    #[test]
    fn native_open_accepts_case_insensitive_dos_root_spelling() {
        let temp = TestDirectory::new("root-case");
        fs::write(temp.path().join("one.bin"), b"one").expect("write own fixture");
        let mut units = temp.path().as_os_str().encode_wide().collect::<Vec<_>>();
        let drive = ascii_upper(units[0]);
        if !(u16::from(b'A')..=u16::from(b'Z')).contains(&drive) {
            return;
        }
        units[0] = drive + u16::from(b'a' - b'A');
        let lower_drive = PathBuf::from(OsString::from_wide(&units));
        assert_eq!(entries(&collect(&lower_drive).unwrap()).len(), 1);
    }

    #[test]
    fn retained_native_paths_support_long_directories_and_non_scalar_names() {
        let temp = TestDirectory::new("long-native");
        let initial =
            prefix_extended_absolute(&temp.path().as_os_str().encode_wide().collect::<Vec<_>>())
                .expect("extended fixture path");
        let mut directory = PathBuf::from(OsString::from_wide(&initial));
        for level in 0..12 {
            directory.push(format!("level-{level:02}-{}", "資料".repeat(12)));
            fs::create_dir(&directory).expect("create owned long-path component");
        }
        assert!(directory.as_os_str().encode_wide().count() > 260);
        let native_name = OsString::from_wide(&[0xd800, b'x' as u16]);
        fs::write(directory.join(&native_name), b"native").expect("write owned raw-UTF16 file");
        let items = collect(&directory).expect("enumerate retained long directory");
        let entries = entries(&items);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, native_name);
        assert_eq!(entries[0].own_metrics.logical().known_bytes(), Some(6));
        assert!(entries[0].file_identity.is_some());
        assert!(omissions(&items).is_empty());
    }

    #[test]
    fn cancellation_during_retained_batch_stops_before_the_next_entry() {
        let temp = TestDirectory::new("cancel-batch");
        for index in 0..8 {
            fs::write(temp.path().join(format!("file-{index}.bin")), b"one").expect("own file");
        }
        let token = CancelToken::new();
        let mut visited = 0;
        let failure = WindowsFileSystem::with_cancel_token(token.clone())
            .visit_directory(temp.path(), &mut |item| {
                if matches!(item, DirectoryItem::Entry(_)) {
                    visited += 1;
                    token.cancel();
                }
                VisitControl::Continue
            })
            .expect_err("cancelled batch");
        assert_eq!(visited, 1);
        assert_eq!(failure.kind, FsErrorKind::Interrupted);
        // All provider handles close on cancellation; an immediate fresh visit
        // has its own cursor and can enumerate the complete untouched fixture.
        assert_eq!(entries(&collect(temp.path()).unwrap()).len(), 8);
    }

    #[test]
    fn disappearance_after_enumeration_reports_unknown_allocation_and_an_omission() {
        let temp = TestDirectory::new("disappeared-record");
        let disappearing = temp.path().join("gone.bin");
        fs::write(&disappearing, b"old").expect("write disappearing fixture");
        fs::write(temp.path().join("kept.bin"), b"kept").expect("write surviving fixture");
        let handle = open_directory(&ExtendedPath::from_path(temp.path()).unwrap()).unwrap();
        let records = DirectoryCursor::new().read(&handle).unwrap().unwrap();
        fs::remove_file(&disappearing).expect("remove only the fixture file created above");
        let gone = records.iter().find(|entry| entry.name == encode("gone.bin")).unwrap();
        let processed = WindowsFileSystem::new().process_entry(temp.path(), &handle, gone).unwrap();
        assert_eq!(processed.entry.own_metrics.logical().known_bytes(), Some(3));
        assert_eq!(processed.entry.own_metrics.allocated().known_bytes(), None);
        assert!(processed.entry.file_identity.is_none());
        assert_eq!(processed.omissions.len(), 1);
        assert_eq!(processed.omissions[0].kind, FsErrorKind::NotFound);
        assert_eq!(processed.omissions[0].operation, FsOperation::ReadMetadata);
        let kept = records.iter().find(|entry| entry.name == encode("kept.bin")).unwrap();
        let processed = WindowsFileSystem::new().process_entry(temp.path(), &handle, kept).unwrap();
        assert_eq!(processed.entry.own_metrics.logical().known_bytes(), Some(4));
        assert!(processed.omissions.is_empty());
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
    fn assert_renamed_directory_cannot_redirect_metadata(replace_ancestor: bool) {
        let temp = TestDirectory::new("retained-directory");
        let parent = temp.path().join("parent");
        let selected = parent.join("selected");
        let outside_parent = temp.path().join("outside-own-fixture");
        let outside = outside_parent.join("selected");
        fs::create_dir_all(&selected).expect("create selected fixture");
        fs::create_dir_all(&outside).expect("create outside fixture");
        for name in ["a.bin", "b.bin", "c.bin"] {
            fs::write(selected.join(name), b"old").expect("write original fixture file");
            fs::write(outside.join(name), vec![0; 8_192]).expect("write outside sentinel");
        }
        let replace = if replace_ancestor { &parent } else { &selected };
        let target = if replace_ancestor { &outside_parent } else { &outside };
        let moved = temp.path().join("original-retained");
        let mut junction = create_junction(&temp.path().join("prepared-junction"), target)
            .expect("create junction");
        let mut attempted = false;
        let mut replaced = false;
        let mut entries = Vec::new();
        let mut omissions = Vec::new();
        WindowsFileSystem::new()
            .visit_directory(&selected, &mut |item| {
                match item {
                    DirectoryItem::Entry(entry) => {
                        entries.push(entry);
                        if !attempted {
                            // This interleaving occurs after the directory was opened
                            // but before the next child's metadata is requested. Every
                            // renamed path and both targets belong to this fixture.
                            attempted = true;
                            match fs::rename(replace, &moved) {
                                Ok(()) => {
                                    fs::rename(&junction.0, replace)
                                        .expect("install prepared fixture junction");
                                    junction.0 = replace.clone();
                                    replaced = true;
                                }
                                Err(error)
                                    if replace_ancestor
                                        && matches!(error.raw_os_error(), Some(5 | 32)) =>
                                {
                                    // Some Windows filesystems refuse renaming
                                    // an ancestor while a descendant directory
                                    // handle is open. This also prevents escape;
                                    // continue asserting all original metadata.
                                }
                                Err(error) => panic!("rename own fixture directory: {error}"),
                            }
                        }
                    }
                    DirectoryItem::Omission(omission) => omissions.push(omission),
                }
                VisitControl::Continue
            })
            .expect("retained enumeration continues on the original directory");
        assert!(attempted);
        assert!(replaced || replace_ancestor, "the direct-directory swap must run");
        assert_eq!(entries.len(), 3);
        assert!(omissions.is_empty(), "relative metadata remains available: {omissions:?}");
        for entry in entries {
            assert_eq!(entry.own_metrics.logical().known_bytes(), Some(3));
            assert!(entry.own_metrics.allocated().known_bytes().is_some());
            assert!(
                entry.file_identity.is_some(),
                "metadata must come from the retained directory"
            );
        }
        assert_eq!(fs::metadata(outside.join("b.bin")).expect("outside sentinel").len(), 8_192);
        let original = if !replaced {
            selected
        } else if replace_ancestor {
            moved.join("selected")
        } else {
            moved
        };
        assert_eq!(fs::metadata(original.join("b.bin")).expect("retained original").len(), 3);
        // Junction drops first and removes only its owned reparse point; the fixture
        // then removes the two directories it created, never following the junction.
    }

    #[test]
    fn replacing_open_directory_with_junction_does_not_redirect_later_metadata() {
        assert_renamed_directory_cannot_redirect_metadata(false);
    }

    #[test]
    fn replacing_ancestor_with_junction_does_not_redirect_later_metadata() {
        assert_renamed_directory_cannot_redirect_metadata(true);
    }

    #[test]
    fn directory_below_a_junction_is_rejected_before_any_entry_is_emitted() {
        let temp = TestDirectory::new("ancestor-junction");
        let outside = temp.path().join("outside-own-fixture");
        fs::create_dir_all(outside.join("selected")).expect("create outside fixture");
        fs::write(outside.join("selected/sentinel.bin"), b"must not be scanned")
            .expect("write outside sentinel");
        let junction =
            create_junction(&temp.path().join("link"), &outside).expect("create junction");
        let mut visited = false;
        let failure = WindowsFileSystem::new()
            .visit_directory(&junction.0.join("selected"), &mut |_| {
                visited = true;
                VisitControl::Continue
            })
            .expect_err("an ancestor junction cannot become a scan root implicitly");
        assert!(!visited);
        assert_eq!(failure.operation, FsOperation::EnumerateDirectory);
        assert_eq!(failure.kind, FsErrorKind::NotSupported);
        assert!(outside.join("selected/sentinel.bin").is_file());
    }
}
