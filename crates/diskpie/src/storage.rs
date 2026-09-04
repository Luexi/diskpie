//! Bounded RON document adapter for the storage-neutral settings policy.
//!
//! Filesystem access belongs to a platform adapter. This module only decodes
//! bytes read before the native event loop and stages bytes for publication
//! after the UI has released its settings session.

use diskpie_app::settings::{
    MAX_SERIALIZED_SETTINGS_BYTES, SETTINGS_SCHEMA_VERSION, SettingsEnvelope, SettingsRead,
    SettingsStore, SettingsStoreError,
};

const SETTINGS_RON_RECURSION_LIMIT: usize = 32;
const SCHEMA_VERSION_FIELD: &[u8] = b"schema_version";

/// One-shot document store used to bridge pure settings policy and native I/O.
///
/// A source document is consumed by the first [`SettingsStore::read`]. A
/// successful write stages one complete replacement document in memory; it
/// never touches the filesystem or blocks an egui callback.
pub struct SettingsDocumentStore {
    initial: Option<SettingsRead>,
    pending: Option<Box<[u8]>>,
}

impl SettingsDocumentStore {
    /// Builds a store from a native adapter's already bounded read outcome.
    ///
    /// This keeps oversized and unavailable files out of the parser without
    /// requiring the platform layer to construct a fake byte buffer.
    #[must_use]
    pub fn from_read(initial: SettingsRead) -> Self {
        Self { initial: Some(initial), pending: None }
    }

    /// Builds a reader from an optional bounded source document.
    #[must_use]
    pub fn from_bytes(source: Option<&[u8]>) -> Self {
        Self::from_read(source.map_or(SettingsRead::Missing, decode_document))
    }

    /// Builds a store whose read result describes an unavailable native path.
    #[must_use]
    pub fn unavailable(code: &'static str) -> Self {
        Self::from_read(SettingsRead::Unavailable { code })
    }

    /// Returns the staged whole-file document, leaving no pending write.
    pub fn take_pending(&mut self) -> Option<Box<[u8]>> {
        self.pending.take()
    }

    /// Borrows the staged document for diagnostics and tests.
    #[must_use]
    pub fn pending(&self) -> Option<&[u8]> {
        self.pending.as_deref()
    }
}

impl SettingsStore for SettingsDocumentStore {
    fn read(&mut self) -> SettingsRead {
        self.initial.take().unwrap_or(SettingsRead::Missing)
    }

    fn write(&mut self, envelope: &SettingsEnvelope) -> Result<(), SettingsStoreError> {
        // A failed newer request must never leave an older document looking
        // like the successful result of that request.
        self.pending = None;
        self.pending = Some(encode_document(envelope)?);
        Ok(())
    }
}

/// Classifies one bounded native document without trusting its payload.
///
/// The native adapter hands over bytes it has already bounded; this is the
/// only place that turns them into a policy-level read outcome.
#[must_use]
pub fn decode_document(raw: &[u8]) -> SettingsRead {
    if raw.len() > MAX_SERIALIZED_SETTINGS_BYTES {
        return SettingsRead::Oversized { bytes: raw.len() };
    }
    let Some(schema_version) = parse_schema_version(raw) else {
        return SettingsRead::Malformed;
    };
    if schema_version != SETTINGS_SCHEMA_VERSION {
        return SettingsRead::UnsupportedVersion { found: schema_version };
    }
    let options = ron::Options::default().with_recursion_limit(SETTINGS_RON_RECURSION_LIMIT);
    options
        .from_bytes::<SettingsEnvelope>(raw)
        .map(SettingsRead::Envelope)
        .unwrap_or(SettingsRead::Malformed)
}

/// Reads only the canonical leading version field and never traverses the
/// untrusted settings payload. Documents emitted by [`encode_document`] always
/// use this field order; noncanonical input remains overwrite-protected as
/// malformed data.
fn parse_schema_version(raw: &[u8]) -> Option<u32> {
    let mut cursor = 0;
    skip_ascii_whitespace(raw, &mut cursor);
    if raw.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor += 1;
    skip_ascii_whitespace(raw, &mut cursor);
    if !raw.get(cursor..)?.starts_with(SCHEMA_VERSION_FIELD) {
        return None;
    }
    cursor += SCHEMA_VERSION_FIELD.len();
    skip_ascii_whitespace(raw, &mut cursor);
    if raw.get(cursor) != Some(&b':') {
        return None;
    }
    cursor += 1;
    skip_ascii_whitespace(raw, &mut cursor);

    let start = cursor;
    let mut version = 0_u32;
    while let Some(digit) = raw.get(cursor).copied().filter(u8::is_ascii_digit) {
        version = version.checked_mul(10)?.checked_add(u32::from(digit - b'0'))?;
        cursor += 1;
    }
    if cursor == start {
        return None;
    }
    skip_ascii_whitespace(raw, &mut cursor);
    matches!(raw.get(cursor), Some(b',') | Some(b')')).then_some(version)
}

fn skip_ascii_whitespace(raw: &[u8], cursor: &mut usize) {
    while raw.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
        *cursor += 1;
    }
}

fn encode_document(envelope: &SettingsEnvelope) -> Result<Box<[u8]>, SettingsStoreError> {
    if envelope.schema_version != SETTINGS_SCHEMA_VERSION {
        return Err(SettingsStoreError::new("settings.write.unsupported_version"));
    }
    let (validated, corrections) = envelope.settings.clone().validated();
    if !corrections.is_empty() || validated != envelope.settings {
        return Err(SettingsStoreError::new("settings.write.unvalidated"));
    }
    let encoded = ron::Options::default()
        .with_recursion_limit(SETTINGS_RON_RECURSION_LIMIT)
        .to_string(envelope)
        .map_err(|_| SettingsStoreError::new("settings.write.encode"))?;
    if encoded.len() > MAX_SERIALIZED_SETTINGS_BYTES {
        return Err(SettingsStoreError::new("settings.write.oversized"));
    }
    Ok(encoded.into_bytes().into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_app::settings::{Settings, SettingsSession, SettingsWarning};

    #[test]
    fn round_trips_a_valid_current_envelope() {
        let envelope = SettingsEnvelope::current(Settings::default());
        let encoded = encode_document(&envelope).expect("encode settings");
        assert_eq!(decode_document(&encoded), SettingsRead::Envelope(envelope));
    }

    #[test]
    fn distinguishes_missing_malformed_oversized_and_newer_data() {
        let mut missing = SettingsDocumentStore::from_bytes(None);
        assert_eq!(missing.read(), SettingsRead::Missing);

        assert_eq!(decode_document(b"not ron"), SettingsRead::Malformed);
        assert_eq!(decode_document(b""), SettingsRead::Malformed);

        let oversized = vec![b'x'; MAX_SERIALIZED_SETTINGS_BYTES + 1];
        assert_eq!(
            decode_document(&oversized),
            SettingsRead::Oversized { bytes: MAX_SERIALIZED_SETTINGS_BYTES + 1 }
        );

        let newer = br#"(schema_version:99,settings:"C:\\sensitive\\must-not-be-parsed")"#;
        assert_eq!(decode_document(newer), SettingsRead::UnsupportedVersion { found: 99 });
    }

    #[test]
    fn newer_version_rejection_never_traverses_its_payload() {
        let nesting = SETTINGS_RON_RECURSION_LIMIT + 64;
        let mut newer = String::from("(schema_version:99,settings:");
        newer.push_str(&"[".repeat(nesting));
        newer.push('0');
        newer.push_str(&"]".repeat(nesting));
        newer.push(')');

        assert!(newer.len() < MAX_SERIALIZED_SETTINGS_BYTES);
        assert_eq!(
            decode_document(newer.as_bytes()),
            SettingsRead::UnsupportedVersion { found: 99 }
        );
    }

    #[test]
    fn schema_header_is_canonical_bounded_and_overflow_checked() {
        assert_eq!(parse_schema_version(b" \r\n( schema_version : 1 , settings:())"), Some(1));
        assert_eq!(parse_schema_version(b"(settings:(),schema_version:1)"), None);
        assert_eq!(parse_schema_version(b"(schema_version_extra:1)"), None);
        assert_eq!(parse_schema_version(b"(schema_version:4294967296,settings:())"), None);
    }

    #[test]
    fn malformed_current_documents_never_stage_a_replacement_without_edit() {
        let original = b"(schema_version:1,settings:(";
        let mut store = SettingsDocumentStore::from_bytes(Some(original));
        let mut session = SettingsSession::load(&mut store);

        assert_eq!(session.warning(), Some(SettingsWarning::Malformed));
        assert!(!session.overwrite_allowed());
        assert!(session.save(&mut store).is_ok());
        assert!(store.pending().is_none());
    }

    #[test]
    fn untouched_newer_documents_never_stage_a_downgrade() {
        let original = b"(schema_version:99,settings:(future_value:42))";
        let mut store = SettingsDocumentStore::from_bytes(Some(original));
        let mut session = SettingsSession::load(&mut store);

        assert_eq!(session.warning(), Some(SettingsWarning::UnsupportedVersion { found: 99 }));
        assert!(!session.overwrite_allowed());
        assert!(session.save(&mut store).is_ok());
        assert!(store.pending().is_none());
    }

    #[test]
    fn explicit_edit_stages_one_complete_document() {
        let mut store = SettingsDocumentStore::from_bytes(Some(b"not ron"));
        let mut session = SettingsSession::load(&mut store);
        let mut edited = session.settings().clone();
        edited.show_both_sizes = !edited.show_both_sizes;
        session.replace(edited.clone());

        session.save(&mut store).expect("stage settings");
        let pending = store.take_pending().expect("pending document");
        assert_eq!(
            decode_document(&pending),
            SettingsRead::Envelope(SettingsEnvelope::current(edited))
        );
        assert!(store.take_pending().is_none());
    }

    #[test]
    fn unknown_fields_are_tolerated_inside_the_current_schema() {
        let encoded = encode_document(&SettingsEnvelope::current(Settings::default()))
            .expect("encode current settings");
        let mut text = String::from_utf8(encoded.into_vec()).expect("RON is UTF-8");
        let closing = text.rfind(')').expect("envelope closing delimiter");
        text.insert_str(closing, ",future_field:42");

        assert!(matches!(decode_document(text.as_bytes()), SettingsRead::Envelope(_)));
    }

    #[test]
    fn deeply_nested_unknown_data_hits_the_explicit_recursion_limit() {
        let mut text = String::from("(schema_version:1,future_field:");
        text.push_str(&"[".repeat(SETTINGS_RON_RECURSION_LIMIT + 4));
        text.push('0');
        text.push_str(&"]".repeat(SETTINGS_RON_RECURSION_LIMIT + 4));
        text.push_str(",settings:())");

        assert_eq!(decode_document(text.as_bytes()), SettingsRead::Malformed);
    }

    #[test]
    fn write_rejects_wrong_version_and_unvalidated_values() {
        let mut store = SettingsDocumentStore::from_bytes(None);
        store.write(&SettingsEnvelope::current(Settings::default())).expect("first valid document");
        assert!(store.pending().is_some());
        let wrong_version = SettingsEnvelope { schema_version: 99, settings: Settings::default() };
        assert_eq!(
            store.write(&wrong_version),
            Err(SettingsStoreError::new("settings.write.unsupported_version"))
        );
        assert!(store.pending().is_none());

        let invalid =
            SettingsEnvelope::current(Settings { layout_max_sectors: 0, ..Settings::default() });
        assert_eq!(
            store.write(&invalid),
            Err(SettingsStoreError::new("settings.write.unvalidated"))
        );
        assert!(store.pending().is_none());
    }

    #[test]
    fn unavailable_store_is_nonfatal_and_protected() {
        let mut unavailable = SettingsDocumentStore::unavailable("settings.storage.unavailable");
        let session = SettingsSession::load(&mut unavailable);
        assert_eq!(
            session.warning(),
            Some(SettingsWarning::Unavailable { code: "settings.storage.unavailable" })
        );
        assert!(!session.overwrite_allowed());
    }

    #[test]
    fn native_preclassified_outcomes_do_not_require_document_bytes() {
        let mut oversized = SettingsDocumentStore::from_read(SettingsRead::Oversized {
            bytes: MAX_SERIALIZED_SETTINGS_BYTES + 1,
        });
        let session = SettingsSession::load(&mut oversized);

        assert_eq!(
            session.warning(),
            Some(SettingsWarning::Oversized { bytes: MAX_SERIALIZED_SETTINGS_BYTES + 1 })
        );
        assert!(!session.overwrite_allowed());
        assert!(oversized.pending().is_none());
    }
}
