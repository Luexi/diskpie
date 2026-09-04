//! Native feedback for the executable's startup paths.
//!
//! Release builds link as a Windows GUI-subsystem process, so they start
//! without standard handles: `--help`, `--version`, usage errors, and fatal
//! startup failures would otherwise vanish. This adapter attaches the parent
//! console once for the CLI text protocol and shows a native message box for a
//! fatal failure. Callers pass path-free text only.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, sync::OnceLock};

use windows::{
    Win32::{
        Foundation::{ERROR_ACCESS_DENIED, GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
            STD_OUTPUT_HANDLE, SetStdHandle,
        },
        UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW},
    },
    core::PCWSTR,
};

use diskpie_core::PRODUCT_NAME;

static PARENT_CONSOLE: OnceLock<bool> = OnceLock::new();

/// Attaches the process to its parent's console once and reports whether a
/// console is available afterwards.
///
/// A standard handle the parent already redirected to a pipe or file is left
/// untouched; only an absent handle is pointed at the console output buffer.
/// That buffer handle is deliberately owned by the process standard-handle
/// table for the rest of the process lifetime. The call is idempotent and
/// returns `true` without touching anything when the process already owns a
/// console, which is the case for debug builds started from a terminal.
pub fn attach_parent_console() -> bool {
    *PARENT_CONSOLE.get_or_init(attach_once)
}

fn attach_once() -> bool {
    // SAFETY: plain system call with the documented sentinel argument; it
    // touches no caller memory.
    match unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } {
        Ok(()) => {}
        Err(error) if is_win32_error(&error, ERROR_ACCESS_DENIED.0) => return true,
        Err(_parent_has_no_console) => return false,
    }
    let Some(output) = console_output_handle() else {
        return true;
    };
    for slot in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if std_handle_missing(slot) {
            // SAFETY: `output` is a live console handle; the standard-handle
            // table takes it over and it is never closed by this module.
            let _replaced = unsafe { SetStdHandle(slot, output) };
        }
    }
    true
}

fn std_handle_missing(slot: STD_HANDLE) -> bool {
    // SAFETY: read-only query of the process standard-handle table.
    match unsafe { GetStdHandle(slot) } {
        Ok(handle) => handle.is_invalid(),
        Err(_error) => true,
    }
}

fn console_output_handle() -> Option<HANDLE> {
    let name = wide("CONOUT$");
    // SAFETY: `name` is NUL-terminated and outlives the call; no security
    // attributes or template handle are supplied.
    unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()
    .filter(|handle| !handle.is_invalid())
}

/// Shows a modal error box for a fatal startup failure.
///
/// The caller supplies path-free text. The box has no owner window because it
/// is only used before or after the native event loop exists.
pub fn show_startup_failure(text: &str) {
    let caption = wide(PRODUCT_NAME);
    let body = wide(text);
    // SAFETY: both buffers are NUL-terminated and live through the modal
    // call; a null owner window is documented as valid.
    let _button = unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        )
    };
}

fn is_win32_error(error: &windows::core::Error, code: u32) -> bool {
    let raw = error.code().0 as u32;
    raw & 0xFFFF_0000 == 0x8007_0000 && raw & 0xFFFF == code
}

/// Encodes UTF-16 with a terminating NUL and without interior NULs, which
/// would otherwise truncate the native text.
fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().filter(|unit| *unit != 0).chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated_without_interior_nuls() {
        assert_eq!(wide("ab"), vec![b'a' as u16, b'b' as u16, 0]);
        assert_eq!(wide("a\0b"), vec![b'a' as u16, b'b' as u16, 0]);
        assert_eq!(wide(""), vec![0]);
    }

    #[test]
    fn win32_error_classification_requires_the_win32_facility() {
        let access_denied =
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0005_u32 as i32));
        assert!(is_win32_error(&access_denied, ERROR_ACCESS_DENIED.0));
        let other_facility =
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8004_0005_u32 as i32));
        assert!(!is_win32_error(&other_facility, ERROR_ACCESS_DENIED.0));
    }

    #[test]
    fn attaching_in_a_console_process_is_idempotent_and_keeps_existing_handles() {
        // SAFETY: read-only query.
        let before = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.ok();
        let first = attach_parent_console();
        let second = attach_parent_console();
        assert_eq!(first, second);
        // SAFETY: read-only query.
        let after = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.ok();
        assert_eq!(before.map(|handle| handle.0 as usize), after.map(|handle| handle.0 as usize));
    }
}
