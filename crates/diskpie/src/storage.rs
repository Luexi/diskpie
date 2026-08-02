//! eframe/RON adapter for the storage-neutral settings policy.

use diskpie_app::settings::{
    MAX_SERIALIZED_SETTINGS_BYTES, SETTINGS_SCHEMA_VERSION, SETTINGS_STORAGE_KEY, SettingsEnvelope,
    SettingsRead, SettingsStore, SettingsStoreError,
};
use eframe::Storage;
use serde::Deserialize;

#[derive(Deserialize)]
struct VersionHeader {
    schema_version: u32,
}

enum StorageAccess<'a> {
    ReadOnly(&'a dyn Storage),
    ReadWrite(&'a mut dyn Storage),
}

/// Thin adapter whose read and write capabilities mirror eframe's lifecycle.
pub struct EframeSettingsStore<'a> {
    access: StorageAccess<'a>,
}

impl<'a> EframeSettingsStore<'a> {
    pub const fn reader(storage: &'a dyn Storage) -> Self {
        Self { access: StorageAccess::ReadOnly(storage) }
    }

    pub const fn writer(storage: &'a mut dyn Storage) -> Self {
        Self { access: StorageAccess::ReadWrite(storage) }
    }

    fn storage(&self) -> &dyn Storage {
        match &self.access {
            StorageAccess::ReadOnly(storage) => *storage,
            StorageAccess::ReadWrite(storage) => &**storage,
        }
    }
}

impl SettingsStore for EframeSettingsStore<'_> {
    fn read(&mut self) -> SettingsRead {
        let Some(raw) = self.storage().get_string(SETTINGS_STORAGE_KEY) else {
            return SettingsRead::Missing;
        };
        if raw.len() > MAX_SERIALIZED_SETTINGS_BYTES {
            return SettingsRead::Oversized { bytes: raw.len() };
        }
        let Ok(header) = ron::from_str::<VersionHeader>(&raw) else {
            return SettingsRead::Malformed;
        };
        if header.schema_version != SETTINGS_SCHEMA_VERSION {
            return SettingsRead::UnsupportedVersion { found: header.schema_version };
        }
        ron::from_str::<SettingsEnvelope>(&raw)
            .map(SettingsRead::Envelope)
            .unwrap_or(SettingsRead::Malformed)
    }

    fn write(&mut self, envelope: &SettingsEnvelope) -> Result<(), SettingsStoreError> {
        if envelope.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(SettingsStoreError::new("settings.write.unsupported_version"));
        }
        let (validated, corrections) = envelope.settings.clone().validated();
        if !corrections.is_empty() || validated != envelope.settings {
            return Err(SettingsStoreError::new("settings.write.unvalidated"));
        }
        let encoded = ron::ser::to_string(envelope)
            .map_err(|_| SettingsStoreError::new("settings.write.encode"))?;
        if encoded.len() > MAX_SERIALIZED_SETTINGS_BYTES {
            return Err(SettingsStoreError::new("settings.write.oversized"));
        }
        match &mut self.access {
            StorageAccess::ReadOnly(_) => {
                Err(SettingsStoreError::new("settings.write.read_only_adapter"))
            }
            StorageAccess::ReadWrite(storage) => {
                storage.set_string(SETTINGS_STORAGE_KEY, encoded);
                Ok(())
            }
        }
    }
}

/// Adapter used when eframe persistence could not be created.
pub struct UnavailableSettingsStore;

impl SettingsStore for UnavailableSettingsStore {
    fn read(&mut self) -> SettingsRead {
        SettingsRead::Unavailable { code: "settings.storage.unavailable" }
    }

    fn write(&mut self, _envelope: &SettingsEnvelope) -> Result<(), SettingsStoreError> {
        Err(SettingsStoreError::new("settings.storage.unavailable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_app::settings::{Settings, SettingsSession, SettingsWarning};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemoryStorage(BTreeMap<String, String>);

    impl Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn round_trips_a_valid_current_envelope() {
        let mut memory = MemoryStorage::default();
        {
            let mut writer = EframeSettingsStore::writer(&mut memory);
            writer.write(&SettingsEnvelope::current(Settings::default())).expect("write settings");
        }
        let mut reader = EframeSettingsStore::reader(&memory);
        assert!(matches!(reader.read(), SettingsRead::Envelope(_)));
    }

    #[test]
    fn distinguishes_missing_malformed_oversized_and_newer_data() {
        let mut memory = MemoryStorage::default();
        let mut reader = EframeSettingsStore::reader(&memory);
        assert_eq!(reader.read(), SettingsRead::Missing);

        memory.set_string(SETTINGS_STORAGE_KEY, "not ron".to_owned());
        let mut reader = EframeSettingsStore::reader(&memory);
        assert_eq!(reader.read(), SettingsRead::Malformed);

        memory.set_string(SETTINGS_STORAGE_KEY, "x".repeat(MAX_SERIALIZED_SETTINGS_BYTES + 1));
        let mut reader = EframeSettingsStore::reader(&memory);
        assert_eq!(
            reader.read(),
            SettingsRead::Oversized { bytes: MAX_SERIALIZED_SETTINGS_BYTES + 1 }
        );

        memory.set_string(
            SETTINGS_STORAGE_KEY,
            r#"(schema_version:99,settings:"C:\\sensitive\\must-not-be-parsed")"#.to_owned(),
        );
        let mut reader = EframeSettingsStore::reader(&memory);
        assert_eq!(reader.read(), SettingsRead::UnsupportedVersion { found: 99 });
    }

    #[test]
    fn creation_read_access_cannot_accidentally_overwrite() {
        let memory = MemoryStorage::default();
        let mut reader = EframeSettingsStore::reader(&memory);
        assert_eq!(
            reader.write(&SettingsEnvelope::current(Settings::default())),
            Err(SettingsStoreError::new("settings.write.read_only_adapter"))
        );
    }

    #[test]
    fn unavailable_storage_is_nonfatal_and_protected() {
        let mut unavailable = UnavailableSettingsStore;
        let session = SettingsSession::load(&mut unavailable);
        assert_eq!(
            session.warning(),
            Some(SettingsWarning::Unavailable { code: "settings.storage.unavailable" })
        );
        assert!(!session.overwrite_allowed());
    }
}
