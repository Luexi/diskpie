//! Real-NTFS evidence that the Windows adapter, the bounded scanner, the
//! session reducer, and the sunburst layout satisfy the filesystem contract
//! in ADR 0002 (sizes and hard links), ADR 0003 (reparse boundaries), and
//! ADR 0004 (bounded scheduling and cancellation).
//!
//! Every fixture is created inside a directory this file creates under the
//! temporary directory and removes on drop. No test touches user data,
//! repository files, or anything it did not create. A test skips, printing
//! the reason, only when the environment refuses the fixture itself (for
//! example a denied symbolic-link privilege or a volume without compression
//! support); it never skips for convenience.

#![cfg(windows)]

use diskpie_app::session::{ApplyOutcome, SessionPhase, SessionReducer};
use diskpie_core::{
    EntryKind, HardLinkStatus, MetricSource, NodeId, ReparseKind, ScanState, TreeSnapshot,
    sunburst::{HiddenBranches, LayoutOptions, SectorKind, SunburstLayout, compute_layout},
};
use diskpie_platform::{WindowsFileSystem, resolve_scan_root, scan_root_from_path};
use diskpie_scan::{
    CancelToken, DirectoryCompletion, DirectoryItem, DirectoryState, FsErrorKind, FsOperation,
    PermitCaps, ScanEvent, ScanFs, ScanGeneration, ScanNodeId, ScanOmission, ScanOptions,
    ScanRequest, ScanTerminal, ScannedEntry, StartedRoot, TerminalState, VisitControl,
    start_scan_with_cancel,
};
use std::{
    ffi::{OsStr, OsString, c_void},
    fs::{self, File, OpenOptions},
    io,
    mem::size_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{OpenOptionsExt, symlink_dir},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Security::{
            ACCESS_DENIED_ACE, ACL, ACL_REVISION, AddAccessDeniedAce, DACL_SECURITY_INFORMATION,
            GetFileSecurityW, GetLengthSid, GetTokenInformation, InitializeAcl,
            InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR,
            SetFileSecurityW, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            COMPRESSION_FORMAT_DEFAULT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_LIST_DIRECTORY, GetCompressedFileSizeW, GetVolumeInformationW, INVALID_FILE_SIZE,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{FSCTL_SET_COMPRESSION, FSCTL_SET_REPARSE_POINT, FSCTL_SET_SPARSE},
            Threading::{GetCurrentProcess, GetProcessHandleCount, OpenProcessToken},
        },
    },
    core::PCWSTR,
};

const ERROR_ACCESS_DENIED_CODE: i32 = 5;
const ERROR_INSUFFICIENT_BUFFER_CODE: i32 = 122;
const ERROR_PRIVILEGE_NOT_HELD_CODE: i32 = 1_314;
/// `IO_REPARSE_TAG_MOUNT_POINT`, the tag Windows uses for junctions.
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `SECURITY_DESCRIPTOR_REVISION` from `winnt.h`.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
/// `FILE_FILE_COMPRESSION` volume capability flag from `winnt.h`.
const FILE_FILE_COMPRESSION: u32 = 0x10;
/// `FILE_SUPPORTS_SPARSE_FILES` volume capability flag from `winnt.h`.
const FILE_SUPPORTS_SPARSE_FILES: u32 = 0x40;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// Upper bound on one fixture scan; a loop that is followed would exceed it.
const SCAN_TIME_BOUND: Duration = Duration::from_secs(10);
/// Hard deadline for collecting events before the test itself fails.
const COLLECT_DEADLINE: Duration = Duration::from_secs(30);

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Fixture infrastructure
// ---------------------------------------------------------------------------

/// Isolated temporary root removed on drop. Only this test process creates
/// content below it.
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("diskpie-scan-fixture-{label}-{}-{sequence}", std::process::id()));
        fs::create_dir(&path).expect("create isolated fixture directory");
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

fn skip(reason: &str) {
    eprintln!("fixture unavailable in this environment; test skipped: {reason}");
}

fn wide_nul(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().collect()
}

fn win32_code(error: &windows::core::Error) -> i32 {
    let hresult = error.code().0;
    let raw = hresult as u32;
    if raw & 0xFFFF_0000 == 0x8007_0000 { (raw & 0xFFFF) as i32 } else { hresult }
}

fn windows_to_io(error: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(win32_code(&error))
}

/// Pointer-aligned byte buffer for self-relative security structures.
struct AlignedBuffer(Vec<u64>);

impl AlignedBuffer {
    fn new(bytes: usize) -> Self {
        Self(vec![0; bytes.div_ceil(size_of::<u64>()).max(1)])
    }

    fn len_bytes(&self) -> u32 {
        u32::try_from(self.0.len() * size_of::<u64>()).expect("buffer length fits in u32")
    }

    fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr().cast()
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }
}

// ---------------------------------------------------------------------------
// Scan harness
// ---------------------------------------------------------------------------

fn scan_options() -> ScanOptions {
    ScanOptions {
        worker_count: 2,
        batch_entries: 64,
        result_capacity: 8,
        event_capacity: 8,
        flush_interval: Duration::from_millis(10),
        backpressure_park: Duration::from_millis(1),
        coordinator_poll: Duration::from_millis(2),
        permit_caps: PermitCaps::default(),
    }
}

struct ScanRun {
    events: Vec<ScanEvent>,
    terminal: ScanTerminal,
    elapsed: Duration,
}

impl ScanRun {
    fn started_root(&self) -> &StartedRoot {
        self.events
            .iter()
            .find_map(|event| match event {
                ScanEvent::Started { roots, .. } => roots.first(),
                _ => None,
            })
            .expect("scan announced its root")
    }

    fn entries(&self) -> impl Iterator<Item = &ScannedEntry> {
        self.events.iter().flat_map(|event| match event {
            ScanEvent::Batch(batch) => batch.entries.iter(),
            _ => [].iter(),
        })
    }

    fn omissions(&self) -> impl Iterator<Item = &ScanOmission> {
        self.events.iter().flat_map(|event| match event {
            ScanEvent::Batch(batch) => batch.omissions.iter(),
            _ => [].iter(),
        })
    }

    fn completions(&self) -> impl Iterator<Item = &DirectoryCompletion> {
        self.events.iter().filter_map(|event| match event {
            ScanEvent::DirectoryComplete(completion) => Some(completion),
            _ => None,
        })
    }

    fn entry_named(&self, name: &str) -> &ScannedEntry {
        let mut matches = self.entries().filter(|entry| entry.name == OsStr::new(name));
        let entry = matches.next().unwrap_or_else(|| panic!("scan reported an entry named {name}"));
        assert!(matches.next().is_none(), "entry named {name} was reported more than once");
        entry
    }

    fn assert_within_bound(&self) {
        assert!(
            self.elapsed < SCAN_TIME_BOUND,
            "scan took {:?}, which exceeds the {SCAN_TIME_BOUND:?} bound",
            self.elapsed
        );
    }

    /// Feeds every event into the reducer for this generation and freezes the
    /// resulting tree.
    fn reduce(&self) -> (SessionReducer, TreeSnapshot) {
        let mut reducer = SessionReducer::new(self.terminal.generation);
        for event in &self.events {
            assert_eq!(
                reducer.apply_event(event.clone()).expect("reducer accepts a live event"),
                ApplyOutcome::Applied
            );
        }
        let snapshot = reducer.materialize_snapshot().expect("materialize the frozen tree");
        (reducer, snapshot)
    }
}

fn run_scan(root: &Path, generation: u64) -> ScanRun {
    run_scan_with(root, generation, |_, _| {})
}

/// Runs a real scan with the Windows adapter and collects every event.
///
/// `on_event` observes each event as it arrives and may cancel the scan.
fn run_scan_with(
    root: &Path,
    generation: u64,
    mut on_event: impl FnMut(&ScanEvent, &CancelToken),
) -> ScanRun {
    let scan_root = scan_root_from_path(root).expect("resolve the fixture volume");
    let cancel = CancelToken::new();
    let filesystem = WindowsFileSystem::with_cancel_token(cancel.clone());
    let request = ScanRequest::new(ScanGeneration::new(generation), vec![scan_root])
        .with_options(scan_options());
    let started = Instant::now();
    let session = start_scan_with_cancel(std::sync::Arc::new(filesystem), request, cancel.clone())
        .expect("start the scan");

    let deadline = started + COLLECT_DEADLINE;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "scan did not terminate within {COLLECT_DEADLINE:?}");
        let event = session.recv_timeout(remaining).expect("scan event before the deadline");
        on_event(&event, &cancel);
        let terminal = matches!(event, ScanEvent::Terminal(_));
        events.push(event);
        if terminal {
            break;
        }
    }
    let terminal = session.join().expect("every coordinator and worker thread joined");
    let elapsed = started.elapsed();
    assert!(matches!(events.last(), Some(ScanEvent::Terminal(last)) if *last == terminal));
    ScanRun { events, terminal, elapsed }
}

/// Reads one directory level directly through the adapter, without the
/// scanner, to cross-check a fixture's metadata.
fn adapter_entries(directory: &Path) -> Vec<diskpie_scan::FsEntry> {
    let mut entries = Vec::new();
    WindowsFileSystem::new()
        .visit_directory(directory, &mut |item| {
            if let DirectoryItem::Entry(entry) = item {
                entries.push(entry);
            }
            VisitControl::Continue
        })
        .expect("adapter enumerates the fixture directory");
    entries
}

fn adapter_entry(directory: &Path, name: &str) -> diskpie_scan::FsEntry {
    adapter_entries(directory)
        .into_iter()
        .find(|entry| entry.name == OsStr::new(name))
        .unwrap_or_else(|| panic!("adapter reported an entry named {name}"))
}

// ---------------------------------------------------------------------------
// Win32 fixture helpers (junction, sparse, compression, DACL, handle count)
// ---------------------------------------------------------------------------

const REPARSE_PATH_UNITS: usize = 1_024;
/// `ReparseTag`, `ReparseDataLength`, and `Reserved`.
const REPARSE_HEADER_BYTES: usize = 8;
/// The four `u16` offset/length fields of the mount-point payload.
const MOUNT_POINT_HEADER_BYTES: usize = 8;

/// `REPARSE_DATA_BUFFER` with its `MountPointReparseBuffer` member, laid out
/// exactly as `ntifs.h` defines it, with a fixed path area.
#[repr(C)]
struct MountPointReparseData {
    reparse_tag: u32,
    reparse_data_length: u16,
    reserved: u16,
    substitute_name_offset: u16,
    substitute_name_length: u16,
    print_name_offset: u16,
    print_name_length: u16,
    path_buffer: [u16; REPARSE_PATH_UNITS],
}

/// Junction created by this test; removed as a reparse point, never followed.
struct Junction(PathBuf);

impl Drop for Junction {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

fn nt_substitute_name(print_name: &[u16]) -> Vec<u16> {
    let verbatim = wide(OsStr::new(r"\\?\"));
    let nt_prefix = wide(OsStr::new(r"\??\"));
    if print_name.starts_with(&verbatim) {
        nt_prefix.into_iter().chain(print_name[verbatim.len()..].iter().copied()).collect()
    } else {
        nt_prefix.into_iter().chain(print_name.iter().copied()).collect()
    }
}

/// Creates an NTFS junction (`IO_REPARSE_TAG_MOUNT_POINT`) at `link` that
/// points at `target`. Junctions require no privilege.
fn create_junction(link: &Path, target: &Path) -> io::Result<Junction> {
    let target = std::path::absolute(target)?;
    let print_name = wide(target.as_os_str());
    let substitute_name = nt_substitute_name(&print_name);
    let needed_units = substitute_name.len() + 1 + print_name.len() + 1;
    assert!(needed_units <= REPARSE_PATH_UNITS, "junction fixture path is too long");

    let mut data = MountPointReparseData {
        reparse_tag: IO_REPARSE_TAG_MOUNT_POINT,
        reparse_data_length: u16::try_from(MOUNT_POINT_HEADER_BYTES + needed_units * 2)
            .expect("reparse payload fits in u16"),
        reserved: 0,
        substitute_name_offset: 0,
        substitute_name_length: u16::try_from(substitute_name.len() * 2)
            .expect("substitute name fits in u16"),
        print_name_offset: u16::try_from((substitute_name.len() + 1) * 2)
            .expect("print name offset fits in u16"),
        print_name_length: u16::try_from(print_name.len() * 2).expect("print name fits in u16"),
        path_buffer: [0; REPARSE_PATH_UNITS],
    };
    data.path_buffer[..substitute_name.len()].copy_from_slice(&substitute_name);
    let print_start = substitute_name.len() + 1;
    data.path_buffer[print_start..print_start + print_name.len()].copy_from_slice(&print_name);
    let input_bytes = u32::try_from(REPARSE_HEADER_BYTES + usize::from(data.reparse_data_length))
        .expect("reparse buffer fits in u32");

    fs::create_dir(link)?;
    let directory = OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(link)?;
    let mut returned = 0_u32;
    // SAFETY: `directory` keeps the handle alive for this synchronous call,
    // `data` is a live `#[repr(C)]` buffer of at least `input_bytes` bytes, no
    // output buffer is requested, and `returned` is a valid writable `u32`.
    let result = unsafe {
        DeviceIoControl(
            HANDLE(directory.as_raw_handle()),
            FSCTL_SET_REPARSE_POINT,
            Some((&raw const data).cast::<c_void>()),
            input_bytes,
            None,
            0,
            Some(&raw mut returned),
            None,
        )
    };
    drop(directory);
    match result {
        Ok(()) => Ok(Junction(link.to_path_buf())),
        Err(error) => {
            let _ = fs::remove_dir(link);
            Err(windows_to_io(error))
        }
    }
}

fn mark_sparse(file: &File) -> io::Result<()> {
    let mut returned = 0_u32;
    // SAFETY: `file` keeps the handle alive for this synchronous call;
    // `FSCTL_SET_SPARSE` without an input buffer sets the sparse attribute and
    // produces no output; `returned` is a valid writable `u32`.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(windows_to_io)
}

fn set_default_compression(file: &File) -> io::Result<()> {
    let format: u16 = COMPRESSION_FORMAT_DEFAULT.0;
    let mut returned = 0_u32;
    // SAFETY: `file` keeps the handle alive for this synchronous call; the
    // input is a live `u16` of exactly `size_of::<u16>()` bytes as
    // `FSCTL_SET_COMPRESSION` requires; no output buffer is requested; and
    // `returned` is a valid writable `u32`.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_COMPRESSION,
            Some((&raw const format).cast::<c_void>()),
            u32::try_from(size_of::<u16>()).expect("u16 size fits in u32"),
            None,
            0,
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(windows_to_io)
}

/// Returns the `FILE_*` capability flags of the volume that holds `path`.
fn volume_capability_flags(path: &Path) -> io::Result<u32> {
    let resolved =
        resolve_scan_root(path).map_err(|error| io::Error::other(format!("{error:?}")))?;
    let root = wide_nul(&resolved.volume_path);
    let mut flags = 0_u32;
    // SAFETY: `root` is NUL-terminated for the duration of the call, every
    // optional output buffer is omitted, and `flags` is a valid writable `u32`.
    unsafe {
        GetVolumeInformationW(PCWSTR(root.as_ptr()), None, None, None, Some(&raw mut flags), None)
    }
    .map_err(windows_to_io)?;
    Ok(flags)
}

/// Independent oracle for compressed/sparse allocation, used only to check
/// the adapter's `FILE_STANDARD_INFO.AllocationSize` against the API that the
/// legacy product used.
fn compressed_file_size_oracle(path: &Path) -> io::Result<u64> {
    let wide_path = wide_nul(path);
    let mut high = 0_u32;
    // SAFETY: `wide_path` is NUL-terminated for the duration of the call and
    // `high` is a valid writable `u32`.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide_path.as_ptr()), Some(&raw mut high)) };
    if low == INVALID_FILE_SIZE {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
    Ok(u64::from(high) << 32 | u64::from(low))
}

fn process_handle_count() -> u32 {
    let mut count = 0_u32;
    // SAFETY: the pseudo-handle for the current process needs no cleanup and
    // `count` is a valid writable `u32`.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut count) }
        .expect("query the process handle count");
    count
}

/// Why a privilege-dependent fixture could not be created.
enum FixtureFailure {
    /// The environment refused; the test must skip with this reason.
    Unavailable(String),
    /// Something unexpected happened; the test must fail.
    Unexpected(io::Error),
}

fn classify_fixture_error(stage: &str, error: io::Error) -> FixtureFailure {
    match error.raw_os_error() {
        Some(code @ (1 | 5 | 50 | 1_314)) => {
            FixtureFailure::Unavailable(format!("{stage} failed with OS error {code}"))
        }
        _ => FixtureFailure::Unexpected(error),
    }
}

/// Directory whose listing is denied to the current user through a DACL
/// deny ACE. The original DACL is restored on drop so cleanup always works,
/// even when the assertion that needed the fixture fails.
struct DeniedListing {
    path_wide: Vec<u16>,
    original_descriptor: AlignedBuffer,
    restored: bool,
}

impl DeniedListing {
    fn deny(path: &Path) -> Result<Self, FixtureFailure> {
        let path_wide = wide_nul(path);
        let original_descriptor = read_dacl_descriptor(&path_wide)
            .map_err(|error| classify_fixture_error("GetFileSecurityW", error))?;
        let user = current_user_sid().map_err(FixtureFailure::Unexpected)?;
        let mut guard = Self { path_wide, original_descriptor, restored: false };

        apply_list_deny(&guard.path_wide, &user)
            .map_err(|error| classify_fixture_error("SetFileSecurityW", error))?;

        match fs::read_dir(path) {
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED_CODE) => Ok(guard),
            Ok(_) | Err(_) => {
                guard.restore();
                Err(FixtureFailure::Unavailable(
                    "a deny ACE for the current user did not deny directory listing".to_owned(),
                ))
            }
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let descriptor = PSECURITY_DESCRIPTOR(self.original_descriptor.as_mut_ptr());
        // SAFETY: `path_wide` is NUL-terminated and `original_descriptor`
        // holds the self-relative descriptor previously returned by
        // `GetFileSecurityW`; both outlive this synchronous call.
        let ok = unsafe {
            SetFileSecurityW(PCWSTR(self.path_wide.as_ptr()), DACL_SECURITY_INFORMATION, descriptor)
        };
        if !ok.as_bool() {
            eprintln!(
                "restoring the fixture DACL failed with OS error {:?}",
                io::Error::last_os_error().raw_os_error()
            );
        }
    }
}

impl Drop for DeniedListing {
    fn drop(&mut self) {
        self.restore();
    }
}

fn read_dacl_descriptor(path_wide: &[u16]) -> io::Result<AlignedBuffer> {
    let mut needed = 0_u32;
    // SAFETY: `path_wide` is NUL-terminated, no buffer is supplied for this
    // size probe, and `needed` is a valid writable `u32`.
    let probe = unsafe {
        GetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            None,
            0,
            &raw mut needed,
        )
    };
    if probe.as_bool() {
        return Err(io::Error::other("security descriptor size probe unexpectedly succeeded"));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER_CODE) {
        return Err(error);
    }

    let mut buffer = AlignedBuffer::new(needed as usize);
    // SAFETY: `path_wide` is NUL-terminated and `buffer` provides
    // `len_bytes()` writable bytes for the self-relative descriptor.
    let ok = unsafe {
        GetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            Some(PSECURITY_DESCRIPTOR(buffer.as_mut_ptr())),
            buffer.len_bytes(),
            &raw mut needed,
        )
    };
    if ok.as_bool() { Ok(buffer) } else { Err(io::Error::last_os_error()) }
}

/// Self-contained copy of the current process user's SID.
struct UserSid(AlignedBuffer);

impl UserSid {
    fn as_psid(&self) -> windows::Win32::Security::PSID {
        windows::Win32::Security::PSID(self.0.as_ptr().cast_mut())
    }
}

fn current_user_sid() -> io::Result<UserSid> {
    let mut raw_token = HANDLE::default();
    // SAFETY: the pseudo-handle for the current process needs no cleanup and
    // `raw_token` receives an owned token handle that is wrapped immediately.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw_token) }
        .map_err(windows_to_io)?;
    // SAFETY: `raw_token` is a valid owned handle returned by the successful
    // call above and is owned exactly once from here on.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token.0) };

    let mut needed = 0_u32;
    // SAFETY: `token` keeps the handle alive, no buffer is supplied for this
    // size probe, and `needed` is a valid writable `u32`.
    let probe = unsafe {
        GetTokenInformation(HANDLE(token.as_raw_handle()), TokenUser, None, 0, &raw mut needed)
    };
    match probe {
        Ok(()) => return Err(io::Error::other("token size probe unexpectedly succeeded")),
        Err(error) if win32_code(&error) == ERROR_INSUFFICIENT_BUFFER_CODE => {}
        Err(error) => return Err(windows_to_io(error)),
    }

    let mut user = AlignedBuffer::new(needed as usize);
    // SAFETY: `token` keeps the handle alive and `user` provides `len_bytes()`
    // writable bytes for the `TOKEN_USER` record.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(user.as_mut_ptr()),
            user.len_bytes(),
            &raw mut needed,
        )
    }
    .map_err(windows_to_io)?;

    // SAFETY: the successful call above wrote a `TOKEN_USER` at the start of
    // `user`, whose `Sid` points inside the same buffer, which stays alive
    // until the SID bytes are copied out below.
    let (sid_pointer, sid_length) = unsafe {
        let sid = (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        (sid.0.cast_const().cast::<u8>(), GetLengthSid(sid) as usize)
    };
    let mut copy = AlignedBuffer::new(sid_length);
    // SAFETY: `sid_pointer` addresses `sid_length` readable bytes inside
    // `user`, `copy` has at least `sid_length` writable bytes, and the two
    // allocations do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(sid_pointer, copy.as_mut_ptr().cast::<u8>(), sid_length);
    }
    Ok(UserSid(copy))
}

fn apply_list_deny(path_wide: &[u16], user: &UserSid) -> io::Result<()> {
    let sid_length = user.0.len_bytes() as usize;
    let mut acl =
        AlignedBuffer::new(size_of::<ACL>() + size_of::<ACCESS_DENIED_ACE>() + sid_length);
    // SAFETY: `acl` provides `len_bytes()` zeroed writable bytes for the ACL.
    unsafe { InitializeAcl(acl.as_mut_ptr().cast::<ACL>(), acl.len_bytes(), ACL_REVISION) }
        .map_err(windows_to_io)?;
    // SAFETY: `acl` was initialized above with room for one ACE plus the SID,
    // and `user` keeps the SID bytes alive for the call.
    unsafe {
        AddAccessDeniedAce(
            acl.as_mut_ptr().cast::<ACL>(),
            ACL_REVISION,
            FILE_LIST_DIRECTORY.0,
            user.as_psid(),
        )
    }
    .map_err(windows_to_io)?;

    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor_pointer = PSECURITY_DESCRIPTOR((&raw mut descriptor).cast::<c_void>());
    // SAFETY: `descriptor` is a live, correctly sized absolute descriptor.
    unsafe { InitializeSecurityDescriptor(descriptor_pointer, SECURITY_DESCRIPTOR_REVISION) }
        .map_err(windows_to_io)?;
    // SAFETY: `descriptor` and `acl` both outlive the descriptor's use below.
    unsafe {
        SetSecurityDescriptorDacl(descriptor_pointer, true, Some(acl.as_ptr().cast::<ACL>()), false)
    }
    .map_err(windows_to_io)?;
    // SAFETY: `path_wide` is NUL-terminated and the absolute descriptor plus
    // the ACL it references stay alive for this synchronous call.
    let ok = unsafe {
        SetFileSecurityW(PCWSTR(path_wide.as_ptr()), DACL_SECURITY_INFORMATION, descriptor_pointer)
    };
    if ok.as_bool() { Ok(()) } else { Err(io::Error::last_os_error()) }
}

// ---------------------------------------------------------------------------
// Shared fixture shapes
// ---------------------------------------------------------------------------

/// Bytes of the four ordinary files in the loop fixture.
const LOOP_FILE_BYTES: [(&str, usize); 4] =
    [("root.bin", 3_000), ("a.bin", 2_000), ("b1.bin", 1_000), ("b2.bin", 500)];

/// Builds `root/root.bin`, `root/a/a.bin`, `root/a/b/{b1.bin,b2.bin}` and
/// returns the `root/a/b` directory where a loop entry can be added.
fn build_loop_tree(root: &Path) -> PathBuf {
    let a = root.join("a");
    let b = a.join("b");
    fs::create_dir_all(&b).expect("create loop fixture tree");
    fs::write(root.join("root.bin"), vec![0x11; LOOP_FILE_BYTES[0].1]).expect("write root.bin");
    fs::write(a.join("a.bin"), vec![0x22; LOOP_FILE_BYTES[1].1]).expect("write a.bin");
    fs::write(b.join("b1.bin"), vec![0x33; LOOP_FILE_BYTES[2].1]).expect("write b1.bin");
    fs::write(b.join("b2.bin"), vec![0x44; LOOP_FILE_BYTES[3].1]).expect("write b2.bin");
    b
}

/// Asserts the exact counts of the loop fixture: root, `a`, `b`, and one
/// directory reparse point named `loop`; four ordinary files; one policy
/// omission for the boundary; and no entry below the boundary.
fn assert_loop_fixture(run: &ScanRun, loop_name: &str) {
    run.assert_within_bound();
    assert_eq!(
        run.terminal.state,
        TerminalState::Partial,
        "ADR 0003 keeps a directory reparse point as an explicit boundary omission, so the \
         generation is Partial rather than Complete; this must not be a Failed or a Cancelled state"
    );
    let counters = run.terminal.counters;
    assert_eq!(counters.files, 4, "exactly the four real files are counted");
    assert_eq!(counters.directories, 4, "root, a, b, and the boundary entry itself");
    assert_eq!(counters.reparse_points, 1, "exactly one reparse point");
    assert_eq!(counters.omissions, 1, "exactly one policy omission for the boundary");
    assert_eq!(counters.directories_completed, 3, "only root, a, and b were enumerated");
    assert_eq!(counters.entries, 8, "root + a + b + loop + four files");

    let boundary = run.entry_named(loop_name);
    assert_eq!(boundary.kind, EntryKind::ReparsePoint(ReparseKind::Directory));
    assert!(boundary.file_identity.is_none(), "the boundary target was never opened");
    assert!(
        run.entries().all(|entry| entry.parent != boundary.id),
        "no entry was materialized below the boundary, so the loop was not followed"
    );
    assert!(
        run.completions().all(|completion| completion.directory != boundary.id),
        "the boundary was never enqueued as directory work"
    );
    let omission = run.omissions().next().expect("policy omission");
    assert!(omission.error.path.ends_with(loop_name));
    assert_eq!(omission.error.operation, FsOperation::EnumerateDirectory);
    assert_eq!(omission.error.kind, FsErrorKind::NotSupported);

    let expected_names = ["root.bin", "a.bin", "b1.bin", "b2.bin", "a", "b", loop_name];
    let mut names: Vec<OsString> = run.entries().map(|entry| entry.name.clone()).collect();
    names.sort();
    let mut expected: Vec<OsString> = expected_names.iter().map(OsString::from).collect();
    expected.sort();
    assert_eq!(names, expected);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// FS-04 (ADR 0003): a junction that points back at the scan root is
/// reported as a directory reparse boundary, is never enumerated, and the
/// counts equal the real tree exactly. The time bound proves the loop was
/// not followed slowly either.
#[test]
fn junction_loop_is_a_boundary_and_counts_are_exact() {
    let temp = TestDirectory::new("junction-loop");
    let root = temp.path().join("root");
    let b = build_loop_tree(&root);
    let _junction = create_junction(&b.join("loop"), &root).expect("create junction loop");
    assert!(
        fs::symlink_metadata(b.join("loop")).expect("junction metadata").file_type().is_symlink(),
        "the junction fixture is a directory reparse point"
    );

    let run = run_scan(&root, 1);
    assert_loop_fixture(&run, "loop");
}

/// FS-04 (ADR 0003): the same contract for a directory symbolic link that
/// points back at the scan root. Skips only when symlink creation is denied
/// (`ERROR_PRIVILEGE_NOT_HELD` or `ERROR_ACCESS_DENIED`).
#[test]
fn symlink_loop_is_a_boundary_and_counts_are_exact() {
    let temp = TestDirectory::new("symlink-loop");
    let root = temp.path().join("root");
    let b = build_loop_tree(&root);
    if let Err(error) = symlink_dir(&root, b.join("symloop")) {
        match error.raw_os_error() {
            Some(code @ (ERROR_ACCESS_DENIED_CODE | ERROR_PRIVILEGE_NOT_HELD_CODE)) => {
                skip(&format!("directory symbolic-link creation denied with OS error {code}"));
                return;
            }
            _ => panic!("create directory symlink: {error}"),
        }
    }

    let run = run_scan(&root, 2);
    assert_loop_fixture(&run, "symloop");
}

/// FS-05 (ADR 0002 items 2 and 7): sparse files larger than 4 GiB report
/// their end-of-file as logical size and their filesystem allocation, not a
/// cluster-rounded guess, as allocated size: one file with a 64 KiB written
/// range above 4 GiB and one with none. Together they prove the `u128`
/// aggregate carries values above the `u32` range without truncation.
#[test]
fn sparse_files_over_4_gib_report_logical_eof_and_tiny_allocation() {
    let temp = TestDirectory::new("sparse");
    match volume_capability_flags(temp.path()) {
        Ok(flags) if flags & FILE_SUPPORTS_SPARSE_FILES != 0 => {}
        Ok(flags) => {
            skip(&format!("volume flags 0x{flags:08X} lack FILE_SUPPORTS_SPARSE_FILES"));
            return;
        }
        Err(error) => panic!("query volume capabilities: {error}"),
    }

    const SPARSE_LOGICAL: u64 = 5 * GIB;
    /// Non-zero data written at a 64 KiB-aligned offset above 4 GiB so the
    /// file must allocate at least this much, but nowhere near its length.
    const WRITTEN_BYTES: u64 = 64 * 1_024;
    const WRITTEN_OFFSET: u64 = 4 * GIB;
    const NAMES: [&str; 2] = ["five-gib-written.bin", "five-gib-empty.bin"];
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create sparse root");
    for (index, name) in NAMES.iter().enumerate() {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(name))
            .expect("create sparse file");
        if let Err(error) = mark_sparse(&file) {
            match error.raw_os_error() {
                Some(code @ (1 | 50)) => {
                    skip(&format!("FSCTL_SET_SPARSE unsupported with OS error {code}"));
                    return;
                }
                _ => panic!("mark file sparse: {error}"),
            }
        }
        file.set_len(SPARSE_LOGICAL).expect("extend sparse file");
        if index == 0 {
            io::Seek::seek(&mut file, io::SeekFrom::Start(WRITTEN_OFFSET)).expect("seek");
            io::Write::write_all(&mut file, &vec![0xAB_u8; WRITTEN_BYTES as usize])
                .expect("write the non-zero range");
            file.sync_all().expect("flush sparse file");
        }
    }

    // Direct adapter cross-check: the fixture consumed almost no storage.
    let mut direct_allocations = Vec::new();
    for (index, name) in NAMES.iter().enumerate() {
        let direct = adapter_entry(&root, name);
        assert_eq!(direct.own_metrics.logical().known_bytes(), Some(SPARSE_LOGICAL));
        let allocated = direct.own_metrics.allocated().known_bytes().expect("allocation known");
        assert!(
            allocated < MIB,
            "sparse fixture {name} consumed {allocated} bytes, which is not a sparse allocation"
        );
        if index == 0 {
            assert!(allocated >= WRITTEN_BYTES, "the written range must be allocated");
        }
        let oracle =
            compressed_file_size_oracle(&root.join(name)).expect("GetCompressedFileSizeW oracle");
        eprintln!(
            "sparse evidence ({name}): EndOfFile={SPARSE_LOGICAL} AllocationSize={allocated} \
             GetCompressedFileSizeW={oracle}"
        );
        assert_eq!(
            allocated, oracle,
            "FILE_STANDARD_INFO.AllocationSize must equal the compressed-size oracle for a \
             sparse file"
        );
        direct_allocations.push(allocated);
    }

    let run = run_scan(&root, 3);
    run.assert_within_bound();
    assert_eq!(run.terminal.state, TerminalState::Complete);
    assert_eq!(run.terminal.counters.files, 2);
    for (name, allocated) in NAMES.iter().zip(&direct_allocations) {
        let entry = run.entry_named(name);
        assert_eq!(entry.own_metrics.logical().known_bytes(), Some(SPARSE_LOGICAL));
        assert_eq!(entry.own_metrics.logical().source(), MetricSource::NativeFileInformation);
        assert_eq!(entry.own_metrics.allocated().known_bytes(), Some(*allocated));
        assert_eq!(entry.own_metrics.allocated().source(), MetricSource::FilesystemAllocation);
    }

    let (_, snapshot) = run.reduce();
    let (root_id, root_record) = snapshot.roots().next().expect("one root");
    let aggregate = root_record.aggregate();
    let expected_logical = u128::from(SPARSE_LOGICAL) * 2;
    assert!(expected_logical > u128::from(u32::MAX));
    assert_eq!(aggregate.logical().known_bytes(), expected_logical, "u128 logical aggregate");
    assert!(aggregate.logical().is_fully_known());
    let expected_allocated: u128 = direct_allocations.iter().map(|&bytes| u128::from(bytes)).sum();
    assert_eq!(aggregate.allocated().known_bytes(), expected_allocated);
    assert!(aggregate.allocated().is_fully_known());
    assert_eq!(aggregate.file_count(), 2);
    assert_eq!(snapshot.node(root_id).expect("root").kind(), EntryKind::Root);
}

/// Repetitive but non-zero text: NTFS compresses it well, yet unlike all
/// zeros it must occupy real clusters.
fn compressible_text(bytes: usize) -> Vec<u8> {
    let mut text = Vec::with_capacity(bytes);
    let mut line = 0_u32;
    while text.len() < bytes {
        text.extend_from_slice(format!("DiskPie compressed fixture line {line:08}\n").as_bytes());
        line += 1;
    }
    text.truncate(bytes);
    text
}

/// FS-05 (ADR 0002 item 2): an NTFS-compressed file reports a non-zero
/// allocation below its logical size, and both values come from
/// `FILE_STANDARD_INFO` through the adapter. Skips only when the volume
/// cannot compress.
#[test]
fn compressed_file_reports_filesystem_allocation_below_logical_size() {
    let temp = TestDirectory::new("compressed");
    match volume_capability_flags(temp.path()) {
        Ok(flags) if flags & FILE_FILE_COMPRESSION != 0 => {}
        Ok(flags) => {
            skip(&format!("volume flags 0x{flags:08X} lack FILE_FILE_COMPRESSION"));
            return;
        }
        Err(error) => panic!("query volume capabilities: {error}"),
    }

    const COMPRESSED_LOGICAL: u64 = 4 * MIB;
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create compressed root");
    let path = root.join("text.bin");
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create compressed file");
        if let Err(error) = set_default_compression(&file) {
            match error.raw_os_error() {
                Some(code @ (1 | 50)) => {
                    skip(&format!("FSCTL_SET_COMPRESSION unsupported with OS error {code}"));
                    return;
                }
                _ => panic!("set compression: {error}"),
            }
        }
        io::Write::write_all(&mut file, &compressible_text(COMPRESSED_LOGICAL as usize))
            .expect("write compressible text");
        file.sync_all().expect("flush compressed file");
    }
    assert_eq!(fs::metadata(&path).expect("metadata").len(), COMPRESSED_LOGICAL);

    let direct = adapter_entry(&root, "text.bin");
    let direct_allocated =
        direct.own_metrics.allocated().known_bytes().expect("allocation is known");
    let oracle = compressed_file_size_oracle(&path).expect("GetCompressedFileSizeW oracle");
    eprintln!(
        "compressed evidence: EndOfFile={COMPRESSED_LOGICAL} AllocationSize={direct_allocated} \
         GetCompressedFileSizeW={oracle}"
    );
    assert!(direct_allocated > 0, "non-zero text must allocate storage");
    assert!(
        direct_allocated < COMPRESSED_LOGICAL,
        "allocation {direct_allocated} is not below the {COMPRESSED_LOGICAL} logical size"
    );
    assert_eq!(
        direct_allocated, oracle,
        "FILE_STANDARD_INFO.AllocationSize must equal the compressed-size oracle"
    );

    let run = run_scan(&root, 4);
    run.assert_within_bound();
    assert_eq!(run.terminal.state, TerminalState::Complete);
    let entry = run.entry_named("text.bin");
    assert_eq!(entry.own_metrics.logical().known_bytes(), Some(COMPRESSED_LOGICAL));
    assert_eq!(entry.own_metrics.logical().source(), MetricSource::NativeFileInformation);
    assert_eq!(entry.own_metrics.allocated().known_bytes(), Some(direct_allocated));
    assert_eq!(
        entry.own_metrics.allocated().source(),
        MetricSource::FilesystemAllocation,
        "allocation must be reported by the filesystem, never guessed from the logical size"
    );
}

/// FS-03 (ADR 0002 items 1, 2, and 4): three names for one file count three
/// logical paths but one allocation, owned by the lexicographically smallest
/// raw-UTF-16 relative path; the other aliases keep their logical size with
/// zero allocated contribution.
#[test]
fn hard_links_count_paths_logically_and_identity_once_for_allocation() {
    const LINK_BYTES: u64 = 8_192;
    let temp = TestDirectory::new("hardlinks");
    let root = temp.path().join("root");
    for directory in ["a", "m", "z"] {
        fs::create_dir_all(root.join(directory)).expect("create link directories");
    }
    let first = root.join("z").join("one.bin");
    fs::write(&first, vec![0x5A; LINK_BYTES as usize]).expect("write hard-link source");
    fs::hard_link(&first, root.join("m").join("two.bin")).expect("second link");
    fs::hard_link(&first, root.join("a").join("three.bin")).expect("third link");
    let single_allocation = adapter_entry(&root.join("z"), "one.bin")
        .own_metrics
        .allocated()
        .known_bytes()
        .expect("allocation is known");
    assert!(single_allocation >= LINK_BYTES, "an 8 KiB non-resident file allocates >= 8 KiB");
    eprintln!("hard-link evidence: EndOfFile={LINK_BYTES} AllocationSize={single_allocation}");

    let run = run_scan(&root, 5);
    run.assert_within_bound();
    assert_eq!(run.terminal.state, TerminalState::Complete);
    assert_eq!(run.terminal.counters.files, 3, "file counts remain path/entry counts");
    let identities: Vec<_> = ["one.bin", "two.bin", "three.bin"]
        .iter()
        .map(|name| run.entry_named(name).file_identity.expect("128-bit identity"))
        .collect();
    assert!(identities.windows(2).all(|pair| pair[0] == pair[1]), "one identity for all links");

    let (reducer, snapshot) = run.reduce();
    let (_, root_record) = snapshot.roots().next().expect("one root");
    let aggregate = root_record.aggregate();
    assert_eq!(
        aggregate.logical().known_bytes(),
        u128::from(LINK_BYTES) * 3,
        "ADR 0002 item 1: every visible hard-link path contributes logical bytes"
    );
    assert_eq!(aggregate.file_count(), 3, "ADR 0002 item 1: every hard-link path adds a file");
    assert_eq!(
        aggregate.allocated().known_bytes(),
        u128::from(single_allocation),
        "ADR 0002 item 2: allocated size contributes once per (VolumeKey, FileId128)"
    );
    assert!(aggregate.allocated().is_fully_known());
    assert!(
        aggregate.has_unique_allocation_precision(),
        "every link had a stable identity, so deduplication is exact"
    );

    let owner_id = reducer.core_id(run.entry_named("three.bin").id).expect("owner core id");
    let owner = snapshot.node(owner_id).expect("owner record");
    assert_eq!(
        owner.hard_link_status(),
        HardLinkStatus::Owner { owner: owner_id },
        "ADR 0002 item 4: the allocation belongs to the lexicographically smallest raw-UTF-16 \
         relative path key, which is a\\three.bin among a\\three.bin, m\\two.bin, z\\one.bin"
    );
    assert_eq!(owner.own_metrics().allocated().known_bytes(), Some(single_allocation));
    for alias_name in ["one.bin", "two.bin"] {
        let alias_id = reducer.core_id(run.entry_named(alias_name).id).expect("alias core id");
        let alias = snapshot.node(alias_id).expect("alias record");
        assert_eq!(alias.hard_link_status(), HardLinkStatus::Alias { owner: owner_id });
        assert_eq!(
            alias.own_metrics().allocated().known_bytes(),
            Some(0),
            "ADR 0002 item 4: other aliases show zero allocated contribution"
        );
        assert_eq!(alias.own_metrics().allocated().source(), MetricSource::HardLinkAlias);
        assert_eq!(
            alias.own_metrics().logical().known_bytes(),
            Some(LINK_BYTES),
            "ADR 0002 item 4: aliases retain their logical size"
        );
    }
}

/// FS-07 (ADR 0003 consequences, architecture section 6): a directory whose
/// listing is denied becomes an `AccessDenied` omission, the generation ends
/// `Partial`, every sibling is scanned completely, and nothing panics. Skips
/// only when the DACL change itself is impossible here.
#[test]
fn denied_directory_is_an_omission_and_siblings_complete() {
    let temp = TestDirectory::new("denied");
    let root = temp.path().join("root");
    let denied = root.join("denied");
    let open = root.join("open");
    fs::create_dir_all(&denied).expect("create denied directory");
    fs::create_dir_all(&open).expect("create open directory");
    fs::write(open.join("visible.bin"), vec![0x77; 1_024]).expect("write visible file");
    fs::write(root.join("other.bin"), vec![0x88; 2_048]).expect("write root file");

    let guard = match DeniedListing::deny(&denied) {
        Ok(guard) => guard,
        Err(FixtureFailure::Unavailable(reason)) => {
            skip(&reason);
            return;
        }
        Err(FixtureFailure::Unexpected(error)) => panic!("apply deny DACL: {error}"),
    };

    let run = run_scan(&root, 6);
    run.assert_within_bound();
    assert_eq!(run.terminal.state, TerminalState::Partial);
    assert_eq!(run.terminal.counters.files, 2);
    assert_eq!(run.terminal.counters.directories, 3, "root, denied, and open");
    assert_eq!(run.terminal.counters.directories_completed, 3);
    assert_eq!(run.terminal.counters.omissions, 1);

    let denied_entry = run.entry_named("denied");
    assert_eq!(denied_entry.kind, EntryKind::Directory);
    let omission = run.omissions().next().expect("access-denied omission");
    assert_eq!(omission.error.kind, FsErrorKind::AccessDenied);
    assert_eq!(omission.error.operation, FsOperation::EnumerateDirectory);
    assert_eq!(omission.error.os_code, Some(ERROR_ACCESS_DENIED_CODE));
    assert!(omission.error.path.ends_with("denied"));

    let open_entry = run.entry_named("open");
    let state_of = |id: ScanNodeId| {
        run.completions().find(|completion| completion.directory == id).map(|c| c.state)
    };
    assert_eq!(state_of(denied_entry.id), Some(DirectoryState::Partial));
    assert_eq!(state_of(open_entry.id), Some(DirectoryState::Complete));
    assert_eq!(state_of(run.started_root().id), Some(DirectoryState::Complete));
    assert_eq!(run.entry_named("visible.bin").parent, open_entry.id);

    let (reducer, snapshot) = run.reduce();
    let denied_id = reducer.core_id(denied_entry.id).expect("denied core id");
    let denied_record = snapshot.node(denied_id).expect("denied record");
    assert_eq!(denied_record.own_omissions(), 1);
    assert_eq!(denied_record.aggregate().state(), ScanState::Partial);
    let open_id = reducer.core_id(open_entry.id).expect("open core id");
    assert_eq!(
        snapshot.node(open_id).expect("open record").aggregate().state(),
        ScanState::Complete
    );
    assert_eq!(snapshot.state(), ScanState::Partial);

    drop(guard);
    assert!(fs::read_dir(&denied).is_ok(), "the original DACL was restored");
}

/// FS/UX long paths (architecture section 6): a directory chain whose native
/// path exceeds 300 UTF-16 units is scanned to the bottom with exact counts.
#[test]
fn long_native_paths_are_scanned_to_the_bottom() {
    let temp = TestDirectory::new("long-path");
    let root = temp.path().join("root");
    let mut deepest = root.clone();
    let mut segments = 0_u64;
    while deepest.as_os_str().encode_wide().count() < 300 {
        deepest.push(format!("long-segment-{segments:03}-abcdefgh"));
        segments += 1;
    }
    fs::create_dir_all(&deepest).expect("create long directory chain");
    assert!(deepest.as_os_str().encode_wide().count() > 300);
    for (index, name) in ["leaf-0.bin", "leaf-1.bin", "leaf-2.bin"].iter().enumerate() {
        fs::write(deepest.join(name), vec![0x99; 100 * (index + 1)]).expect("write leaf");
    }

    let run = run_scan(&root, 7);
    run.assert_within_bound();
    assert_eq!(run.terminal.state, TerminalState::Complete);
    assert_eq!(run.terminal.counters.files, 3);
    assert_eq!(run.terminal.counters.directories, segments + 1);
    assert_eq!(run.terminal.counters.directories_completed, segments + 1);
    assert_eq!(run.terminal.counters.omissions, 0);

    let (reducer, snapshot) = run.reduce();
    let leaf = run.entry_named("leaf-2.bin");
    let leaf_id = reducer.core_id(leaf.id).expect("leaf core id");
    let reconstructed = snapshot.path(leaf_id).expect("reconstruct native path");
    let expected = PathBuf::from(&run.started_root().path)
        .join(deepest.strip_prefix(&root).expect("relative chain"))
        .join("leaf-2.bin");
    assert_eq!(wide(reconstructed.as_os_str()), wide(expected.as_os_str()));
    assert!(reconstructed.as_os_str().encode_wide().count() > 300);
    assert_eq!(
        snapshot.roots().next().expect("root").1.aggregate().logical().known_bytes(),
        100 + 200 + 300
    );
}

/// Native names (architecture section 6): Unicode, emoji, and non-scalar
/// UTF-16 names survive a full scan and tree materialization byte for byte.
#[test]
fn unicode_emoji_and_non_scalar_names_survive_end_to_end() {
    let temp = TestDirectory::new("unicode");
    let root = temp.path().join("root");
    let file_name = OsString::from("r\u{e9}sum\u{e9}-\u{1f680}.bin");
    let directory_name = OsString::from("\u{65e5}\u{672c}\u{8a9e}-\u{1f4c1}");
    let mut lone_high = wide(OsStr::new("nested-"));
    lone_high.push(0xD800);
    lone_high.extend(wide(OsStr::new(".bin")));
    let lone_high_name = OsString::from_wide(&lone_high);
    let mut lone_low = vec![0xDC00];
    lone_low.extend(wide(OsStr::new("-dir")));
    let lone_low_name = OsString::from_wide(&lone_low);

    let directory = root.join(&directory_name);
    let inner = directory.join(&lone_low_name);
    fs::create_dir_all(&inner).expect("create native-name directories");
    fs::write(root.join(&file_name), b"seven!!").expect("write emoji file");
    fs::write(directory.join(&lone_high_name), b"hidden").expect("write lone-surrogate file");
    fs::write(inner.join("leaf.bin"), b"leaf").expect("write leaf");

    let run = run_scan(&root, 8);
    run.assert_within_bound();
    assert_eq!(run.terminal.state, TerminalState::Complete);
    assert_eq!(run.terminal.counters.files, 3);
    assert_eq!(run.terminal.counters.directories, 3);

    let names: Vec<Vec<u16>> = run.entries().map(|entry| wide(&entry.name)).collect();
    for expected in [&file_name, &directory_name, &lone_high_name, &lone_low_name] {
        assert!(
            names.iter().any(|name| *name == wide(expected)),
            "name {expected:?} was not reported byte for byte"
        );
    }

    let (reducer, snapshot) = run.reduce();
    let leaf_id = reducer.core_id(run.entry_named("leaf.bin").id).expect("leaf core id");
    let reconstructed = snapshot.path(leaf_id).expect("reconstruct native path");
    let expected = PathBuf::from(&run.started_root().path)
        .join(&directory_name)
        .join(&lone_low_name)
        .join("leaf.bin");
    assert_eq!(wide(reconstructed.as_os_str()), wide(expected.as_os_str()));
    assert_eq!(snapshot.node(leaf_id).expect("leaf record").aggregate().logical().known_bytes(), 4);
}

/// ADR 0004 items 5 and 8: cancelling after the first batch publishes
/// `Cancelled` quickly, and rescanning the same root completes with exact
/// counts. Five rounds prove that sessions join and neither counts nor the
/// process handle count drift.
#[test]
fn repeated_cancel_and_rescan_neither_leaks_nor_drifts() {
    const DIRECTORIES: u64 = 50;
    const FILES_PER_DIRECTORY: u64 = 40;
    let temp = TestDirectory::new("cancel-rescan");
    let root = temp.path().join("root");
    for directory in 0..DIRECTORIES {
        let path = root.join(format!("dir-{directory:03}"));
        fs::create_dir_all(&path).expect("create wide directory");
        for file in 0..FILES_PER_DIRECTORY {
            fs::write(path.join(format!("file-{file:03}.bin")), b"x").expect("write small file");
        }
    }

    let mut handle_baseline = None;
    let mut generation = 100_u64;
    for round in 0..5 {
        let cancelled_at = std::cell::Cell::new(None);
        let cancelled = run_scan_with(&root, generation, |event, cancel| {
            if matches!(event, ScanEvent::Batch(_)) && cancelled_at.get().is_none() {
                cancel.cancel();
                cancelled_at.set(Some(Instant::now()));
            }
        });
        generation += 1;
        let cancel_instant = cancelled_at.get().expect("at least one batch arrived");
        let latency = cancel_instant.elapsed();
        assert_eq!(cancelled.terminal.state, TerminalState::Cancelled, "round {round}");
        assert!(
            latency < Duration::from_secs(5),
            "round {round}: cancellation took {latency:?} to publish"
        );
        assert!(cancelled.terminal.counters.files < DIRECTORIES * FILES_PER_DIRECTORY);

        let rescan = run_scan(&root, generation);
        generation += 1;
        rescan.assert_within_bound();
        assert_eq!(rescan.terminal.state, TerminalState::Complete, "round {round}");
        assert_eq!(rescan.terminal.counters.files, DIRECTORIES * FILES_PER_DIRECTORY);
        assert_eq!(rescan.terminal.counters.directories, DIRECTORIES + 1);
        assert_eq!(rescan.terminal.counters.directories_completed, DIRECTORIES + 1);
        assert_eq!(rescan.terminal.counters.omissions, 0);
        assert!(rescan.terminal.peak_active_workers <= scan_options().worker_count);

        let handles = process_handle_count();
        match handle_baseline {
            None => handle_baseline = Some(handles),
            Some(baseline) => assert!(
                handles <= baseline + 64,
                "round {round}: process handle count grew from {baseline} to {handles}"
            ),
        }
    }
}

/// F-01, F-03, Q-02 (architecture section 4): real scan events flow through
/// the reducer into a frozen snapshot and a bounded layout whose root is the
/// scan root, whose angles are conserved, and whose reparse boundary never
/// acts as a directory with children.
#[test]
fn scan_to_layout_conserves_angles_and_keeps_boundaries_childless() {
    let temp = TestDirectory::new("layout");
    let root = temp.path().join("root");
    let b = build_loop_tree(&root);
    let _junction = create_junction(&b.join("loop"), &root).expect("create junction loop");

    let run = run_scan(&root, 9);
    assert_loop_fixture(&run, "loop");
    let (reducer, snapshot) = run.reduce();
    assert_eq!(*reducer.phase(), SessionPhase::Partial);
    assert_eq!(snapshot.state(), ScanState::Partial);

    let root_id = reducer.core_id(run.started_root().id).expect("root core id");
    let root_record = snapshot.node(root_id).expect("root record");
    assert_eq!(root_record.kind(), EntryKind::Root);
    assert_eq!(root_record.aggregate().file_count(), 4);
    assert_eq!(root_record.aggregate().directory_count(), 4);
    assert_eq!(root_record.aggregate().omission_count(), 1);
    let total_bytes: usize = LOOP_FILE_BYTES.iter().map(|(_, bytes)| bytes).sum();
    assert_eq!(root_record.aggregate().logical().known_bytes(), total_bytes as u128);

    let junction_id = reducer.core_id(run.entry_named("loop").id).expect("junction core id");
    let junction = snapshot.node(junction_id).expect("junction record");
    assert_eq!(junction.kind(), EntryKind::ReparsePoint(ReparseKind::Directory));
    assert!(!junction.kind().can_have_children());
    assert_eq!(snapshot.children(junction_id).expect("children").count(), 0);

    let layout = compute_layout(&snapshot, LayoutOptions::new(root_id), &HiddenBranches::new())
        .expect("compute the sunburst layout");
    assert_eq!(layout.root(), root_id);
    assert!(!layout.sectors().is_empty());
    assert!(layout.rings().len() >= 4, "root, root.bin/a, a.bin/b, b1.bin/b2.bin");
    assert_angular_conservation(&layout, &snapshot);

    for sector in layout.sectors() {
        if sector.node_id == Some(junction_id) {
            assert_eq!(sector.action_target(), Some(junction_id));
            assert_eq!(sector.weight, 0, "a policy-zero boundary carries no weight");
        }
        let parent_is_junction = sector
            .node_id
            .and_then(|id| snapshot.node(id))
            .and_then(|record| record.parent())
            .is_some_and(|parent| parent == junction_id)
            || matches!(sector.kind, SectorKind::Other { parent, .. } if parent == junction_id);
        assert!(!parent_is_junction, "no sector may hang below the reparse boundary");
    }
    let b_id = reducer.core_id(run.entry_named("b").id).expect("b core id");
    let b_children: Vec<NodeId> = layout
        .sectors()
        .iter()
        .filter(|sector| {
            sector
                .node_id
                .and_then(|id| snapshot.node(id))
                .and_then(|record| record.parent())
                .is_some_and(|parent| parent == b_id)
        })
        .filter_map(|sector| sector.node_id)
        .collect();
    assert_eq!(b_children.len(), 2, "b1.bin and b2.bin are laid out; the boundary is weightless");
    assert!(!b_children.contains(&junction_id));
}

/// Angular conservation: every parent's real children (and any `Other`
/// aggregate they were folded into) sweep at most the parent's own sweep.
fn assert_angular_conservation(layout: &SunburstLayout, snapshot: &TreeSnapshot) {
    const EPSILON: f64 = 1e-9;
    let sweep_of = |node: NodeId| {
        layout
            .sectors()
            .iter()
            .find(|sector| sector.kind == SectorKind::Real && sector.node_id == Some(node))
            .map(|sector| sector.sweep_angle)
    };
    let mut sums = std::collections::HashMap::<NodeId, f64>::new();
    for sector in layout.sectors() {
        assert!(sector.sweep_angle >= 0.0 && sector.sweep_angle <= std::f64::consts::TAU + EPSILON);
        assert!(sector.inner_radius <= sector.outer_radius);
        if sector.depth == 0 {
            continue;
        }
        let parent = match sector.kind {
            SectorKind::Real => snapshot
                .node(sector.node_id.expect("real sectors carry a node"))
                .and_then(|record| record.parent())
                .expect("a non-root real sector has a parent"),
            SectorKind::Other { parent, .. } => parent,
            SectorKind::Hidden { .. } => panic!("no branch was hidden"),
        };
        *sums.entry(parent).or_insert(0.0) += sector.sweep_angle;
    }
    assert!(!sums.is_empty(), "the layout has at least one child ring");
    for (parent, sum) in sums {
        let parent_sweep = sweep_of(parent).expect("every parent with children is laid out");
        assert!(
            sum <= parent_sweep + EPSILON,
            "children of {parent} sweep {sum} which exceeds the parent sweep {parent_sweep}"
        );
    }
}
