//! Windows volume discovery and scan-root resolution.
//!
//! Local volume identity comes directly from the stable volume GUID returned by
//! Windows. When a selected path has no volume GUID (notably some UNC shares),
//! the adapter prefers the server/filesystem `FILE_ID_INFO` volume serial and
//! finally uses a documented, deterministic FNV-1a-128 digest of the canonical
//! volume root. It never uses `DefaultHasher` or process-random state.
//!
//! Storage scheduling is conservative. Network and removable drive types are
//! classified directly from `GetDriveTypeW`; a fixed drive is called
//! low-seek-penalty or rotational only after a successful
//! `StorageDeviceSeekPenaltyProperty` query. Unsupported, denied, malformed, or
//! unavailable hardware data remains [`StorageClass::Unknown`]. Optional
//! capacity, label, filesystem, device, and seek queries produce per-volume
//! issues without aborting discovery of other volumes.

use diskpie_core::VolumeKey;
use diskpie_scan::{ScanRoot, StorageClass};
use std::{
    error::Error,
    ffi::{OsString, c_void},
    fmt,
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
        Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, FindFirstVolumeW,
            FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDriveTypeW,
            GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
            GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
            GetVolumePathNamesForVolumeNameW,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_GET_DEVICE_NUMBER,
                IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_NUMBER,
                STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
            },
        },
    },
    core::PCWSTR,
};

const ERROR_INVALID_FUNCTION_CODE: i32 = 1;
const ERROR_FILE_NOT_FOUND_CODE: i32 = 2;
const ERROR_PATH_NOT_FOUND_CODE: i32 = 3;
const ERROR_ACCESS_DENIED_CODE: i32 = 5;
const ERROR_NOT_READY_CODE: i32 = 21;
const ERROR_NO_MORE_FILES_CODE: i32 = 18;
const ERROR_NOT_SUPPORTED_CODE: i32 = 50;
const ERROR_INVALID_PARAMETER_CODE: i32 = 87;
const ERROR_INVALID_NAME_CODE: i32 = 123;
const ERROR_MORE_DATA_CODE: i32 = 234;

const DRIVE_UNKNOWN_VALUE: u32 = 0;
const DRIVE_NO_ROOT_DIR_VALUE: u32 = 1;
const DRIVE_REMOVABLE_VALUE: u32 = 2;
const DRIVE_FIXED_VALUE: u32 = 3;
const DRIVE_REMOTE_VALUE: u32 = 4;
const DRIVE_CDROM_VALUE: u32 = 5;
const DRIVE_RAMDISK_VALUE: u32 = 6;

const WIDE_BACKSLASH: u16 = b'\\' as u16;
const WIDE_SLASH: u16 = b'/' as u16;
const WIDE_COLON: u16 = b':' as u16;
const WIDE_QUESTION: u16 = b'?' as u16;
const VOLUME_ENUM_BUFFER_UNITS: usize = 1_024;
const PATH_BUFFER_UNITS: usize = 32_768;
const INFO_BUFFER_UNITS: usize = 512;
const INITIAL_MULTI_STRING_UNITS: usize = 512;
const MAX_MULTI_STRING_UNITS: usize = 1_048_576;

const FNV_128_OFFSET_BASIS: u128 = 0x6C62_272E_07BB_0142_62B8_2175_6295_C58D;
const FNV_128_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;

/// One Windows operation that can fail independently during discovery.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum VolumeOperation {
    EnumerateVolumes,
    ReadMountPoints,
    ResolveVolumePath,
    ResolveVolumeName,
    OpenVolume,
    ReadVolumeInformation,
    ReadVolumeIdentity,
    ReadCapacity,
    OpenStorageDevice,
    QueryDeviceIdentity,
    QuerySeekPenalty,
}

/// Portable classification of an error from a Windows volume operation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum VolumeErrorKind {
    AccessDenied,
    NotFound,
    NotReady,
    NotSupported,
    InvalidPath,
    InvalidData,
    BufferTooSmall,
    Io,
}

/// Cloneable typed diagnostic retained per volume whenever possible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeError {
    pub path: PathBuf,
    pub operation: VolumeOperation,
    pub kind: VolumeErrorKind,
    pub os_code: Option<i32>,
    pub detail: Option<String>,
}

impl VolumeError {
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        operation: VolumeOperation,
        kind: VolumeErrorKind,
    ) -> Self {
        Self { path: path.into(), operation, kind, os_code: None, detail: None }
    }

    #[must_use]
    pub const fn with_os_code(mut self, os_code: i32) -> Self {
        self.os_code = Some(os_code);
        self
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for VolumeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} failed for {}: {:?}",
            self.operation,
            self.path.display(),
            self.kind
        )?;
        if let Some(code) = self.os_code {
            write!(formatter, " (OS error {code})")?;
        }
        if let Some(detail) = &self.detail {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

impl Error for VolumeError {}

/// Windows drive type returned by `GetDriveTypeW`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WindowsDriveType {
    Unknown,
    InvalidRoot,
    Removable,
    Fixed,
    Network,
    Optical,
    RamDisk,
    Other(u32),
}

impl WindowsDriveType {
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            DRIVE_UNKNOWN_VALUE => Self::Unknown,
            DRIVE_NO_ROOT_DIR_VALUE => Self::InvalidRoot,
            DRIVE_REMOVABLE_VALUE => Self::Removable,
            DRIVE_FIXED_VALUE => Self::Fixed,
            DRIVE_REMOTE_VALUE => Self::Network,
            DRIVE_CDROM_VALUE => Self::Optical,
            DRIVE_RAMDISK_VALUE => Self::RamDisk,
            other => Self::Other(other),
        }
    }
}

/// Provenance of the stable key assigned to a discovered/selected volume.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum VolumeKeySource {
    /// Parsed directly from `\\?\Volume{GUID}\`.
    VolumeGuid,
    /// Server/filesystem volume serial from `FILE_ID_INFO`.
    FileIdVolumeSerial,
    /// Deterministic FNV-1a-128 of the canonical raw-UTF-16 volume root.
    DeterministicVolumePath,
}

/// Capacity values reported by `GetDiskFreeSpaceExW`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeSpace {
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub total_free_bytes: u64,
}

/// Runtime evidence that two volumes reside on the same storage device.
///
/// Device numbers are scheduling hints for the current Windows boot, not
/// persistent volume identities. `partition_number` is retained diagnostically
/// but excluded from [`StorageDeviceIdentity::same_device_as`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StorageDeviceIdentity {
    pub device_type: u32,
    pub device_number: u32,
    pub partition_number: u32,
}

impl StorageDeviceIdentity {
    #[must_use]
    pub const fn same_device_as(self, other: Self) -> bool {
        self.device_type == other.device_type && self.device_number == other.device_number
    }
}

/// One discovered Windows volume. Optional probe failures are retained in
/// `issues` and do not remove the volume from the discovery result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsVolume {
    pub volume_name: PathBuf,
    pub mount_points: Vec<PathBuf>,
    pub volume_key: VolumeKey,
    pub volume_key_source: VolumeKeySource,
    pub drive_type: WindowsDriveType,
    pub storage_class: StorageClass,
    pub seek_penalty: Option<bool>,
    pub device_identity: Option<StorageDeviceIdentity>,
    pub space: Option<VolumeSpace>,
    pub label: Option<OsString>,
    pub filesystem: Option<OsString>,
    pub serial_number: Option<u32>,
    pub filesystem_flags: Option<u32>,
    pub maximum_component_length: Option<u32>,
    pub issues: Vec<VolumeError>,
}

impl WindowsVolume {
    /// Whether Windows supplied evidence that both records share a volume or
    /// underlying device. This is suitable for permit grouping, not hard-link
    /// identity across distinct partitions.
    #[must_use]
    pub fn shares_permit_device_with(&self, other: &Self) -> bool {
        self.volume_key == other.volume_key
            || self
                .device_identity
                .zip(other.device_identity)
                .is_some_and(|(left, right)| left.same_device_as(right))
    }
}

/// Partial discovery result; a global enumeration error or an inaccessible
/// volume never discards records already discovered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VolumeDiscovery {
    pub volumes: Vec<WindowsVolume>,
    pub errors: Vec<VolumeError>,
}

/// Rich result for a user-selected absolute path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedScanRoot {
    pub scan_root: ScanRoot,
    pub volume_path: PathBuf,
    pub volume_name: Option<PathBuf>,
    pub volume_key_source: VolumeKeySource,
    pub drive_type: WindowsDriveType,
    pub seek_penalty: Option<bool>,
    pub device_identity: Option<StorageDeviceIdentity>,
    pub issues: Vec<VolumeError>,
}

/// Enumerates every volume Windows exposes to the current standard user.
///
/// Enumeration is read-only. Errors from one volume are attached to that
/// volume; an error advancing the enumeration is returned in `errors` together
/// with all records collected before it.
#[must_use]
pub fn discover_volumes() -> VolumeDiscovery {
    let mut result = VolumeDiscovery::default();
    let mut buffer = vec![0_u16; VOLUME_ENUM_BUFFER_UNITS];

    // SAFETY: `buffer` is a valid writable UTF-16 slice. The returned owned
    // enumeration handle is placed under RAII immediately.
    let handle = match unsafe { FindFirstVolumeW(&mut buffer) } {
        Ok(handle) => VolumeFindHandle(handle),
        Err(error) => {
            result.errors.push(volume_error_from_windows(
                Path::new("<Windows volumes>"),
                VolumeOperation::EnumerateVolumes,
                error,
            ));
            return result;
        }
    };

    loop {
        let name_units = nul_terminated_units(&buffer);
        if name_units.is_empty() {
            result.errors.push(
                VolumeError::new(
                    "<Windows volumes>",
                    VolumeOperation::EnumerateVolumes,
                    VolumeErrorKind::InvalidData,
                )
                .with_detail("FindFirst/FindNextVolumeW returned an empty volume name"),
            );
        } else {
            result.volumes.push(probe_discovered_volume(name_units));
        }

        buffer.fill(0);
        // SAFETY: `handle` owns a live volume-enumeration handle and `buffer`
        // is a valid writable UTF-16 slice for the complete call.
        match unsafe { FindNextVolumeW(handle.raw(), &mut buffer) } {
            Ok(()) => {}
            Err(error) if win32_code(&error) == ERROR_NO_MORE_FILES_CODE => break,
            Err(error) => {
                result.errors.push(volume_error_from_windows(
                    Path::new("<Windows volumes>"),
                    VolumeOperation::EnumerateVolumes,
                    error,
                ));
                break;
            }
        }
    }

    result.volumes.sort_by(|left, right| left.volume_name.cmp(&right.volume_name));
    result
}

/// Resolves a user-selected absolute DOS/UNC directory into the portable scan
/// protocol while retaining optional diagnostics and device evidence.
pub fn resolve_scan_root(path: &Path) -> Result<ResolvedScanRoot, VolumeError> {
    let extended = ExtendedPath::from_absolute_path(path)
        .map_err(|error| path_conversion_error(path, VolumeOperation::ResolveVolumePath, error))?;
    let scan_path = extended.to_path_buf();
    let volume_path = query_volume_path(&extended)?;
    let drive_type = query_drive_type(&volume_path);
    let mut issues = Vec::new();

    let volume_name = match query_volume_name(&volume_path) {
        Ok(name) => Some(name),
        Err(error) => {
            issues.push(error);
            None
        }
    };

    let (volume_key, volume_key_source) = if let Some(name) = &volume_name {
        match parse_volume_guid(name.units()) {
            Some(key) => (VolumeKey::new(key), VolumeKeySource::VolumeGuid),
            None => {
                issues.push(
                    VolumeError::new(
                        name.to_path_buf(),
                        VolumeOperation::ReadVolumeIdentity,
                        VolumeErrorKind::InvalidData,
                    )
                    .with_detail("canonical volume name did not contain a valid volume GUID"),
                );
                (
                    VolumeKey::new(deterministic_volume_path_key(name.units())),
                    VolumeKeySource::DeterministicVolumePath,
                )
            }
        }
    } else {
        resolve_non_guid_volume_key(&volume_path, &mut issues)
    };

    let mut seek_penalty = None;
    let mut device_identity = None;
    if let Some(name) = &volume_name {
        let probe = probe_storage_device(name, drive_type == WindowsDriveType::Fixed);
        device_identity = probe.device_identity;
        seek_penalty = probe.seek_penalty;
        issues.extend(probe.issues);
    }
    let storage_class = classify_storage(drive_type, seek_penalty);

    Ok(ResolvedScanRoot {
        scan_root: ScanRoot::new(scan_path, volume_key, storage_class),
        volume_path: volume_path.to_path_buf(),
        volume_name: volume_name.map(|name| name.to_path_buf()),
        volume_key_source,
        drive_type,
        seek_penalty,
        device_identity,
        issues,
    })
}

/// Convenience conversion when the caller needs only the scan protocol value.
pub fn scan_root_from_path(path: &Path) -> Result<ScanRoot, VolumeError> {
    resolve_scan_root(path).map(|resolved| resolved.scan_root)
}

fn probe_discovered_volume(name_units: &[u16]) -> WindowsVolume {
    let volume_name = ExtendedPath::from_existing_extended(name_units.to_vec());
    let volume_path_buf = volume_name.to_path_buf();
    let mut issues = Vec::new();

    let (volume_key, volume_key_source) = match parse_volume_guid(name_units) {
        Some(key) => (VolumeKey::new(key), VolumeKeySource::VolumeGuid),
        None => {
            issues.push(
                VolumeError::new(
                    &volume_path_buf,
                    VolumeOperation::ReadVolumeIdentity,
                    VolumeErrorKind::InvalidData,
                )
                .with_detail("enumerated volume name did not contain a valid GUID"),
            );
            (
                VolumeKey::new(deterministic_volume_path_key(name_units)),
                VolumeKeySource::DeterministicVolumePath,
            )
        }
    };

    let mut mount_points = match query_mount_points(&volume_name) {
        Ok(points) => points,
        Err(error) => {
            issues.push(error);
            Vec::new()
        }
    };
    mount_points.sort();

    let drive_path = mount_points
        .first()
        .and_then(|path| ExtendedPath::from_absolute_path(path).ok())
        .unwrap_or_else(|| volume_name.clone());
    let drive_type = query_drive_type(&drive_path);

    let mut label = None;
    let mut filesystem = None;
    let mut serial_number = None;
    let mut filesystem_flags = None;
    let mut maximum_component_length = None;
    match open_directory_metadata(&volume_name) {
        Ok(handle) => match query_volume_information(&handle) {
            Ok(info) => {
                label = Some(info.label);
                filesystem = Some(info.filesystem);
                serial_number = Some(info.serial_number);
                filesystem_flags = Some(info.filesystem_flags);
                maximum_component_length = Some(info.maximum_component_length);
            }
            Err(error) => issues.push(error.with_path(&volume_path_buf)),
        },
        Err(error) => {
            issues.push(volume_error_from_io(&volume_path_buf, VolumeOperation::OpenVolume, error))
        }
    }

    let space = match query_space(&volume_name) {
        Ok(space) => Some(space),
        Err(error) => {
            issues.push(error);
            None
        }
    };

    let device_probe = probe_storage_device(&volume_name, drive_type == WindowsDriveType::Fixed);
    issues.extend(device_probe.issues);
    let storage_class = classify_storage(drive_type, device_probe.seek_penalty);

    WindowsVolume {
        volume_name: volume_path_buf,
        mount_points,
        volume_key,
        volume_key_source,
        drive_type,
        storage_class,
        seek_penalty: device_probe.seek_penalty,
        device_identity: device_probe.device_identity,
        space,
        label,
        filesystem,
        serial_number,
        filesystem_flags,
        maximum_component_length,
        issues,
    }
}

impl VolumeError {
    fn with_path(mut self, path: &Path) -> Self {
        self.path = path.to_path_buf();
        self
    }
}

#[derive(Debug)]
struct VolumeFindHandle(HANDLE);

impl VolumeFindHandle {
    const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for VolumeFindHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one handle returned by a successful
        // `FindFirstVolumeW` and closes it exactly once.
        let _ = unsafe { FindVolumeClose(self.0) };
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtendedPath {
    units: Vec<u16>,
}

impl ExtendedPath {
    fn from_absolute_path(path: &Path) -> Result<Self, PathConversionError> {
        if !path.is_absolute() {
            return Err(PathConversionError::Relative);
        }
        let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        prefix_extended_absolute(&units).map(|units| Self { units })
    }

    const fn from_existing_extended(units: Vec<u16>) -> Self {
        Self { units }
    }

    fn from_api_path(units: &[u16]) -> Result<Self, PathConversionError> {
        prefix_extended_absolute(units).map(|units| Self { units })
    }

    fn units(&self) -> &[u16] {
        &self.units
    }

    fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(OsString::from_wide(&self.units))
    }

    fn as_wide_c_string(&self) -> WideCString {
        WideCString::new(self.units.clone())
    }

    fn volume_device_path(&self) -> PathBuf {
        let mut units = self.units.clone();
        if units.last().copied() == Some(WIDE_BACKSLASH) {
            units.pop();
        }
        PathBuf::from(OsString::from_wide(&units))
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
    Relative,
    EmbeddedNul,
    UnsupportedNamespace,
    InvalidDosOrUnc,
}

fn prefix_extended_absolute(units: &[u16]) -> Result<Vec<u16>, PathConversionError> {
    if units.contains(&0) {
        return Err(PathConversionError::EmbeddedNul);
    }
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
    operation: VolumeOperation,
    error: PathConversionError,
) -> VolumeError {
    match error {
        PathConversionError::Relative => {
            VolumeError::new(path, operation, VolumeErrorKind::InvalidPath)
                .with_detail("selected scan roots must be absolute DOS or UNC paths")
        }
        PathConversionError::EmbeddedNul => {
            VolumeError::new(path, operation, VolumeErrorKind::InvalidPath)
                .with_detail("path contains a NUL UTF-16 code unit")
        }
        PathConversionError::UnsupportedNamespace => {
            VolumeError::new(path, operation, VolumeErrorKind::InvalidPath)
                .with_detail("device and non-DOS/non-UNC verbatim namespaces are not scan roots")
        }
        PathConversionError::InvalidDosOrUnc => {
            VolumeError::new(path, operation, VolumeErrorKind::InvalidPath)
                .with_detail("path is not an absolute DOS or UNC filesystem path")
        }
    }
}

fn query_volume_path(path: &ExtendedPath) -> Result<ExtendedPath, VolumeError> {
    let wide = path.as_wide_c_string();
    let mut buffer = vec![0_u16; PATH_BUFFER_UNITS];
    // SAFETY: input is NUL-terminated and `buffer` is a writable UTF-16 slice
    // whose length is within `u32`. No pointer escapes the synchronous call.
    unsafe { GetVolumePathNameW(wide.as_pcwstr(), &mut buffer) }.map_err(|error| {
        volume_error_from_windows(&path.to_path_buf(), VolumeOperation::ResolveVolumePath, error)
    })?;
    let units = nul_terminated_units(&buffer);
    ExtendedPath::from_api_path(units).map_err(|error| {
        path_conversion_error(&path.to_path_buf(), VolumeOperation::ResolveVolumePath, error)
    })
}

fn query_volume_name(volume_path: &ExtendedPath) -> Result<ExtendedPath, VolumeError> {
    let input = volume_path.as_wide_c_string();
    let mut buffer = vec![0_u16; INFO_BUFFER_UNITS];
    // SAFETY: input is a NUL-terminated volume mount path and `buffer` is a
    // valid writable UTF-16 slice for the duration of the call.
    unsafe { GetVolumeNameForVolumeMountPointW(input.as_pcwstr(), &mut buffer) }.map_err(
        |error| {
            volume_error_from_windows(
                &volume_path.to_path_buf(),
                VolumeOperation::ResolveVolumeName,
                error,
            )
        },
    )?;
    let units = nul_terminated_units(&buffer);
    if units.is_empty() {
        return Err(VolumeError::new(
            volume_path.to_path_buf(),
            VolumeOperation::ResolveVolumeName,
            VolumeErrorKind::InvalidData,
        )
        .with_detail("Windows returned an empty canonical volume name"));
    }
    Ok(ExtendedPath::from_existing_extended(units.to_vec()))
}

fn query_mount_points(volume_name: &ExtendedPath) -> Result<Vec<PathBuf>, VolumeError> {
    let input = volume_name.as_wide_c_string();
    let mut buffer = vec![0_u16; INITIAL_MULTI_STRING_UNITS];
    loop {
        let mut required = 0_u32;
        // SAFETY: input is a NUL-terminated canonical volume name; `buffer` and
        // `required` are valid writable outputs and do not escape the call.
        let result = unsafe {
            GetVolumePathNamesForVolumeNameW(
                input.as_pcwstr(),
                Some(&mut buffer),
                &raw mut required,
            )
        };
        match result {
            Ok(()) => return Ok(parse_multi_string(&buffer)),
            Err(error) if win32_code(&error) == ERROR_MORE_DATA_CODE => {
                let requested = usize::try_from(required).map_err(|_| {
                    VolumeError::new(
                        volume_name.to_path_buf(),
                        VolumeOperation::ReadMountPoints,
                        VolumeErrorKind::BufferTooSmall,
                    )
                    .with_detail("mount-point buffer length does not fit usize")
                })?;
                if requested <= buffer.len() || requested > MAX_MULTI_STRING_UNITS {
                    return Err(VolumeError::new(
                        volume_name.to_path_buf(),
                        VolumeOperation::ReadMountPoints,
                        VolumeErrorKind::InvalidData,
                    )
                    .with_os_code(ERROR_MORE_DATA_CODE)
                    .with_detail("Windows returned an invalid mount-point buffer length"));
                }
                buffer.resize(requested, 0);
            }
            Err(error) => {
                return Err(volume_error_from_windows(
                    &volume_name.to_path_buf(),
                    VolumeOperation::ReadMountPoints,
                    error,
                ));
            }
        }
    }
}

fn parse_multi_string(buffer: &[u16]) -> Vec<PathBuf> {
    let mut values = Vec::new();
    let mut start = 0;
    while start < buffer.len() {
        let Some(relative_end) = buffer[start..].iter().position(|&unit| unit == 0) else {
            break;
        };
        if relative_end == 0 {
            break;
        }
        let end = start + relative_end;
        values.push(PathBuf::from(OsString::from_wide(&buffer[start..end])));
        start = end + 1;
    }
    values
}

fn query_drive_type(path: &ExtendedPath) -> WindowsDriveType {
    let wide = path.as_wide_c_string();
    // SAFETY: `wide` is a live NUL-terminated root path. The function has no
    // output pointer or ownership transfer.
    WindowsDriveType::from_raw(unsafe { GetDriveTypeW(wide.as_pcwstr()) })
}

fn open_directory_metadata(path: &ExtendedPath) -> io::Result<File> {
    let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
    options.open(path.to_path_buf())
}

fn open_storage_device(path: &ExtendedPath) -> io::Result<File> {
    let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    let mut options = OpenOptions::new();
    options.access_mode(0).share_mode(share);
    options.open(path.volume_device_path())
}

struct RawVolumeInformation {
    label: OsString,
    filesystem: OsString,
    serial_number: u32,
    filesystem_flags: u32,
    maximum_component_length: u32,
}

fn query_volume_information(file: &File) -> Result<RawVolumeInformation, VolumeError> {
    let mut label = vec![0_u16; INFO_BUFFER_UNITS];
    let mut filesystem = vec![0_u16; INFO_BUFFER_UNITS];
    let mut serial_number = 0;
    let mut filesystem_flags = 0;
    let mut maximum_component_length = 0;
    // SAFETY: the `File` owns a live handle; every optional output points to a
    // correctly sized writable value/slice for this synchronous call.
    unsafe {
        GetVolumeInformationByHandleW(
            HANDLE(file.as_raw_handle()),
            Some(&mut label),
            Some(&raw mut serial_number),
            Some(&raw mut maximum_component_length),
            Some(&raw mut filesystem_flags),
            Some(&mut filesystem),
        )
    }
    .map_err(|error| {
        volume_error_from_windows(
            Path::new("<open volume handle>"),
            VolumeOperation::ReadVolumeInformation,
            error,
        )
    })?;

    Ok(RawVolumeInformation {
        label: OsString::from_wide(nul_terminated_units(&label)),
        filesystem: OsString::from_wide(nul_terminated_units(&filesystem)),
        serial_number,
        filesystem_flags,
        maximum_component_length,
    })
}

fn query_file_id_info(file: &File, path: &Path) -> Result<FILE_ID_INFO, VolumeError> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the `File` owns a live handle and `info` is a correctly sized
    // writable `FILE_ID_INFO` buffer confined to this synchronous call.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|error| volume_error_from_windows(path, VolumeOperation::ReadVolumeIdentity, error))?;
    Ok(info)
}

fn resolve_non_guid_volume_key(
    volume_path: &ExtendedPath,
    issues: &mut Vec<VolumeError>,
) -> (VolumeKey, VolumeKeySource) {
    match open_directory_metadata(volume_path) {
        Ok(file) => match query_file_id_info(&file, &volume_path.to_path_buf()) {
            Ok(info) => (
                VolumeKey::new(u128::from(info.VolumeSerialNumber)),
                VolumeKeySource::FileIdVolumeSerial,
            ),
            Err(error) => {
                issues.push(error);
                (
                    VolumeKey::new(deterministic_volume_path_key(volume_path.units())),
                    VolumeKeySource::DeterministicVolumePath,
                )
            }
        },
        Err(error) => {
            issues.push(volume_error_from_io(
                &volume_path.to_path_buf(),
                VolumeOperation::OpenVolume,
                error,
            ));
            (
                VolumeKey::new(deterministic_volume_path_key(volume_path.units())),
                VolumeKeySource::DeterministicVolumePath,
            )
        }
    }
}

fn query_space(volume_name: &ExtendedPath) -> Result<VolumeSpace, VolumeError> {
    let wide = volume_name.as_wide_c_string();
    let mut available_bytes = 0;
    let mut total_bytes = 0;
    let mut total_free_bytes = 0;
    // SAFETY: `wide` is NUL-terminated and all three output pointers are valid
    // writable `u64` values for the synchronous call.
    unsafe {
        GetDiskFreeSpaceExW(
            wide.as_pcwstr(),
            Some(&raw mut available_bytes),
            Some(&raw mut total_bytes),
            Some(&raw mut total_free_bytes),
        )
    }
    .map_err(|error| {
        volume_error_from_windows(&volume_name.to_path_buf(), VolumeOperation::ReadCapacity, error)
    })?;
    Ok(VolumeSpace { available_bytes, total_bytes, total_free_bytes })
}

#[derive(Default)]
struct DeviceProbe {
    device_identity: Option<StorageDeviceIdentity>,
    seek_penalty: Option<bool>,
    issues: Vec<VolumeError>,
}

fn probe_storage_device(volume_name: &ExtendedPath, query_seek: bool) -> DeviceProbe {
    let mut result = DeviceProbe::default();
    let path = volume_name.to_path_buf();
    let file = match open_storage_device(volume_name) {
        Ok(file) => file,
        Err(error) => {
            result.issues.push(volume_error_from_io(
                &path,
                VolumeOperation::OpenStorageDevice,
                error,
            ));
            return result;
        }
    };

    match query_device_identity(&file, &path) {
        Ok(identity) => result.device_identity = Some(identity),
        Err(error) => result.issues.push(error),
    }
    if query_seek {
        match query_seek_penalty(&file, &path) {
            Ok(penalty) => result.seek_penalty = Some(penalty),
            Err(error) => result.issues.push(error),
        }
    }
    result
}

fn query_device_identity(file: &File, path: &Path) -> Result<StorageDeviceIdentity, VolumeError> {
    let mut raw = STORAGE_DEVICE_NUMBER::default();
    let mut returned = 0_u32;
    // SAFETY: the handle remains live; no input is required; `raw` and
    // `returned` are correctly sized writable outputs and no pointer escapes.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some((&raw mut raw).cast::<c_void>()),
            size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(|error| {
        volume_error_from_windows(path, VolumeOperation::QueryDeviceIdentity, error)
    })?;

    if usize::try_from(returned).ok() != Some(size_of::<STORAGE_DEVICE_NUMBER>()) {
        return Err(VolumeError::new(
            path,
            VolumeOperation::QueryDeviceIdentity,
            VolumeErrorKind::InvalidData,
        )
        .with_detail("IOCTL_STORAGE_GET_DEVICE_NUMBER returned an unexpected byte count"));
    }
    Ok(StorageDeviceIdentity {
        device_type: raw.DeviceType,
        device_number: raw.DeviceNumber,
        partition_number: raw.PartitionNumber,
    })
}

fn query_seek_penalty(file: &File, path: &Path) -> Result<bool, VolumeError> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0_u32;
    // SAFETY: `query` is a correctly initialized input record; `descriptor` and
    // `returned` are valid writable outputs, the file handle remains live, and
    // all pointers are confined to the synchronous call.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast::<c_void>()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some((&raw mut descriptor).cast::<c_void>()),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(|error| volume_error_from_windows(path, VolumeOperation::QuerySeekPenalty, error))?;

    let expected = size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>();
    if !usize::try_from(returned).is_ok_and(|returned| returned >= expected)
        || !usize::try_from(descriptor.Size).is_ok_and(|size| size >= expected)
        || !usize::try_from(descriptor.Version).is_ok_and(|version| version >= expected)
    {
        return Err(VolumeError::new(
            path,
            VolumeOperation::QuerySeekPenalty,
            VolumeErrorKind::InvalidData,
        )
        .with_detail("seek-penalty descriptor was truncated or had an invalid header"));
    }
    Ok(descriptor.IncursSeekPenalty)
}

const fn classify_storage(
    drive_type: WindowsDriveType,
    seek_penalty: Option<bool>,
) -> StorageClass {
    match drive_type {
        WindowsDriveType::Network => StorageClass::Network,
        WindowsDriveType::Removable | WindowsDriveType::Optical => StorageClass::Removable,
        WindowsDriveType::RamDisk => StorageClass::LocalLowSeekPenalty,
        WindowsDriveType::Fixed => match seek_penalty {
            Some(false) => StorageClass::LocalLowSeekPenalty,
            Some(true) => StorageClass::Rotational,
            None => StorageClass::Unknown,
        },
        WindowsDriveType::Unknown | WindowsDriveType::InvalidRoot | WindowsDriveType::Other(_) => {
            StorageClass::Unknown
        }
    }
}

fn parse_volume_guid(units: &[u16]) -> Option<u128> {
    const PREFIX: [u16; 11] = [
        WIDE_BACKSLASH,
        WIDE_BACKSLASH,
        WIDE_QUESTION,
        WIDE_BACKSLASH,
        b'V' as u16,
        b'o' as u16,
        b'l' as u16,
        b'u' as u16,
        b'm' as u16,
        b'e' as u16,
        b'{' as u16,
    ];
    if !starts_with_ascii_case_insensitive(units, &PREFIX)
        || units.len() != PREFIX.len() + 38
        || units[units.len() - 2] != b'}' as u16
        || units[units.len() - 1] != WIDE_BACKSLASH
    {
        return None;
    }

    let mut value = 0_u128;
    let mut digits = 0_u8;
    for &unit in &units[PREFIX.len()..units.len() - 2] {
        if unit == b'-' as u16 {
            continue;
        }
        let digit = hex_digit(unit)?;
        value = value.checked_mul(16)?.checked_add(u128::from(digit))?;
        digits = digits.checked_add(1)?;
    }
    (digits == 32).then_some(value)
}

const fn hex_digit(unit: u16) -> Option<u8> {
    match unit {
        value if value >= b'0' as u16 && value <= b'9' as u16 => Some((value - b'0' as u16) as u8),
        value if value >= b'a' as u16 && value <= b'f' as u16 => {
            Some((value - b'a' as u16 + 10) as u8)
        }
        value if value >= b'A' as u16 && value <= b'F' as u16 => {
            Some((value - b'A' as u16 + 10) as u8)
        }
        _ => None,
    }
}

fn deterministic_volume_path_key(units: &[u16]) -> u128 {
    let mut hash = FNV_128_OFFSET_BASIS;
    for byte in b"DiskPie.Windows.VolumePath.v1\0" {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV_128_PRIME);
    }
    for &unit in units {
        // Namespace/path prefixes and ASCII server/share spelling are
        // case-insensitive on Windows. Non-ASCII code units remain lossless.
        let normalized = ascii_upper(unit);
        for byte in normalized.to_le_bytes() {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(FNV_128_PRIME);
        }
    }
    hash
}

fn nul_terminated_units(buffer: &[u16]) -> &[u16] {
    let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    &buffer[..length]
}

fn volume_error_from_io(path: &Path, operation: VolumeOperation, error: io::Error) -> VolumeError {
    let code = error.raw_os_error();
    let mut result =
        VolumeError::new(path, operation, volume_error_kind(code)).with_detail(error.to_string());
    if let Some(code) = code {
        result = result.with_os_code(code);
    }
    result
}

fn volume_error_from_windows(
    path: &Path,
    operation: VolumeOperation,
    error: windows::core::Error,
) -> VolumeError {
    let code = win32_code(&error);
    VolumeError::new(path, operation, volume_error_kind(Some(code)))
        .with_os_code(code)
        .with_detail(error.to_string())
}

fn win32_code(error: &windows::core::Error) -> i32 {
    let hresult = error.code().0;
    let raw = hresult as u32;
    if raw & 0xFFFF_0000 == 0x8007_0000 { (raw & 0xFFFF) as i32 } else { hresult }
}

const fn volume_error_kind(code: Option<i32>) -> VolumeErrorKind {
    match code {
        Some(ERROR_ACCESS_DENIED_CODE) => VolumeErrorKind::AccessDenied,
        Some(ERROR_FILE_NOT_FOUND_CODE | ERROR_PATH_NOT_FOUND_CODE) => VolumeErrorKind::NotFound,
        Some(ERROR_NOT_READY_CODE) => VolumeErrorKind::NotReady,
        Some(
            ERROR_INVALID_FUNCTION_CODE | ERROR_NOT_SUPPORTED_CODE | ERROR_INVALID_PARAMETER_CODE,
        ) => VolumeErrorKind::NotSupported,
        Some(ERROR_INVALID_NAME_CODE) => VolumeErrorKind::InvalidPath,
        Some(ERROR_MORE_DATA_CODE) => VolumeErrorKind::BufferTooSmall,
        _ => VolumeErrorKind::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn encode(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().collect()
    }

    fn decode(value: &[u16]) -> OsString {
        OsString::from_wide(value)
    }

    #[test]
    fn extended_path_conversion_accepts_only_absolute_dos_and_unc() {
        let dos =
            prefix_extended_absolute(&encode(r"C:\data\..\chosen")).expect("absolute DOS path");
        assert_eq!(decode(&dos), OsString::from(r"\\?\C:\data\..\chosen"));

        let unc =
            prefix_extended_absolute(&encode(r"\\server\share\folder")).expect("absolute UNC path");
        assert_eq!(decode(&unc), OsString::from(r"\\?\UNC\server\share\folder"));

        assert!(matches!(
            prefix_extended_absolute(&encode(r"\\.\PhysicalDrive0")),
            Err(PathConversionError::UnsupportedNamespace)
        ));
        assert!(matches!(
            ExtendedPath::from_absolute_path(Path::new("relative")),
            Err(PathConversionError::Relative)
        ));
    }

    #[test]
    fn volume_guid_is_parsed_directly_without_hashing() {
        let volume = encode(r"\\?\Volume{00112233-4455-6677-8899-aabbccddeeff}\");
        assert_eq!(parse_volume_guid(&volume), Some(0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF));
        let lowercase_prefix = encode(r"\\?\volume{00112233-4455-6677-8899-AABBCCDDEEFF}\");
        assert_eq!(parse_volume_guid(&lowercase_prefix), parse_volume_guid(&volume));
        assert_eq!(parse_volume_guid(&encode(r"\\?\Volume{not-a-guid}\")), None);
    }

    #[test]
    fn deterministic_fallback_key_is_stable_case_insensitive_and_lossless() {
        let first = deterministic_volume_path_key(&encode(r"\\?\UNC\Server\Share\"));
        let same = deterministic_volume_path_key(&encode(r"\\?\unc\server\share\"));
        let other = deterministic_volume_path_key(&encode(r"\\?\UNC\Server\Other\"));
        assert_eq!(first, same);
        assert_ne!(first, other);

        let mut raw = encode(r"\\?\UNC\server\share\");
        raw.push(0xD800);
        assert_ne!(deterministic_volume_path_key(&raw), first);
    }

    #[test]
    fn parses_multisz_mount_points_without_lossy_conversion() {
        let first = encode(r"C:\");
        let mut second = encode(r"C:\mount-");
        second.push(0xD800);
        second.push(WIDE_BACKSLASH);
        let mut multi = first.clone();
        multi.push(0);
        multi.extend_from_slice(&second);
        multi.extend_from_slice(&[0, 0]);

        let parsed = parse_multi_string(&multi);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].as_os_str(), OsString::from_wide(&first));
        assert_eq!(parsed[1].as_os_str(), OsString::from_wide(&second));
    }

    #[test]
    fn storage_classification_never_guesses_fixed_media() {
        assert_eq!(classify_storage(WindowsDriveType::Fixed, None), StorageClass::Unknown);
        assert_eq!(
            classify_storage(WindowsDriveType::Fixed, Some(false)),
            StorageClass::LocalLowSeekPenalty
        );
        assert_eq!(classify_storage(WindowsDriveType::Fixed, Some(true)), StorageClass::Rotational);
        assert_eq!(classify_storage(WindowsDriveType::Network, None), StorageClass::Network);
        assert_eq!(
            classify_storage(WindowsDriveType::Removable, Some(false)),
            StorageClass::Removable
        );
    }

    #[test]
    fn device_identity_groups_partitions_only_with_explicit_device_evidence() {
        let first = StorageDeviceIdentity { device_type: 7, device_number: 3, partition_number: 1 };
        let second =
            StorageDeviceIdentity { device_type: 7, device_number: 3, partition_number: 2 };
        let other = StorageDeviceIdentity { device_type: 7, device_number: 4, partition_number: 1 };
        assert!(first.same_device_as(second));
        assert!(!first.same_device_as(other));
    }

    #[test]
    fn resolves_current_absolute_path_read_only_and_stably() {
        let current = std::env::current_dir().expect("current directory");
        let first = resolve_scan_root(&current).expect("resolve current path");
        let second = resolve_scan_root(&current).expect("resolve current path again");
        assert!(first.scan_root.path.is_absolute());
        assert_eq!(first.scan_root.volume, second.scan_root.volume);
        assert_eq!(first.scan_root.storage_class, second.scan_root.storage_class);
        if first.drive_type == WindowsDriveType::Fixed && first.seek_penalty.is_none() {
            assert_eq!(first.scan_root.storage_class, StorageClass::Unknown);
        }
        assert_eq!(scan_root_from_path(&current).expect("convenience root"), first.scan_root);
    }

    #[test]
    fn discovery_is_read_only_partial_and_contains_stable_volume_records() {
        let discovery = discover_volumes();
        assert!(
            !discovery.volumes.is_empty(),
            "Windows should expose at least the system volume: {:?}",
            discovery.errors
        );
        for volume in &discovery.volumes {
            assert!(volume.volume_name.is_absolute());
            if volume.drive_type == WindowsDriveType::Fixed && volume.seek_penalty.is_none() {
                assert_eq!(volume.storage_class, StorageClass::Unknown);
            }
            if let Some(space) = volume.space {
                assert!(space.total_free_bytes <= space.total_bytes);
                assert!(space.available_bytes <= space.total_bytes);
            }
        }
    }
}
