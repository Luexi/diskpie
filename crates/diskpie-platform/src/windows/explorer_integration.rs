//! Windows registry adapter for the reversible per-user Explorer verb.
//!
//! This is the [`IntegrationRegistry`] port of ADR 0009 implemented over
//! `windows-registry`'s RAII `Key`. Every key path is relative to one root
//! opened below `HKEY_CURRENT_USER`: `Software\Classes` in production and a
//! disposable `Software\DiskPieTest\<run id>\Classes` subtree in tests, so
//! the real Classes tree is never touched by the test suite.
//!
//! Ownership checks, staging, and the exact-subtree deletion policy stay in
//! `diskpie_app::explorer_integration`; this module only performs the narrow
//! registry operations it is asked for and never widens them. The current
//! executable path is a parameter of the policy layer, resolved by the
//! composition root, not here.
//!
//! Staging keys become final through `RegRenameKey` (the documented
//! `winreg.h` sibling rename, available since Windows Vista). It is verified
//! against the real per-user registry by this module's tests; a build where
//! it fails surfaces as a sanitized `Rename` error with a clean rollback
//! rather than a partially written verb.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    os::windows::ffi::{OsStrExt, OsStringExt},
};

use diskpie_app::explorer_integration::{
    IntegrationRegistry, KeyPath, KeySnapshot, RegistryError, RegistryErrorKind, RegistryOperation,
    RegistryValue,
};
use windows::{
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_CALL_NOT_IMPLEMENTED,
            ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_KEY_DELETED, ERROR_NOT_SUPPORTED,
            ERROR_PATH_NOT_FOUND,
        },
        UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify},
    },
    core::HRESULT,
};
use windows_registry::{CURRENT_USER, HSTRING, Key, Type};

/// Production integration root below `HKEY_CURRENT_USER`.
pub const CLASSES_ROOT_PATH: &str = r"Software\Classes";

/// Registry port bound to one per-user root key.
pub struct WindowsIntegrationRegistry {
    root: Key,
}

impl fmt::Debug for WindowsIntegrationRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WindowsIntegrationRegistry").finish_non_exhaustive()
    }
}

impl WindowsIntegrationRegistry {
    /// Opens `HKCU\Software\Classes`, the only production root.
    pub fn open_current_user_classes() -> Result<Self, RegistryError> {
        Self::open_under_current_user(CLASSES_ROOT_PATH)
    }

    /// Opens or creates a root below `HKEY_CURRENT_USER` with read and write
    /// access. Nothing outside the current user's hive is ever addressed.
    pub fn open_under_current_user(relative_path: &str) -> Result<Self, RegistryError> {
        let root = CURRENT_USER
            .options()
            .read()
            .write()
            .create()
            .open(relative_path)
            .map_err(|error| map_error(RegistryOperation::Create, &error))?;
        Ok(Self { root })
    }

    fn open_read(&self, key: &KeyPath) -> Result<Option<Key>, RegistryError> {
        match self.root.options().read().open(key.to_native()) {
            Ok(key) => Ok(Some(key)),
            Err(error) => {
                let mapped = map_error(RegistryOperation::Read, &error);
                if mapped.kind == RegistryErrorKind::NotFound { Ok(None) } else { Err(mapped) }
            }
        }
    }

    fn open_write(
        &self,
        operation: RegistryOperation,
        key: &KeyPath,
    ) -> Result<Key, RegistryError> {
        self.root
            .options()
            .read()
            .write()
            .open(key.to_native())
            .map_err(|error| map_error(operation, &error))
    }
}

impl IntegrationRegistry for WindowsIntegrationRegistry {
    fn key_exists(&self, key: &KeyPath) -> Result<bool, RegistryError> {
        self.open_read(key)
            .map(|key| key.is_some())
            .map_err(|error| RegistryError { operation: RegistryOperation::Exists, ..error })
    }

    fn read_key(&self, key: &KeyPath) -> Result<Option<KeySnapshot>, RegistryError> {
        let Some(key) = self.open_read(key)? else {
            return Ok(None);
        };
        let mut snapshot = KeySnapshot::new();
        let values = key.values().map_err(|error| map_error(RegistryOperation::Read, &error))?;
        for (name, value) in values {
            let observed = if value.ty() == Type::String {
                RegistryValue::String(OsString::from_wide(trim_nul(value.as_wide())))
            } else {
                RegistryValue::Other { type_id: u32::from(value.ty()) }
            };
            snapshot.insert_value(&name, observed);
        }
        let subkeys = key.keys().map_err(|error| map_error(RegistryOperation::Read, &error))?;
        for name in subkeys {
            snapshot.add_subkey(&name);
        }
        Ok(Some(snapshot))
    }

    fn create_key(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
        self.root
            .options()
            .read()
            .write()
            .create()
            .open(key.to_native())
            .map(drop)
            .map_err(|error| map_error(RegistryOperation::Create, &error))
    }

    fn write_string(
        &mut self,
        key: &KeyPath,
        name: &str,
        value: &OsStr,
    ) -> Result<(), RegistryError> {
        let key = self.open_write(RegistryOperation::Write, key)?;
        let units: Vec<u16> = value.encode_wide().collect();
        key.set_hstring(name, &HSTRING::from_wide(&units))
            .map_err(|error| map_error(RegistryOperation::Write, &error))
    }

    fn rename_key(&mut self, key: &KeyPath, new_name: &str) -> Result<(), RegistryError> {
        let Some(parent) = key.parent() else {
            return Err(RegistryError::new(
                RegistryOperation::Rename,
                RegistryErrorKind::Unsupported,
            ));
        };
        let parent = self.open_write(RegistryOperation::Rename, &parent)?;
        parent
            .rename(key.name(), new_name)
            .map_err(|error| map_error(RegistryOperation::Rename, &error))
    }

    fn delete_tree(&mut self, key: &KeyPath) -> Result<(), RegistryError> {
        match self.root.remove_tree(key.to_native()) {
            Ok(()) => Ok(()),
            Err(error) => {
                let mapped = map_error(RegistryOperation::Delete, &error);
                if mapped.kind == RegistryErrorKind::NotFound { Ok(()) } else { Err(mapped) }
            }
        }
    }

    fn notify_association_changed(&mut self) {
        // SAFETY: SHCNE_ASSOCCHANGED with SHCNF_IDLIST takes no item
        // pointers; both are passed as null, exactly as documented.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    }
}

fn trim_nul(mut units: &[u16]) -> &[u16] {
    while units.last() == Some(&0) {
        units = &units[..units.len() - 1];
    }
    units
}

fn map_error(operation: RegistryOperation, error: &windows::core::Error) -> RegistryError {
    let hresult = error.code().0;
    RegistryError::new(operation, error_kind_from_hresult(hresult)).with_code(hresult)
}

fn is_win32(hresult: i32, code: u32) -> bool {
    hresult == HRESULT::from_win32(code).0
}

fn error_kind_from_hresult(hresult: i32) -> RegistryErrorKind {
    if is_win32(hresult, ERROR_ACCESS_DENIED.0) {
        RegistryErrorKind::AccessDenied
    } else if is_win32(hresult, ERROR_FILE_NOT_FOUND.0)
        || is_win32(hresult, ERROR_PATH_NOT_FOUND.0)
        || is_win32(hresult, ERROR_KEY_DELETED.0)
    {
        RegistryErrorKind::NotFound
    } else if is_win32(hresult, ERROR_ALREADY_EXISTS.0) || is_win32(hresult, ERROR_FILE_EXISTS.0) {
        RegistryErrorKind::AlreadyExists
    } else if is_win32(hresult, ERROR_CALL_NOT_IMPLEMENTED.0)
        || is_win32(hresult, ERROR_NOT_SUPPORTED.0)
    {
        RegistryErrorKind::Unsupported
    } else {
        RegistryErrorKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use diskpie_app::explorer_integration::{
        COMMAND_KEY_NAME, DEFAULT_VALUE_NAME, EXECUTABLE_VALUE, ICON_VALUE, IntegrationError,
        IntegrationOutcome, IntegrationRequest, IntegrationState, KeyState, MUI_VERB_VALUE,
        MULTI_SELECT_MODEL_VALUE, OWNER_MARKER, OWNER_VALUE, SCHEMA_VALUE, StagingToken,
        VERB_KEY_NAME, VERB_LABEL, VerbRecord, VerbTarget, apply, command_key, inspect, shell_key,
        staging_verb_key, verb_key,
    };
    use windows::{
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            UI::Shell::CommandLineToArgvW,
        },
        core::PCWSTR,
    };

    const TEST_ROOT_PARENT: &str = r"Software\DiskPieTest";
    const EXE: &str = r"C:\Tools\🍕 100% & ^caret, comma\-leading dash\diskpie.exe";
    const MOVED_EXE: &str = r"D:\Portable\diskpie.exe";

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// Unique per-test root that removes only its own subtree on drop.
    struct TestRoot {
        relative_path: String,
        registry: Option<WindowsIntegrationRegistry>,
    }

    impl TestRoot {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let relative_path = format!(
                "{TEST_ROOT_PARENT}\\{label}-{}-{}-{nanos}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            assert!(
                CURRENT_USER.open(&relative_path).is_err(),
                "the unique test root must not exist before the test"
            );
            let registry = WindowsIntegrationRegistry::open_under_current_user(&format!(
                "{relative_path}\\Classes"
            ))
            .expect("a standard user can create a test root below HKCU");
            Self { relative_path, registry: Some(registry) }
        }

        fn registry(&mut self) -> &mut WindowsIntegrationRegistry {
            self.registry.as_mut().expect("registry is open until drop")
        }

        fn classes(&self) -> Key {
            CURRENT_USER
                .options()
                .read()
                .write()
                .open(format!("{}\\Classes", self.relative_path))
                .expect("the test Classes root exists")
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            // Close the root handle first so the deletion is observable
            // through a fresh open, then delete exactly this run's subtree.
            self.registry = None;
            let removed = CURRENT_USER.remove_tree(&self.relative_path);
            let gone = CURRENT_USER.open(&self.relative_path).is_err();
            if std::thread::panicking() {
                if removed.is_err() || !gone {
                    eprintln!("test root cleanup incomplete for {}", self.relative_path);
                }
            } else {
                removed.expect("the test root is removable by its creator");
                assert!(gone, "the test root no longer opens after removal");
            }
        }
    }

    type RawTree = BTreeMap<String, (BTreeMap<String, (u32, Vec<u8>)>, Vec<String>)>;

    fn dump(key: &Key, path: &str, out: &mut RawTree) {
        let mut values = BTreeMap::new();
        for (name, value) in key.values().expect("values enumerate") {
            values.insert(name, (u32::from(value.ty()), value.to_vec()));
        }
        let mut subkeys: Vec<String> = key.keys().expect("keys enumerate").collect();
        subkeys.sort();
        out.insert(path.to_owned(), (values, subkeys.clone()));
        for name in subkeys {
            let child = key.open(&name).expect("child opens");
            dump(&child, &format!("{path}\\{name}"), out);
        }
    }

    fn raw_tree(root: &TestRoot) -> RawTree {
        let mut out = RawTree::new();
        dump(&root.classes(), "Classes", &mut out);
        out
    }

    fn raw_string(key: &Key, name: &str) -> OsString {
        assert_eq!(key.get_type(name).expect("value exists"), Type::String, "{name} is REG_SZ");
        let value = key.get_value(name).expect("value reads");
        OsString::from_wide(trim_nul(value.as_wide()))
    }

    fn create_adjacent(root: &TestRoot) -> String {
        let classes = root.classes();
        let adjacent = classes.create(r"Directory\shell\Adjacent.Verb").expect("create");
        adjacent.set_string(MUI_VERB_VALUE, "Adjacent").expect("set");
        adjacent.set_u32("Flags", 7).expect("set");
        let command = adjacent.create(COMMAND_KEY_NAME).expect("create");
        command.set_string(DEFAULT_VALUE_NAME, r#""C:\other.exe" "%1""#).expect("set");
        classes.create(r"Drive\shell\Adjacent.Drive").expect("create");
        r"Classes\Directory\shell\Adjacent.Verb".to_owned()
    }

    fn token(sequence: u64) -> StagingToken {
        StagingToken::new(std::process::id(), sequence)
    }

    fn assert_installed_for(root: &TestRoot, exe: &str) {
        let record = VerbRecord::new(Path::new(exe)).expect("valid executable");
        let classes = root.classes();
        for target in VerbTarget::ALL {
            let verb = classes.open(verb_key(target).to_native()).expect("verb key exists");
            assert_eq!(raw_string(&verb, MUI_VERB_VALUE), OsString::from(VERB_LABEL));
            assert_eq!(raw_string(&verb, ICON_VALUE), OsString::from(format!("\"{exe}\",0")));
            assert_eq!(raw_string(&verb, MULTI_SELECT_MODEL_VALUE), OsString::from("Single"));
            assert_eq!(raw_string(&verb, OWNER_VALUE), OsString::from(OWNER_MARKER));
            assert_eq!(raw_string(&verb, SCHEMA_VALUE), OsString::from("1"));
            assert_eq!(raw_string(&verb, EXECUTABLE_VALUE), OsString::from(exe));
            let names: Vec<String> = verb.values().expect("values").map(|(name, _)| name).collect();
            assert_eq!(names.len(), 6, "exactly the owned values: {names:?}");
            let subkeys: Vec<String> = verb.keys().expect("keys").collect();
            assert_eq!(subkeys, [COMMAND_KEY_NAME]);
            let command = classes.open(command_key(target).to_native()).expect("command exists");
            assert_eq!(raw_string(&command, DEFAULT_VALUE_NAME), record.command_line(target));
            assert_eq!(command.keys().expect("keys").count(), 0);
        }
    }

    fn shell_subkeys(root: &TestRoot, target: VerbTarget) -> Vec<String> {
        let mut names: Vec<String> = root
            .classes()
            .open(shell_key(target).to_native())
            .expect("shell exists")
            .keys()
            .expect("keys")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn install_writes_exact_reg_sz_keys_and_round_trips_native_text() {
        let mut root = TestRoot::new("install");
        create_adjacent(&root);
        let report = apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(1))
            .expect("install succeeds for a standard user");
        assert_eq!(report.outcome, IntegrationOutcome::Installed);
        assert_eq!(report.incomplete_cleanup, None);
        assert_installed_for(&root, EXE);
        assert_eq!(shell_subkeys(&root, VerbTarget::Directory), ["Adjacent.Verb", VERB_KEY_NAME]);
        assert_eq!(shell_subkeys(&root, VerbTarget::Drive), ["Adjacent.Drive", VERB_KEY_NAME]);
        let status = inspect(root.registry(), Path::new(EXE)).expect("inspect");
        assert!(status.is_installed());
        assert!(status.strays().is_empty());
    }

    #[test]
    fn repeated_install_is_idempotent_and_leaves_the_tree_byte_identical() {
        let mut root = TestRoot::new("idempotent");
        create_adjacent(&root);
        apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(1))
            .expect("first install");
        let before = raw_tree(&root);
        let report = apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(2))
            .expect("second install");
        assert_eq!(report.outcome, IntegrationOutcome::AlreadyInstalled);
        assert_eq!(raw_tree(&root), before);
        let report = apply(root.registry(), IntegrationRequest::Repair, Path::new(EXE), &token(3))
            .expect("repair of a current install");
        assert_eq!(report.outcome, IntegrationOutcome::AlreadyInstalled);
        assert_eq!(raw_tree(&root), before);
    }

    #[test]
    fn moved_executable_is_reported_stale_without_mutation_and_repair_replaces_it() {
        let mut root = TestRoot::new("stale");
        create_adjacent(&root);
        apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(1))
            .expect("install");
        let before = raw_tree(&root);

        let status = inspect(root.registry(), Path::new(MOVED_EXE)).expect("inspect");
        assert_eq!(
            status.state(),
            &IntegrationState::OwnedStale {
                stored_executable: PathBuf::from(EXE),
                current_executable: PathBuf::from(MOVED_EXE),
            }
        );
        assert_eq!(
            status.key_state(VerbTarget::Drive),
            &KeyState::OwnedStale { stored_executable: PathBuf::from(EXE) }
        );
        let error =
            apply(root.registry(), IntegrationRequest::Install, Path::new(MOVED_EXE), &token(2))
                .expect_err("install refuses a stale integration");
        assert!(matches!(error, IntegrationError::StaleRequiresDecision(_)));
        assert_eq!(raw_tree(&root), before, "refusal does not mutate");

        let report =
            apply(root.registry(), IntegrationRequest::Repair, Path::new(MOVED_EXE), &token(3))
                .expect("repair succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Repaired);
        assert_eq!(report.incomplete_cleanup, None);
        assert_installed_for(&root, MOVED_EXE);
        assert_eq!(shell_subkeys(&root, VerbTarget::Directory), ["Adjacent.Verb", VERB_KEY_NAME]);
        assert_eq!(shell_subkeys(&root, VerbTarget::Drive), ["Adjacent.Drive", VERB_KEY_NAME]);
        let adjacent = r"Classes\Directory\shell\Adjacent.Verb";
        let after = raw_tree(&root);
        assert_eq!(after[adjacent], before[adjacent]);
        assert_eq!(
            after[r"Classes\Directory\shell\Adjacent.Verb\command"],
            before[r"Classes\Directory\shell\Adjacent.Verb\command"]
        );
    }

    #[test]
    fn foreign_key_reports_conflict_without_overwrite() {
        let mut root = TestRoot::new("foreign");
        let classes = root.classes();
        let foreign = classes.create(verb_key(VerbTarget::Directory).to_native()).expect("create");
        foreign.set_string(MUI_VERB_VALUE, "Someone else's scan").expect("set");
        foreign
            .create(COMMAND_KEY_NAME)
            .expect("create")
            .set_string(DEFAULT_VALUE_NAME, r#""C:\other.exe" "%1""#)
            .expect("set");
        drop(foreign);
        drop(classes);
        let before = raw_tree(&root);

        for request in
            [IntegrationRequest::Install, IntegrationRequest::Repair, IntegrationRequest::Remove]
        {
            let error = apply(root.registry(), request, Path::new(EXE), &token(1))
                .expect_err("foreign key refuses");
            let IntegrationError::Conflict(status) = error else {
                panic!("expected conflict, got {error:?}");
            };
            assert_eq!(status.state(), &IntegrationState::Foreign);
            assert_eq!(status.key_state(VerbTarget::Directory), &KeyState::Foreign);
            assert_eq!(status.key_state(VerbTarget::Drive), &KeyState::NotInstalled);
            assert_eq!(raw_tree(&root), before, "{request:?} does not mutate");
        }
        assert!(
            root.classes().open(shell_key(VerbTarget::Drive).to_native()).is_err(),
            "no Drive key is created while refusing"
        );
    }

    #[test]
    fn remove_deletes_only_owned_subtrees_and_is_idempotent() {
        let mut root = TestRoot::new("remove");
        let adjacent = create_adjacent(&root);
        let untouched = raw_tree(&root);
        apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(1))
            .expect("install");

        let report = apply(root.registry(), IntegrationRequest::Remove, Path::new(EXE), &token(2))
            .expect("remove succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::Removed);
        let after = raw_tree(&root);
        for (path, content) in &untouched {
            let is_shell = path.ends_with("\\shell");
            if is_shell {
                assert_eq!(after[path].0, content.0, "{path} values unchanged");
                assert_eq!(after[path].1, content.1, "{path} subkeys restored");
            } else {
                assert_eq!(after.get(path), Some(content), "{path} unchanged");
            }
        }
        assert_eq!(after[&adjacent], untouched[&adjacent]);
        assert!(after.keys().all(|path| !path.contains(VERB_KEY_NAME)));
        assert!(after.contains_key(r"Classes\Directory\shell"));
        assert!(after.contains_key(r"Classes\Drive\shell"));
        assert!(after.contains_key(r"Classes\Directory"));
        assert!(after.contains_key(r"Classes\Drive"));
        assert_eq!(
            inspect(root.registry(), Path::new(EXE)).expect("inspect").state(),
            &IntegrationState::NotInstalled
        );

        let report = apply(root.registry(), IntegrationRequest::Remove, Path::new(EXE), &token(3))
            .expect("repeated remove succeeds");
        assert_eq!(report.outcome, IntegrationOutcome::AlreadyRemoved);
        assert_eq!(raw_tree(&root), after);
    }

    #[test]
    fn adjacent_keys_are_byte_identical_across_install_repair_and_remove() {
        let mut root = TestRoot::new("adjacent");
        let adjacent = create_adjacent(&root);
        let adjacent_command = format!("{adjacent}\\{COMMAND_KEY_NAME}");
        let drive_adjacent = r"Classes\Drive\shell\Adjacent.Drive";
        let snapshot = |root: &TestRoot| {
            let tree = raw_tree(root);
            (tree[&adjacent].clone(), tree[&adjacent_command].clone(), tree[drive_adjacent].clone())
        };
        let before = snapshot(&root);
        apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(1))
            .expect("install");
        assert_eq!(snapshot(&root), before);
        apply(root.registry(), IntegrationRequest::Repair, Path::new(MOVED_EXE), &token(2))
            .expect("repair");
        assert_eq!(snapshot(&root), before);
        apply(root.registry(), IntegrationRequest::Remove, Path::new(MOVED_EXE), &token(3))
            .expect("remove");
        assert_eq!(snapshot(&root), before);
    }

    #[test]
    fn leftover_staging_sibling_is_visible_as_a_stray_and_removed() {
        let mut root = TestRoot::new("stray");
        apply(root.registry(), IntegrationRequest::Install, Path::new(EXE), &token(1))
            .expect("install");
        let stray = staging_verb_key(VerbTarget::Directory, &token(99));
        let classes = root.classes();
        let stray_key = classes.create(stray.to_native()).expect("create");
        stray_key.set_string(OWNER_VALUE, OWNER_MARKER).expect("set");
        stray_key.set_string(SCHEMA_VALUE, "1").expect("set");
        stray_key.set_string(EXECUTABLE_VALUE, EXE).expect("set");
        drop(stray_key);
        drop(classes);

        let status = inspect(root.registry(), Path::new(EXE)).expect("inspect");
        assert!(status.is_installed());
        assert_eq!(status.strays(), std::slice::from_ref(&stray));
        let report = apply(root.registry(), IntegrationRequest::Remove, Path::new(EXE), &token(2))
            .expect("remove");
        assert_eq!(report.outcome, IntegrationOutcome::Removed);
        assert!(root.classes().open(stray.to_native()).is_err());
        assert_eq!(shell_subkeys(&root, VerbTarget::Directory), Vec::<String>::new());
    }

    #[test]
    fn port_operations_are_exact_and_errors_carry_no_paths() {
        let mut root = TestRoot::new("port");
        let registry = root.registry();
        let key = KeyPath::try_new(["Directory", "shell", "Probe"]).expect("path");
        assert_eq!(registry.key_exists(&key), Ok(false));
        assert_eq!(registry.read_key(&key), Ok(None));
        assert_eq!(registry.delete_tree(&key), Ok(()), "missing deletion is idempotent");
        registry.create_key(&key.child("command")).expect("create with parents");
        assert_eq!(registry.key_exists(&key), Ok(true));

        let mut native = OsString::from(EXE);
        native.push(OsString::from_wide(&[0xD800]));
        registry.write_string(&key, "Native", &native).expect("write");
        let snapshot = registry.read_key(&key).expect("read").expect("exists");
        assert_eq!(snapshot.string_value("native"), Some(native.as_os_str()));
        assert_eq!(snapshot.subkeys(), ["command"]);
        let probe = root.classes().options().read().write().open(key.to_native()).expect("open");
        probe.set_u32("Number", 5).expect("set");
        drop(probe);
        let snapshot = root.registry().read_key(&key).expect("read").expect("exists");
        assert_eq!(snapshot.value("Number"), Some(&RegistryValue::Other { type_id: 4 }));

        let registry = root.registry();
        registry.rename_key(&key, "Renamed").expect("RegRenameKey works below HKCU");
        let renamed = key.with_name("Renamed");
        assert_eq!(registry.key_exists(&key), Ok(false));
        assert_eq!(registry.key_exists(&renamed.child("command")), Ok(true));
        registry.create_key(&key).expect("create");
        let collision = registry.rename_key(&renamed, "Probe").expect_err("target exists");
        assert_eq!(collision.operation, RegistryOperation::Rename);
        assert!(collision.code.is_some());
        let rendered = format!("{collision:?} {collision}");
        assert!(!rendered.contains("Probe"));
        assert!(!rendered.contains("Directory"));

        let missing = KeyPath::try_new(["Directory", "Nowhere"]).expect("path");
        let error = registry.rename_key(&missing, "Else").expect_err("missing source");
        assert_eq!(error.kind, RegistryErrorKind::NotFound);
        let error = registry.write_string(&missing, "x", OsStr::new("y")).expect_err("missing key");
        assert_eq!(
            (error.operation, error.kind),
            (RegistryOperation::Write, RegistryErrorKind::NotFound)
        );
        registry.delete_tree(&renamed).expect("delete");
        assert_eq!(registry.key_exists(&renamed.child("command")), Ok(false));
        assert_eq!(registry.key_exists(&key), Ok(true), "sibling survives exact deletion");
    }

    fn windows_argv(line: &OsStr) -> Vec<OsString> {
        let wide: Vec<u16> = line.encode_wide().chain(std::iter::once(0)).collect();
        let mut count = 0_i32;
        // SAFETY: `wide` is NUL-terminated and outlives the call; `count` is
        // a live out-parameter.
        let argv = unsafe { CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count) };
        assert!(!argv.is_null(), "CommandLineToArgvW parses the line");
        let mut arguments = Vec::new();
        for index in 0..usize::try_from(count).expect("non-negative count") {
            // SAFETY: `argv` holds `count` valid NUL-terminated entries until
            // it is freed below.
            let units = unsafe { (*argv.add(index)).as_wide() };
            arguments.push(OsString::from_wide(units));
        }
        // SAFETY: `argv` was allocated by CommandLineToArgvW and is freed once.
        unsafe { LocalFree(Some(HLOCAL(argv.cast()))) };
        arguments
    }

    #[test]
    fn command_templates_round_trip_selected_paths_through_the_windows_argv_parser() {
        let record = VerbRecord::new(Path::new(EXE)).expect("valid executable");
        let directories = [
            r"C:\Data with spaces\100% & ready",
            r"C:\🍕 pie\-leading dash",
            r"C:\a^b,c\--not-an-option",
            r"\\server\share\folder",
        ];
        for selected in directories {
            let line = OsString::from(
                record
                    .command_line(VerbTarget::Directory)
                    .to_string_lossy()
                    .replace("%1", selected),
            );
            assert_eq!(
                windows_argv(&line),
                [OsString::from(EXE), OsString::from("--scan-path"), OsString::from(selected)],
                "{selected}"
            );
        }
        for drive in [r"C:\", r"Z:\"] {
            let line = OsString::from(
                record.command_line(VerbTarget::Drive).to_string_lossy().replace("%1", drive),
            );
            assert_eq!(
                windows_argv(&line),
                [OsString::from(EXE), OsString::from("--scan-path"), OsString::from(drive)],
                "{drive}"
            );
        }
        // Evidence for the drive template: the naive quoting corrupts a root.
        let naive = OsString::from(
            record.command_line(VerbTarget::Directory).to_string_lossy().replace("%1", r"C:\"),
        );
        assert_eq!(windows_argv(&naive).last(), Some(&OsString::from(r#"C:""#)));
    }
}
