//! Native Shell actions executed on the DiskPie Shell STA.
//!
//! Every function here runs on the message-pumping STA owned by
//! [`super::shell_service`]. COM interfaces, PIDLs, progress sinks, and file
//! handles are created, used, and released inside one call; only plain data
//! leaves. The module implements the native half of ADR 0007 and ADR 0008:
//!
//! - Open: `SHCreateItemFromParsingName`, `SHGetIDListFromObject`, and
//!   `ShellExecuteExW` with the fixed verb `open` on the owner window;
//! - Reveal: `SHParseDisplayName` and `SHOpenFolderAndSelectItems`;
//! - Installed Apps: `ShellExecuteExW` on the fixed `ms-settings:appsfeatures`
//!   URI with no fallback to `appwiz.cpl`;
//! - Recycle and permanent deletion: one `IFileOperation` per request with an
//!   `IFileOperationProgressSink` that records every per-item callback, after
//!   late identity validation through a handle that never follows reparse
//!   points; and
//! - Recycle Bin: `SHQueryRecycleBinW` for the advisory estimate and
//!   `SHEmptyRecycleBinW` with the exact confirmed scope.
//!
//! No path is ever interpolated into a command line, and no path is retained
//! in an error, `Debug` output, or diagnostic.

use diskpie_app::actions::{
    DeleteEvidence, DeleteMode, DestructiveOutcome, EmptyBinOutcome, FailureStage,
    IdentityAssurance, PostDeleteEvidence, PreDeleteEvidence, RecycleBinEstimate, RecycleBinScope,
    RecycleReceipt, TargetChangeReason, TargetKind, classify_delete_evidence, identity_from_raw,
};
use diskpie_core::FileIdentity;
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    mem::size_of,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::{
            E_INVALIDARG, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME,
            ERROR_PATH_NOT_FOUND, HANDLE, HWND,
        },
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdInfo,
            GetFileInformationByHandleEx,
        },
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
        UI::{
            Shell::{
                Common::ITEMIDLIST, FILEOPERATION_FLAGS, FOF_NO_CONNECTED_ELEMENTS,
                FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
                FOFX_ADDUNDORECORD, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
                IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
                IShellItem, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
                SHCreateItemFromParsingName, SHELLEXECUTEINFOW, SHERB_NOCONFIRMATION,
                SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW, SHGetIDListFromObject,
                SHOpenFolderAndSelectItems, SHParseDisplayName, SHQUERYRBINFO, SHQueryRecycleBinW,
                SIGDN_NORMALDISPLAY, ShellExecuteExW,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::{HRESULT, PCWSTR, Ref, implement},
};

use super::shell_service::OwnerWindow;

/// The only URI DiskPie ever activates: the modern Installed Apps page.
pub const INSTALLED_APPS_URI: &str = "ms-settings:appsfeatures";

/// The only Shell verb DiskPie ever passes to `ShellExecuteExW`.
const OPEN_VERB: &str = "open";

/// Flags shared by every `SHEmptyRecycleBinW` call.
const EMPTY_BIN_FLAGS: u32 = SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND;

/// Native stage at which open, reveal, or Installed Apps dispatch failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchStage {
    /// The path contains an embedded NUL and cannot be a native string.
    EmbeddedNul,
    CreateShellItem,
    GetIdList,
    ParseDisplayName,
    ShellExecute,
    OpenFolderAndSelectItems,
    /// The request was cancelled before the STA touched the Shell.
    CancelledBeforeDispatch,
    ServiceShutdown,
    WorkerPanicked,
}

/// Outcome of a non-destructive activation. `Dispatched` only means Windows
/// accepted the activation; it is never a completion claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Dispatched,
    Failed { stage: DispatchStage, hresult: Option<i32> },
}

impl DispatchOutcome {
    pub(crate) const fn failed(stage: DispatchStage, hresult: i32) -> Self {
        Self::Failed { stage, hresult: Some(hresult) }
    }

    pub(crate) const fn not_run(stage: DispatchStage) -> Self {
        Self::Failed { stage, hresult: None }
    }
}

/// Outcome of the advisory, non-destructive Recycle Bin query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecycleBinQueryOutcome {
    Estimated(RecycleBinEstimate),
    Failed { hresult: Option<i32> },
    NotRun(DispatchStage),
}

/// NUL-terminated UTF-16 buffer built from an exact native path.
struct WideZ(Vec<u16>);

impl WideZ {
    fn from_path(path: &Path) -> Result<Self, DispatchStage> {
        let mut units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if units.contains(&0) {
            return Err(DispatchStage::EmbeddedNul);
        }
        units.push(0);
        Ok(Self(units))
    }

    fn from_str(value: &str) -> Self {
        let mut units = value.encode_utf16().collect::<Vec<_>>();
        units.push(0);
        Self(units)
    }

    fn as_pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }

    fn units(&self) -> &[u16] {
        &self.0
    }
}

/// Owned absolute PIDL freed with `CoTaskMemFree` on drop.
pub(crate) struct IdList(*mut ITEMIDLIST);

impl IdList {
    /// A null list for marshalling tests; freeing null is a documented no-op.
    #[cfg(test)]
    const fn null_for_test() -> Self {
        Self(std::ptr::null_mut())
    }

    fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0.cast_const()
    }
}

impl Drop for IdList {
    fn drop(&mut self) {
        // SAFETY: the pointer was returned by a Shell allocation routine that
        // documents `CoTaskMemFree` (`ILFree`) as its release, or is null.
        unsafe { CoTaskMemFree(Some(self.0.cast::<c_void>().cast_const())) };
    }
}

/// What `ShellExecuteExW` is asked to activate.
pub(crate) enum ExecuteTarget<'a> {
    /// An absolute PIDL passed through `SEE_MASK_IDLIST`.
    IdList(&'a IdList),
    /// The fixed Installed Apps URI.
    InstalledAppsUri,
}

/// Every field DiskPie ever sets on `SHELLEXECUTEINFOW`.
pub(crate) struct ShellExecuteInvocation<'a> {
    pub(crate) owner: OwnerWindow,
    pub(crate) verb: &'static str,
    pub(crate) mask: u32,
    pub(crate) show: i32,
    pub(crate) target: ExecuteTarget<'a>,
}

/// The raw Shell calls behind open, reveal, and Installed Apps.
///
/// The seam exists so argument marshalling can be tested without launching
/// Explorer or Settings. The production implementation is [`NativeShell`].
pub(crate) trait ShellPrimitives {
    /// `SHCreateItemFromParsingName` followed by `SHGetIDListFromObject`.
    fn id_list_from_shell_item(&mut self, path: &[u16]) -> Result<IdList, (DispatchStage, i32)>;
    /// `SHParseDisplayName` with no bind context and no attribute request.
    fn id_list_from_display_name(&mut self, path: &[u16]) -> Result<IdList, (DispatchStage, i32)>;
    fn shell_execute(&mut self, invocation: &ShellExecuteInvocation<'_>) -> Result<(), i32>;
    fn open_folder_and_select(&mut self, item: &IdList) -> Result<(), i32>;
}

/// Production Shell calls; every method must run on the STA.
pub(crate) struct NativeShell;

impl ShellPrimitives for NativeShell {
    fn id_list_from_shell_item(&mut self, path: &[u16]) -> Result<IdList, (DispatchStage, i32)> {
        debug_assert_eq!(path.last(), Some(&0));
        // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the
        // call, no bind context is supplied, and the returned interface is
        // owned by this STA until it drops at the end of the function.
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }
            .map_err(|error| (DispatchStage::CreateShellItem, error.code().0))?;
        // SAFETY: `item` is a live Shell item; the returned PIDL is owned by
        // the `IdList` wrapper immediately.
        let pidl = unsafe { SHGetIDListFromObject(&item) }
            .map_err(|error| (DispatchStage::GetIdList, error.code().0))?;
        Ok(IdList(pidl))
    }

    fn id_list_from_display_name(&mut self, path: &[u16]) -> Result<IdList, (DispatchStage, i32)> {
        debug_assert_eq!(path.last(), Some(&0));
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated UTF-16 for the whole call, `pidl`
        // is a writable out-parameter, no attributes are requested, and the
        // returned PIDL is wrapped immediately.
        unsafe { SHParseDisplayName(PCWSTR(path.as_ptr()), None, &raw mut pidl, 0, None) }
            .map_err(|error| (DispatchStage::ParseDisplayName, error.code().0))?;
        Ok(IdList(pidl))
    }

    fn shell_execute(&mut self, invocation: &ShellExecuteInvocation<'_>) -> Result<(), i32> {
        let verb = WideZ::from_str(invocation.verb);
        let uri = WideZ::from_str(INSTALLED_APPS_URI);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: invocation.mask,
            hwnd: HWND(invocation.owner.as_raw() as *mut c_void),
            lpVerb: verb.as_pcwstr(),
            nShow: invocation.show,
            ..Default::default()
        };
        match invocation.target {
            ExecuteTarget::IdList(list) => {
                info.lpIDList = list.as_ptr().cast_mut().cast::<c_void>();
            }
            ExecuteTarget::InstalledAppsUri => info.lpFile = uri.as_pcwstr(),
        }
        // SAFETY: `info` is fully initialized with `cbSize`, every pointer it
        // holds (`verb`, `uri`, the PIDL) outlives the synchronous call, and
        // `SEE_MASK_NOASYNC` keeps the activation on this STA so no pointer is
        // read after return. Neither `lpParameters` nor `lpDirectory` is set,
        // so no command line is constructed.
        unsafe { ShellExecuteExW(&raw mut info) }.map_err(|error| error.code().0)
    }

    fn open_folder_and_select(&mut self, item: &IdList) -> Result<(), i32> {
        // SAFETY: `item` is a live absolute PIDL owned by the caller for the
        // whole synchronous call; zero selected children means Explorer opens
        // the parent and selects the item itself.
        unsafe { SHOpenFolderAndSelectItems(item.as_ptr(), None, 0) }
            .map_err(|error| error.code().0)
    }
}

const fn open_mask() -> u32 {
    SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI
}

const fn uri_mask() -> u32 {
    SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI
}

/// Activates one real item with the fixed `open` verb.
pub(crate) fn open_item<P: ShellPrimitives>(
    shell: &mut P,
    owner: OwnerWindow,
    path: &Path,
) -> DispatchOutcome {
    let wide = match WideZ::from_path(path) {
        Ok(wide) => wide,
        Err(stage) => return DispatchOutcome::failed(stage, E_INVALIDARG.0),
    };
    let id_list = match shell.id_list_from_shell_item(wide.units()) {
        Ok(id_list) => id_list,
        Err((stage, hresult)) => return DispatchOutcome::failed(stage, hresult),
    };
    let invocation = ShellExecuteInvocation {
        owner,
        verb: OPEN_VERB,
        mask: open_mask(),
        show: SW_SHOWNORMAL.0,
        target: ExecuteTarget::IdList(&id_list),
    };
    match shell.shell_execute(&invocation) {
        Ok(()) => DispatchOutcome::Dispatched,
        Err(hresult) => DispatchOutcome::failed(DispatchStage::ShellExecute, hresult),
    }
}

/// Opens the parent folder in Explorer and selects the item.
pub(crate) fn reveal_item<P: ShellPrimitives>(shell: &mut P, path: &Path) -> DispatchOutcome {
    let wide = match WideZ::from_path(path) {
        Ok(wide) => wide,
        Err(stage) => return DispatchOutcome::failed(stage, E_INVALIDARG.0),
    };
    let id_list = match shell.id_list_from_display_name(wide.units()) {
        Ok(id_list) => id_list,
        Err((stage, hresult)) => return DispatchOutcome::failed(stage, hresult),
    };
    match shell.open_folder_and_select(&id_list) {
        Ok(()) => DispatchOutcome::Dispatched,
        Err(hresult) => DispatchOutcome::failed(DispatchStage::OpenFolderAndSelectItems, hresult),
    }
}

/// Dispatches the fixed Installed Apps URI; failure is reported, never masked.
pub(crate) fn open_installed_apps<P: ShellPrimitives>(
    shell: &mut P,
    owner: OwnerWindow,
) -> DispatchOutcome {
    let invocation = ShellExecuteInvocation {
        owner,
        verb: OPEN_VERB,
        mask: uri_mask(),
        show: SW_SHOWNORMAL.0,
        target: ExecuteTarget::InstalledAppsUri,
    };
    match shell.shell_execute(&invocation) {
        Ok(()) => DispatchOutcome::Dispatched,
        Err(hresult) => DispatchOutcome::failed(DispatchStage::ShellExecute, hresult),
    }
}

/// Exact `IFileOperation` flag profile for one deletion mode.
pub(crate) const fn delete_operation_flags(mode: DeleteMode) -> FILEOPERATION_FLAGS {
    let common = FILEOPERATION_FLAGS(
        FOF_NOCONFIRMATION.0
            | FOF_NOERRORUI.0
            | FOF_SILENT.0
            | FOF_NO_CONNECTED_ELEMENTS.0
            | FOFX_EARLYFAILURE.0,
    );
    match mode {
        DeleteMode::Recycle => FILEOPERATION_FLAGS(
            common.0 | FOFX_RECYCLEONDELETE.0 | FOFX_ADDUNDORECORD.0 | FOF_WANTNUKEWARNING.0,
        ),
        DeleteMode::Permanent => common,
    }
}

/// Why late identity validation refused to proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityValidationFailure {
    TargetChanged(TargetChangeReason),
    /// The item could not be inspected; nothing was mutated.
    Failed {
        code: i32,
    },
}

fn open_for_identity(path: &Path) -> io::Result<File> {
    let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    let flags = (FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).0;
    OpenOptions::new().access_mode(0).share_mode(share).custom_flags(flags).open(path)
}

fn query_attribute_tag(file: &File) -> Result<FILE_ATTRIBUTE_TAG_INFO, i32> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `file` keeps a valid handle alive, `info` is a correctly sized
    // writable buffer for `FileAttributeTagInfo`, and the pointer is confined
    // to this synchronous call.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileAttributeTagInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|error| error.code().0)?;
    Ok(info)
}

fn query_identity(file: &File) -> Result<FileIdentity, i32> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` keeps a valid handle alive, `info` is a correctly sized
    // writable buffer for `FileIdInfo`, and the pointer is confined to this
    // synchronous call.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|error| error.code().0)?;
    Ok(identity_from_raw(info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)))
}

/// Reads the current `(volume, FILE_ID_128)` of the object at `path` itself.
///
/// The open never follows a reparse point. Tests use this to capture the
/// identity a confirmation is bound to.
pub fn read_file_identity(path: &Path) -> io::Result<FileIdentity> {
    let file = open_for_identity(path)?;
    query_identity(&file).map_err(|code| io::Error::from_raw_os_error(win32_code(code)))
}

fn win32_code(hresult: i32) -> i32 {
    if (hresult as u32) & 0xFFFF_0000 == 0x8007_0000 { hresult & 0xFFFF } else { hresult }
}

fn is_missing_code(code: i32) -> bool {
    [ERROR_FILE_NOT_FOUND.0, ERROR_PATH_NOT_FOUND.0, ERROR_INVALID_NAME.0]
        .iter()
        .any(|missing| i32::try_from(*missing).is_ok_and(|missing| missing == code))
}

fn observed_kind(info: &FILE_ATTRIBUTE_TAG_INFO) -> ObservedKind {
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        ObservedKind::ReparsePoint
    } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        ObservedKind::Directory
    } else {
        ObservedKind::File
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservedKind {
    File,
    Directory,
    ReparsePoint,
}

const fn kind_matches(expected: TargetKind, observed: ObservedKind) -> bool {
    match expected {
        TargetKind::File => matches!(observed, ObservedKind::File),
        TargetKind::Directory | TargetKind::ScanRoot => matches!(observed, ObservedKind::Directory),
        TargetKind::ReparsePoint(_) => matches!(observed, ObservedKind::ReparsePoint),
    }
}

/// Validates the confirmed target immediately before a path-based mutation.
///
/// The handle is closed before the function returns so it never blocks the
/// Shell's own delete. A missing item or a changed kind or identity is
/// `TargetChanged`; an item that cannot be inspected is `Failed`.
pub(crate) fn validate_target_identity(
    path: &Path,
    expected_kind: TargetKind,
    expected: Option<FileIdentity>,
) -> Result<IdentityAssurance, IdentityValidationFailure> {
    let file = match open_for_identity(path) {
        Ok(file) => file,
        Err(error) => {
            let code = error.raw_os_error().unwrap_or(-1);
            return Err(if is_missing_code(code) {
                IdentityValidationFailure::TargetChanged(TargetChangeReason::Missing)
            } else {
                IdentityValidationFailure::Failed { code }
            });
        }
    };
    let tag =
        query_attribute_tag(&file).map_err(|code| IdentityValidationFailure::Failed { code })?;
    if !kind_matches(expected_kind, observed_kind(&tag)) {
        return Err(IdentityValidationFailure::TargetChanged(TargetChangeReason::KindMismatch));
    }
    let Some(expected) = expected else {
        return Ok(IdentityAssurance::KindOnly);
    };
    let current =
        query_identity(&file).map_err(|code| IdentityValidationFailure::Failed { code })?;
    if current != expected {
        return Err(IdentityValidationFailure::TargetChanged(TargetChangeReason::IdentityMismatch));
    }
    Ok(IdentityAssurance::Exact)
}

/// Everything the STA needs for one recycle or permanent-delete request.
pub(crate) struct DeleteJob<'a> {
    pub(crate) owner: OwnerWindow,
    pub(crate) path: &'a Path,
    pub(crate) kind: TargetKind,
    pub(crate) identity: Option<FileIdentity>,
    pub(crate) mode: DeleteMode,
    pub(crate) cancel: &'a Arc<AtomicBool>,
}

#[derive(Default)]
struct SinkLog {
    started: bool,
    pre_delete: Vec<PreDeleteEvidence>,
    post_delete: Vec<PostDeleteEvidence>,
    finish: Option<i32>,
}

struct SinkRecord {
    cancel: Arc<AtomicBool>,
    log: Mutex<SinkLog>,
}

impl SinkRecord {
    fn with_log<R>(&self, update: impl FnOnce(&mut SinkLog) -> R) -> R {
        let mut log = self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut log)
    }
}

/// Progress sink that records callbacks and honours cooperative cancellation.
#[implement(IFileOperationProgressSink)]
struct ProgressSink {
    record: Arc<SinkRecord>,
}

fn cancelled_error() -> windows::core::Error {
    windows::core::Error::from_hresult(HRESULT::from_win32(ERROR_CANCELLED.0))
}

fn receipt_from_item(item: Option<&IShellItem>) -> Option<RecycleReceipt> {
    let item = item?;
    // SAFETY: `item` is a live interface supplied for the duration of this
    // synchronous callback on the STA; the returned allocation is wrapped and
    // freed immediately.
    let display_name = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }.ok().and_then(|name| {
        if name.is_null() {
            return None;
        }
        // SAFETY: `name` is a non-null NUL-terminated CoTaskMem string that
        // is freed exactly once below.
        let value = unsafe { name.to_string() }.ok();
        // SAFETY: `name` is the allocation returned above and has not been freed.
        unsafe { CoTaskMemFree(Some(name.as_ptr().cast_const().cast::<c_void>())) };
        value
    });
    Some(RecycleReceipt { display_name })
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for ProgressSink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        self.record.with_log(|log| log.started = true);
        Ok(())
    }

    fn FinishOperations(&self, hrresult: HRESULT) -> windows::core::Result<()> {
        self.record.with_log(|log| log.finish = Some(hrresult.0));
        Ok(())
    }

    fn PreRenameItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostRenameItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
        _hrrename: HRESULT,
        _psinewlycreated: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreMoveItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        _psidestinationfolder: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostMoveItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        _psidestinationfolder: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
        _hrmove: HRESULT,
        _psinewlycreated: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreCopyItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        _psidestinationfolder: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostCopyItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        _psidestinationfolder: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
        _hrcopy: HRESULT,
        _psinewlycreated: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PreDeleteItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        let cancelled = self.record.cancel.load(Ordering::Acquire);
        self.record.with_log(|log| log.pre_delete.push(PreDeleteEvidence { cancelled }));
        if cancelled { Err(cancelled_error()) } else { Ok(()) }
    }

    fn PostDeleteItem(
        &self,
        _dwflags: u32,
        _psiitem: Ref<'_, IShellItem>,
        hrdelete: HRESULT,
        psinewlycreated: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        let newly_created = receipt_from_item(psinewlycreated.as_ref());
        self.record.with_log(|log| {
            log.post_delete.push(PostDeleteEvidence { hresult: hrdelete.0, newly_created });
        });
        Ok(())
    }

    fn PreNewItem(
        &self,
        _dwflags: u32,
        _psidestinationfolder: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn PostNewItem(
        &self,
        _dwflags: u32,
        _psidestinationfolder: Ref<'_, IShellItem>,
        _psznewname: &PCWSTR,
        _psztemplatename: &PCWSTR,
        _dwfileattributes: u32,
        _hrnew: HRESULT,
        _psinewitem: Ref<'_, IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn UpdateProgress(&self, _iworktotal: u32, _iworksofar: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// RAII `Advise` cookie that always `Unadvise`s before the operation drops.
struct AdviseGuard<'a> {
    operation: &'a IFileOperation,
    cookie: Option<u32>,
    unadvise_hresult: Option<i32>,
}

impl<'a> AdviseGuard<'a> {
    fn advise(
        operation: &'a IFileOperation,
        sink: &IFileOperationProgressSink,
    ) -> Result<Self, i32> {
        // SAFETY: both interfaces are live and owned by this STA; the cookie is
        // retained for the balancing `Unadvise`.
        let cookie = unsafe { operation.Advise(sink) }.map_err(|error| error.code().0)?;
        Ok(Self { operation, cookie: Some(cookie), unadvise_hresult: None })
    }

    fn unadvise(&mut self) -> Option<i32> {
        if let Some(cookie) = self.cookie.take() {
            // SAFETY: the cookie came from `Advise` on this same operation and
            // is released exactly once.
            self.unadvise_hresult =
                unsafe { self.operation.Unadvise(cookie) }.err().map(|error| error.code().0);
        }
        self.unadvise_hresult
    }
}

impl Drop for AdviseGuard<'_> {
    fn drop(&mut self) {
        let _ = self.unadvise();
    }
}

/// Runs one recycle or permanent-delete request and classifies the evidence.
///
/// Identity validation runs first; every failure before `PerformOperations`
/// is reported as `Failed` because nothing was queued or mutated. All COM
/// objects are released before the function returns.
pub(crate) fn run_delete(job: &DeleteJob<'_>) -> DestructiveOutcome {
    if job.cancel.load(Ordering::Acquire) {
        return DestructiveOutcome::CancelledBeforeMutation;
    }
    let assurance = match validate_target_identity(job.path, job.kind, job.identity) {
        Ok(assurance) => assurance,
        Err(IdentityValidationFailure::TargetChanged(reason)) => {
            return DestructiveOutcome::TargetChanged { reason };
        }
        Err(IdentityValidationFailure::Failed { code }) => {
            return DestructiveOutcome::Failed {
                stage: FailureStage::IdentityValidation,
                code: Some(code),
            };
        }
    };
    let wide = match WideZ::from_path(job.path) {
        Ok(wide) => wide,
        Err(_) => {
            return DestructiveOutcome::Failed {
                stage: FailureStage::CreateShellItem,
                code: Some(E_INVALIDARG.0),
            };
        }
    };

    // SAFETY: COM is initialized as an STA on this thread; the operation never
    // leaves this function and is released on return.
    let operation: IFileOperation =
        match unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) } {
            Ok(operation) => operation,
            Err(error) => {
                return DestructiveOutcome::Failed {
                    stage: FailureStage::CreateOperation,
                    code: Some(error.code().0),
                };
            }
        };
    // SAFETY: the owner HWND is a borrowed value the UI keeps alive until the
    // terminal event, used only for this operation's UI parenting.
    if let Err(error) = unsafe { operation.SetOwnerWindow(HWND(job.owner.as_raw() as *mut c_void)) }
    {
        return DestructiveOutcome::Failed {
            stage: FailureStage::SetOwnerWindow,
            code: Some(error.code().0),
        };
    }
    // SAFETY: the flags are the exact documented profile for this mode.
    if let Err(error) = unsafe { operation.SetOperationFlags(delete_operation_flags(job.mode)) } {
        return DestructiveOutcome::Failed {
            stage: FailureStage::SetOperationFlags,
            code: Some(error.code().0),
        };
    }

    let record = Arc::new(SinkRecord {
        cancel: Arc::clone(job.cancel),
        log: Mutex::new(SinkLog::default()),
    });
    let sink: IFileOperationProgressSink = ProgressSink { record: Arc::clone(&record) }.into();
    let mut advise = match AdviseGuard::advise(&operation, &sink) {
        Ok(guard) => guard,
        Err(code) => {
            return DestructiveOutcome::Failed { stage: FailureStage::Advise, code: Some(code) };
        }
    };

    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call
    // and the returned item is owned by this STA until it drops below.
    let item: IShellItem = match unsafe { SHCreateItemFromParsingName(wide.as_pcwstr(), None) } {
        Ok(item) => item,
        Err(error) => {
            return DestructiveOutcome::Failed {
                stage: FailureStage::CreateShellItem,
                code: Some(error.code().0),
            };
        }
    };
    // SAFETY: `item` is live; queuing does not mutate anything yet.
    if let Err(error) = unsafe { operation.DeleteItem(&item, None) } {
        return DestructiveOutcome::Failed {
            stage: FailureStage::QueueDelete,
            code: Some(error.code().0),
        };
    }
    // SAFETY: the operation, sink, and item are live for the whole
    // synchronous call; callbacks arrive on this STA.
    let perform = unsafe { operation.PerformOperations() }.map_err(|error| error.code().0);
    // SAFETY: the operation is still live; the query has no side effects.
    let aborted = unsafe { operation.GetAnyOperationsAborted() }
        .map(|flag| flag.as_bool())
        .map_err(|error| error.code().0);
    let _unadvise = advise.unadvise();
    drop(advise);
    drop(item);
    drop(sink);
    drop(operation);

    let log = record.with_log(std::mem::take);
    let evidence = DeleteEvidence {
        mode: job.mode,
        target_kind: job.kind,
        assurance,
        cancel_requested: job.cancel.load(Ordering::Acquire),
        started: log.started,
        pre_delete: log.pre_delete,
        post_delete: log.post_delete,
        finish: log.finish,
        perform,
        aborted,
    };
    classify_delete_evidence(&evidence)
}

/// NUL-terminated root for a scope, or `None` only for the explicit all-drives scope.
pub(crate) fn recycle_bin_root(scope: &RecycleBinScope) -> Option<Vec<u16>> {
    match scope {
        RecycleBinScope::Drive(root) => {
            let mut units = root.as_path().as_os_str().encode_wide().collect::<Vec<_>>();
            debug_assert!(!units.is_empty());
            units.push(0);
            Some(units)
        }
        RecycleBinScope::AllDrives => None,
    }
}

fn root_pcwstr(root: Option<&Vec<u16>>) -> PCWSTR {
    root.map_or(PCWSTR::null(), |units| PCWSTR(units.as_ptr()))
}

/// Queries the advisory Recycle Bin estimate for one scope.
pub(crate) fn query_recycle_bin(scope: &RecycleBinScope) -> Result<RecycleBinEstimate, i32> {
    let root = recycle_bin_root(scope);
    let mut info =
        SHQUERYRBINFO { cbSize: size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    // SAFETY: `root` is either null (all drives) or a NUL-terminated UTF-16
    // string that outlives the call; `info` is a writable struct with its
    // size set.
    unsafe { SHQueryRecycleBinW(root_pcwstr(root.as_ref()), &raw mut info) }
        .map_err(|error| error.code().0)?;
    Ok(RecycleBinEstimate {
        items: u64::try_from(info.i64NumItems).unwrap_or(0),
        bytes: u64::try_from(info.i64Size).unwrap_or(0),
    })
}

/// Empties exactly the confirmed scope and reports before/after estimates.
pub(crate) fn empty_recycle_bin(
    owner: OwnerWindow,
    scope: &RecycleBinScope,
    cancel: &AtomicBool,
) -> EmptyBinOutcome {
    if cancel.load(Ordering::Acquire) {
        return EmptyBinOutcome::CancelledBeforeMutation;
    }
    let before = query_recycle_bin(scope).ok();
    let root = recycle_bin_root(scope);
    // SAFETY: the owner HWND is a borrowed value kept alive by the UI, `root`
    // outlives the synchronous call, and the flags are the fixed documented
    // no-UI profile.
    let result = unsafe {
        SHEmptyRecycleBinW(
            Some(HWND(owner.as_raw() as *mut c_void)),
            root_pcwstr(root.as_ref()),
            EMPTY_BIN_FLAGS,
        )
    };
    let after = query_recycle_bin(scope).ok();
    classify_empty_bin(result.map_err(|error| error.code().0), before, after)
}

/// Combines the empty-bin HRESULT with the post-operation observation.
pub(crate) fn classify_empty_bin(
    result: Result<(), i32>,
    before: Option<RecycleBinEstimate>,
    after: Option<RecycleBinEstimate>,
) -> EmptyBinOutcome {
    match result {
        Ok(()) => EmptyBinOutcome::Completed { before, after },
        Err(code) => match (before, after) {
            (Some(before_estimate), Some(after_estimate)) if before_estimate == after_estimate => {
                EmptyBinOutcome::Failed {
                    stage: FailureStage::EmptyRecycleBin,
                    code: Some(code),
                    before,
                }
            }
            _ => EmptyBinOutcome::UnknownMayHaveMutated {
                stage: FailureStage::EmptyRecycleBin,
                code: Some(code),
                before,
                after,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_app::actions::DriveRoot;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    use windows::Win32::UI::Shell::{
        FOFX_NOSKIPJUNCTIONS, FOFX_REQUIREELEVATION, FOFX_SHOWELEVATIONPROMPT,
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct TestDirectory(PathBuf);

    impl TestDirectory {
        pub(crate) fn new(label: &str) -> Self {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("diskpie-shell-actions-{label}-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn owner() -> OwnerWindow {
        OwnerWindow::from_raw(0x51).expect("nonzero test HWND value")
    }

    #[derive(Debug, PartialEq)]
    enum Recorded {
        ShellItemIdList(Vec<u16>),
        DisplayNameIdList(Vec<u16>),
        Execute { owner: isize, verb: String, mask: u32, show: i32, uses_id_list: bool },
        OpenFolderAndSelect,
    }

    #[derive(Default)]
    struct RecordingShell {
        calls: Vec<Recorded>,
        fail_id_list: Option<(DispatchStage, i32)>,
        fail_execute: Option<i32>,
        fail_select: Option<i32>,
    }

    impl ShellPrimitives for RecordingShell {
        fn id_list_from_shell_item(
            &mut self,
            path: &[u16],
        ) -> Result<IdList, (DispatchStage, i32)> {
            self.calls.push(Recorded::ShellItemIdList(path.to_vec()));
            match self.fail_id_list {
                Some(failure) => Err(failure),
                None => Ok(IdList::null_for_test()),
            }
        }

        fn id_list_from_display_name(
            &mut self,
            path: &[u16],
        ) -> Result<IdList, (DispatchStage, i32)> {
            self.calls.push(Recorded::DisplayNameIdList(path.to_vec()));
            match self.fail_id_list {
                Some(failure) => Err(failure),
                None => Ok(IdList::null_for_test()),
            }
        }

        fn shell_execute(&mut self, invocation: &ShellExecuteInvocation<'_>) -> Result<(), i32> {
            self.calls.push(Recorded::Execute {
                owner: invocation.owner.as_raw(),
                verb: invocation.verb.to_owned(),
                mask: invocation.mask,
                show: invocation.show,
                uses_id_list: matches!(invocation.target, ExecuteTarget::IdList(_)),
            });
            self.fail_execute.map_or(Ok(()), Err)
        }

        fn open_folder_and_select(&mut self, _item: &IdList) -> Result<(), i32> {
            self.calls.push(Recorded::OpenFolderAndSelect);
            self.fail_select.map_or(Ok(()), Err)
        }
    }

    fn wide(path: &str) -> Vec<u16> {
        let mut units = path.encode_utf16().collect::<Vec<_>>();
        units.push(0);
        units
    }

    #[test]
    fn open_marshals_the_exact_path_verb_owner_and_mask() {
        let mut shell = RecordingShell::default();
        let path = r"C:\zqx\--option-like name & 100% ok.txt";
        assert_eq!(open_item(&mut shell, owner(), Path::new(path)), DispatchOutcome::Dispatched);
        assert_eq!(
            shell.calls,
            vec![
                Recorded::ShellItemIdList(wide(path)),
                Recorded::Execute {
                    owner: 0x51,
                    verb: "open".into(),
                    mask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                    show: SW_SHOWNORMAL.0,
                    uses_id_list: true,
                },
            ]
        );
    }

    #[test]
    fn open_reports_each_failing_stage_and_never_executes_after_item_failure() {
        let mut shell = RecordingShell {
            fail_id_list: Some((DispatchStage::CreateShellItem, -3)),
            ..Default::default()
        };
        assert_eq!(
            open_item(&mut shell, owner(), Path::new(r"C:\zqx\missing")),
            DispatchOutcome::failed(DispatchStage::CreateShellItem, -3)
        );
        assert_eq!(shell.calls.len(), 1);

        let mut shell = RecordingShell { fail_execute: Some(-4), ..Default::default() };
        assert_eq!(
            open_item(&mut shell, owner(), Path::new(r"C:\zqx\file")),
            DispatchOutcome::failed(DispatchStage::ShellExecute, -4)
        );

        let mut shell = RecordingShell::default();
        let with_nul = PathBuf::from(std::ffi::OsString::from("C:\\zqx\\a\0b"));
        assert_eq!(
            open_item(&mut shell, owner(), &with_nul),
            DispatchOutcome::failed(DispatchStage::EmbeddedNul, E_INVALIDARG.0)
        );
        assert!(shell.calls.is_empty());
    }

    #[test]
    fn reveal_parses_the_display_name_and_selects_without_a_command_line() {
        let mut shell = RecordingShell::default();
        let path = r"\\?\C:\zqx\very long\item";
        assert_eq!(reveal_item(&mut shell, Path::new(path)), DispatchOutcome::Dispatched);
        assert_eq!(
            shell.calls,
            vec![Recorded::DisplayNameIdList(wide(path)), Recorded::OpenFolderAndSelect]
        );

        let mut shell = RecordingShell { fail_select: Some(-8), ..Default::default() };
        assert_eq!(
            reveal_item(&mut shell, Path::new(path)),
            DispatchOutcome::failed(DispatchStage::OpenFolderAndSelectItems, -8)
        );
        let mut shell = RecordingShell {
            fail_id_list: Some((DispatchStage::ParseDisplayName, -9)),
            ..Default::default()
        };
        assert_eq!(
            reveal_item(&mut shell, Path::new(path)),
            DispatchOutcome::failed(DispatchStage::ParseDisplayName, -9)
        );
    }

    #[test]
    fn installed_apps_uses_only_the_fixed_uri_and_reports_failure() {
        assert_eq!(INSTALLED_APPS_URI, "ms-settings:appsfeatures");
        let mut shell = RecordingShell::default();
        assert_eq!(open_installed_apps(&mut shell, owner()), DispatchOutcome::Dispatched);
        assert_eq!(
            shell.calls,
            vec![Recorded::Execute {
                owner: 0x51,
                verb: "open".into(),
                mask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                show: SW_SHOWNORMAL.0,
                uses_id_list: false,
            }]
        );
        let mut shell = RecordingShell { fail_execute: Some(-2), ..Default::default() };
        assert_eq!(
            open_installed_apps(&mut shell, owner()),
            DispatchOutcome::failed(DispatchStage::ShellExecute, -2)
        );
        assert_eq!(shell.calls.len(), 1, "no appwiz.cpl fallback is attempted");
    }

    #[test]
    fn delete_flag_profiles_are_exact() {
        let common = FOF_NOCONFIRMATION
            | FOF_NOERRORUI
            | FOF_SILENT
            | FOF_NO_CONNECTED_ELEMENTS
            | FOFX_EARLYFAILURE;
        assert_eq!(
            delete_operation_flags(DeleteMode::Recycle),
            common | FOFX_RECYCLEONDELETE | FOFX_ADDUNDORECORD | FOF_WANTNUKEWARNING
        );
        assert_eq!(delete_operation_flags(DeleteMode::Permanent), common);
        for mode in [DeleteMode::Recycle, DeleteMode::Permanent] {
            let flags = delete_operation_flags(mode);
            assert!(!flags.contains(FOFX_SHOWELEVATIONPROMPT));
            assert!(!flags.contains(FOFX_REQUIREELEVATION));
            assert!(!flags.contains(FOFX_NOSKIPJUNCTIONS));
        }
        assert!(!delete_operation_flags(DeleteMode::Permanent).contains(FOFX_RECYCLEONDELETE));
        assert!(!delete_operation_flags(DeleteMode::Permanent).contains(FOFX_ADDUNDORECORD));
        assert!(!delete_operation_flags(DeleteMode::Permanent).contains(FOF_WANTNUKEWARNING));
    }

    #[test]
    fn recycle_bin_root_is_null_only_for_all_drives() {
        assert_eq!(recycle_bin_root(&RecycleBinScope::AllDrives), None);
        let drive = RecycleBinScope::Drive(DriveRoot::parse(r"C:\").expect("root"));
        assert_eq!(recycle_bin_root(&drive), Some(wide(r"C:\")));
        assert_eq!(EMPTY_BIN_FLAGS, SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND);
    }

    #[test]
    fn empty_bin_classification_reports_partial_mutation_honestly() {
        let estimate = |items| RecycleBinEstimate { items, bytes: items * 10 };
        assert_eq!(
            classify_empty_bin(Ok(()), Some(estimate(3)), Some(estimate(0))),
            EmptyBinOutcome::Completed { before: Some(estimate(3)), after: Some(estimate(0)) }
        );
        assert_eq!(
            classify_empty_bin(Err(-5), Some(estimate(3)), Some(estimate(3))),
            EmptyBinOutcome::Failed {
                stage: FailureStage::EmptyRecycleBin,
                code: Some(-5),
                before: Some(estimate(3))
            }
        );
        assert_eq!(
            classify_empty_bin(Err(-5), Some(estimate(3)), Some(estimate(1))),
            EmptyBinOutcome::UnknownMayHaveMutated {
                stage: FailureStage::EmptyRecycleBin,
                code: Some(-5),
                before: Some(estimate(3)),
                after: Some(estimate(1)),
            }
        );
        assert_eq!(
            classify_empty_bin(Err(-5), None, None),
            EmptyBinOutcome::UnknownMayHaveMutated {
                stage: FailureStage::EmptyRecycleBin,
                code: Some(-5),
                before: None,
                after: None,
            }
        );
    }

    #[test]
    fn identity_validation_matches_missing_kind_and_identity_changes() {
        let directory = TestDirectory::new("identity");
        let file = directory.path().join("target.bin");
        fs::write(&file, b"payload").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");

        assert_eq!(
            validate_target_identity(&file, TargetKind::File, Some(identity)),
            Ok(IdentityAssurance::Exact)
        );
        assert_eq!(
            validate_target_identity(&file, TargetKind::File, None),
            Ok(IdentityAssurance::KindOnly)
        );
        assert_eq!(
            validate_target_identity(&file, TargetKind::Directory, Some(identity)),
            Err(IdentityValidationFailure::TargetChanged(TargetChangeReason::KindMismatch))
        );
        assert_eq!(
            validate_target_identity(directory.path(), TargetKind::Directory, None),
            Ok(IdentityAssurance::KindOnly)
        );
        assert_eq!(
            validate_target_identity(
                &directory.path().join("absent"),
                TargetKind::File,
                Some(identity)
            ),
            Err(IdentityValidationFailure::TargetChanged(TargetChangeReason::Missing))
        );

        fs::remove_file(&file).expect("remove fixture");
        fs::write(&file, b"replacement").expect("recreate fixture");
        assert_eq!(
            validate_target_identity(&file, TargetKind::File, Some(identity)),
            Err(IdentityValidationFailure::TargetChanged(TargetChangeReason::IdentityMismatch))
        );
        assert!(file.exists(), "validation never mutates the fixture");
    }

    #[test]
    fn run_delete_refuses_a_swapped_target_without_mutating_it() {
        let directory = TestDirectory::new("swap");
        let file = directory.path().join("confirmed.bin");
        fs::write(&file, b"original").expect("write fixture");
        let identity = read_file_identity(&file).expect("identity");
        fs::remove_file(&file).expect("remove fixture");
        fs::write(&file, b"swapped-in").expect("recreate fixture");

        let cancel = Arc::new(AtomicBool::new(false));
        let job = DeleteJob {
            owner: owner(),
            path: &file,
            kind: TargetKind::File,
            identity: Some(identity),
            mode: DeleteMode::Permanent,
            cancel: &cancel,
        };
        assert_eq!(
            run_delete(&job),
            DestructiveOutcome::TargetChanged { reason: TargetChangeReason::IdentityMismatch }
        );
        assert_eq!(fs::read(&file).expect("fixture intact"), b"swapped-in");

        let cancelled = Arc::new(AtomicBool::new(true));
        let job = DeleteJob { cancel: &cancelled, identity: read_file_identity(&file).ok(), ..job };
        assert_eq!(run_delete(&job), DestructiveOutcome::CancelledBeforeMutation);
        assert!(file.exists());
    }

    #[test]
    fn win32_code_unwraps_only_facility_win32_hresults() {
        assert_eq!(win32_code(0x8007_0002_u32 as i32), 2);
        assert_eq!(win32_code(-1), -1);
        assert!(is_missing_code(2));
        assert!(is_missing_code(3));
        assert!(!is_missing_code(5));
    }
}
