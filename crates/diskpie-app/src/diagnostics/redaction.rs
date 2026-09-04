//! Deterministic, dependency-free defense-in-depth text redaction.

#![forbid(unsafe_code)]

use std::fmt;

pub const REDACTED_PATH: &str = "<redacted:path>";
pub const REDACTED_URL: &str = "<redacted:url>";
pub const REDACTED_QUERY: &str = "<redacted:query>";
pub const REDACTED_ASSIGNMENT: &str = "<redacted:assignment>";
pub const REDACTED_SECRET: &str = "<redacted:secret>";
pub const REDACTED_CONTROL: &str = "<redacted:control>";

/// Per-export consent. Path inclusion never weakens the other redactors.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RedactionPolicy {
    pub include_paths: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RedactionCounts {
    pub paths: u64,
    pub urls: u64,
    pub query_strings: u64,
    pub assignments: u64,
    pub secrets: u64,
    pub control_runs: u64,
}

impl RedactionCounts {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.paths
            .saturating_add(self.urls)
            .saturating_add(self.query_strings)
            .saturating_add(self.assignments)
            .saturating_add(self.secrets)
            .saturating_add(self.control_runs)
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.paths = self.paths.saturating_add(other.paths);
        self.urls = self.urls.saturating_add(other.urls);
        self.query_strings = self.query_strings.saturating_add(other.query_strings);
        self.assignments = self.assignments.saturating_add(other.assignments);
        self.secrets = self.secrets.saturating_add(other.secrets);
        self.control_runs = self.control_runs.saturating_add(other.control_runs);
    }
}

/// Redacted UTF-8 plus auditable category counts.
#[derive(Clone, Eq, PartialEq)]
pub struct RedactedText {
    text: String,
    counts: RedactionCounts,
}

impl RedactedText {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn counts(&self) -> RedactionCounts {
        self.counts
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.text
    }
}

impl fmt::Display for RedactedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl fmt::Debug for RedactedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedText")
            .field("byte_len", &self.text.len())
            .field("counts", &self.counts)
            .finish()
    }
}

/// Applies every mandatory scrub in a fixed order.
///
/// Secret-like fields run first so a later path-consent choice cannot expose
/// one. URLs, query strings, assignments, and controls are mandatory on every
/// invocation; only the path pass is conditional.
#[must_use]
pub fn redact(input: &str, policy: RedactionPolicy) -> RedactedText {
    let mut counts = RedactionCounts::default();
    let text = replace_secrets(input, &mut counts);
    let text = replace_urls_and_queries(&text, &mut counts);
    let text = replace_assignments(&text, &mut counts);
    let text = if policy.include_paths { text } else { replace_paths(&text, &mut counts) };
    let text = replace_controls(&text, &mut counts);
    RedactedText { text, counts }
}

fn replace_secrets(input: &str, counts: &mut RedactionCounts) -> String {
    replace_spans(input, secret_span, |output| {
        output.push_str(REDACTED_SECRET);
        counts.secrets = counts.secrets.saturating_add(1);
    })
}

fn replace_urls_and_queries(input: &str, counts: &mut RedactionCounts) -> String {
    let text = replace_spans(input, url_span, |output| {
        output.push_str(REDACTED_URL);
        counts.urls = counts.urls.saturating_add(1);
    });
    replace_spans(&text, query_span, |output| {
        output.push_str(REDACTED_QUERY);
        counts.query_strings = counts.query_strings.saturating_add(1);
    })
}

fn replace_assignments(input: &str, counts: &mut RedactionCounts) -> String {
    replace_spans(input, assignment_span, |output| {
        output.push_str(REDACTED_ASSIGNMENT);
        counts.assignments = counts.assignments.saturating_add(1);
    })
}

fn replace_paths(input: &str, counts: &mut RedactionCounts) -> String {
    replace_spans(input, path_span, |output| {
        output.push_str(REDACTED_PATH);
        counts.paths = counts.paths.saturating_add(1);
    })
}

fn replace_controls(input: &str, counts: &mut RedactionCounts) -> String {
    let mut output = String::with_capacity(input.len());
    let mut redacting_run = false;
    for character in input.chars() {
        if is_disallowed_control(character) {
            if !redacting_run {
                output.push_str(REDACTED_CONTROL);
                counts.control_runs = counts.control_runs.saturating_add(1);
                redacting_run = true;
            }
        } else {
            redacting_run = false;
            output.push(character);
        }
    }
    output
}

fn replace_spans<F, G>(input: &str, mut find: F, mut replace: G) -> String
where
    F: FnMut(&str, usize) -> Option<usize>,
    G: FnMut(&mut String),
{
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if let Some(end) = find(input, index) {
            replace(&mut output);
            index = end.max(index + 1);
        } else {
            index = copy_next_character(input, index, &mut output);
        }
    }
    output
}

fn copy_next_character(input: &str, index: usize, output: &mut String) -> usize {
    let character = input[index..].chars().next().expect("index is a UTF-8 boundary");
    output.push(character);
    index + character.len_utf8()
}

fn secret_span(input: &str, index: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    if !is_word_boundary_before(bytes, index) {
        return None;
    }

    for prefix in [b"bearer".as_slice(), b"basic".as_slice()] {
        if ascii_prefix_eq_ignore_case(bytes, index, prefix) {
            let after = index + prefix.len();
            if is_word_boundary_after(bytes, after)
                && bytes.get(after).is_some_and(u8::is_ascii_whitespace)
            {
                let value_start = skip_ascii_whitespace(bytes, after);
                let end = consume_secret_value(input, value_start);
                if end > value_start {
                    return Some(end);
                }
            }
        }
    }

    const SECRET_KEYS: [&[u8]; 17] = [
        b"proxy-authorization",
        b"authorization",
        b"refresh_token",
        b"client_secret",
        b"access_token",
        b"set-cookie",
        b"password",
        b"x-api-key",
        b"id_token",
        b"api_key",
        b"apikey",
        b"passwd",
        b"cookie",
        b"secret",
        b"token",
        b"pwd",
        b"session",
    ];

    for key in SECRET_KEYS {
        if !ascii_prefix_eq_ignore_case(bytes, index, key) {
            continue;
        }
        let after_key = index + key.len();
        if !is_word_boundary_after(bytes, after_key) {
            continue;
        }
        let separator = skip_ascii_whitespace(bytes, after_key);
        let (value_start, minimum_end) = if matches!(bytes.get(separator), Some(b':' | b'=')) {
            (skip_ascii_whitespace(bytes, separator + 1), separator + 1)
        } else if separator > after_key {
            (separator, separator)
        } else {
            continue;
        };
        let consumes_remainder = matches_ignore_ascii_case(key, b"authorization")
            || matches_ignore_ascii_case(key, b"proxy-authorization")
            || matches_ignore_ascii_case(key, b"cookie")
            || matches_ignore_ascii_case(key, b"set-cookie");
        let end = if consumes_remainder {
            input.len()
        } else {
            consume_secret_value(input, value_start).max(value_start)
        };
        return Some(end.max(minimum_end));
    }
    None
}

fn consume_secret_value(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let Some(first) = bytes.get(start).copied() else {
        return start;
    };
    if matches!(first, b'\'' | b'"') {
        return consume_quoted(bytes, start, first);
    }

    let mut index = start;
    while let Some(byte) = bytes.get(index) {
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' | b')' | b']' | b'}') {
            break;
        }
        index += 1;
    }
    index
}

fn url_span(input: &str, index: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    if !is_url_boundary_before(bytes, index) {
        return None;
    }

    if ascii_prefix_eq_ignore_case(bytes, index, b"www.") {
        let after = index + 4;
        if bytes.get(after).is_some_and(|byte| byte.is_ascii_alphanumeric()) {
            return Some(trim_url_end(input, consume_url(input, after)));
        }
    }

    let first = *bytes.get(index)?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut cursor = index + 1;
    while cursor < bytes.len()
        && cursor - index <= 20
        && matches!(bytes[cursor], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'.' | b'-')
    {
        cursor += 1;
    }
    // A one-letter scheme is treated as a Windows drive designator.
    if cursor - index < 2 || bytes.get(cursor) != Some(&b':') {
        return None;
    }
    let (payload_start, valid_scheme) = if bytes.get(cursor..cursor + 3) == Some(b"://") {
        (cursor + 3, true)
    } else {
        let scheme = &bytes[index..cursor];
        (
            cursor + 1,
            [
                b"mailto".as_slice(),
                b"file".as_slice(),
                b"data".as_slice(),
                b"urn".as_slice(),
                b"tel".as_slice(),
                b"ssh".as_slice(),
            ]
            .iter()
            .any(|known| matches_ignore_ascii_case(scheme, known)),
        )
    };
    if !valid_scheme {
        return None;
    }
    let end = trim_url_end(input, consume_url(input, payload_start));
    (end > payload_start).then_some(end)
}

fn query_span(input: &str, index: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    if bytes.get(index) != Some(&b'?') || matches!(bytes.get(index.wrapping_sub(1)), Some(b'\\')) {
        return None;
    }
    let after = index + 1;
    if !bytes
        .get(after)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'%'))
    {
        return None;
    }
    let end = consume_url(input, after);
    Some(trim_url_end(input, end))
}

fn consume_url(input: &str, start: usize) -> usize {
    for (offset, character) in input[start..].char_indices() {
        if character.is_whitespace()
            || character.is_control()
            || matches!(character, '"' | '\'' | '<' | '>')
        {
            return start + offset;
        }
    }
    input.len()
}

fn trim_url_end(input: &str, mut end: usize) -> usize {
    while end > 0 {
        let character = input[..end].chars().next_back().expect("nonempty prefix");
        if matches!(character, '.' | ',' | ';' | '!' | ')' | ']' | '}') {
            end -= character.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn assignment_span(input: &str, index: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    if !is_word_boundary_before(bytes, index) {
        return None;
    }
    let mut cursor = index;
    let mut explicit_prefix = false;
    let percent_wrapped = bytes.get(cursor) == Some(&b'%');
    if matches!(bytes.get(cursor), Some(b'$' | b'%')) {
        explicit_prefix = true;
        cursor += 1;
    }
    let name_start = cursor;
    if !bytes.get(cursor).is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_') {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len()
        && cursor - name_start <= 64
        && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
    {
        cursor += 1;
    }
    if cursor - name_start > 64 {
        return None;
    }
    let name = &bytes[name_start..cursor];
    if percent_wrapped {
        if bytes.get(cursor) != Some(&b'%') {
            return None;
        }
        cursor += 1;
    }
    let before_whitespace = cursor;
    cursor = skip_ascii_whitespace(bytes, cursor);
    let allows_mixed_case_known_name = cursor != before_whitespace;
    if !explicit_prefix && !is_environment_like_name(name, allows_mixed_case_known_name) {
        return None;
    }
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ascii_whitespace(bytes, cursor + 1);
    if let Some(quote @ (b'\'' | b'"')) = bytes.get(cursor).copied() {
        return Some(consume_quoted(bytes, cursor, quote));
    }
    // An unquoted environment value can legally contain spaces. Conservatively
    // consume the remainder rather than expose a path component after one.
    Some(input.len())
}

fn is_environment_like_name(name: &[u8], allows_mixed_case_known_name: bool) -> bool {
    let has_letter = name.iter().any(u8::is_ascii_alphabetic);
    let all_uppercase = has_letter
        && name.iter().all(|byte| !byte.is_ascii_alphabetic() || byte.is_ascii_uppercase());
    all_uppercase
        || (allows_mixed_case_known_name
            && [
                b"path".as_slice(),
                b"home".as_slice(),
                b"userprofile".as_slice(),
                b"appdata".as_slice(),
                b"localappdata".as_slice(),
                b"temp".as_slice(),
                b"tmp".as_slice(),
                b"username".as_slice(),
                b"computername".as_slice(),
                b"homedrive".as_slice(),
                b"homepath".as_slice(),
                b"programfiles".as_slice(),
                b"systemroot".as_slice(),
            ]
            .iter()
            .any(|known| matches_ignore_ascii_case(name, known)))
}

fn path_span(input: &str, index: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    if !is_path_boundary_before(bytes, index) {
        return None;
    }

    let is_drive = bytes.get(index).is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(index + 1) == Some(&b':')
        && bytes.get(index + 2).is_some_and(|byte| {
            is_path_separator(*byte)
                || byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-')
        });
    let is_unc_or_verbatim = starts_with_two_separators(bytes, index)
        && (is_verbatim_prefix(bytes, index) || has_unc_host(bytes, index));
    let is_native_verbatim = bytes.get(index).is_some_and(|byte| is_path_separator(*byte))
        && bytes.get(index + 1..index + 3) == Some(b"??".as_slice())
        && bytes.get(index + 3).is_some_and(|byte| is_path_separator(*byte));

    if !(is_drive || is_unc_or_verbatim || is_native_verbatim) {
        return None;
    }
    Some(consume_path(input, index))
}

fn consume_path(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let quote = start.checked_sub(1).and_then(|before| match bytes[before] {
        b'\'' | b'"' => Some(bytes[before]),
        _ => None,
    });
    let mut index = start;
    while index < bytes.len() {
        if quote.is_some_and(|quote| bytes[index] == quote) {
            break;
        }
        if quote.is_none() && matches!(bytes[index], b'"' | b'\'' | b'<' | b'>' | b'|') {
            break;
        }
        index += 1;
    }
    index
}

fn is_verbatim_prefix(bytes: &[u8], index: usize) -> bool {
    matches!(bytes.get(index + 2), Some(b'?' | b'.'))
        && bytes.get(index + 3).is_some_and(|byte| is_path_separator(*byte))
}

fn has_unc_host(bytes: &[u8], index: usize) -> bool {
    let mut cursor = index + 2;
    let server_start = cursor;
    while bytes
        .get(cursor)
        .is_some_and(|byte| !is_path_separator(*byte) && !byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    cursor > server_start
}

fn starts_with_two_separators(bytes: &[u8], index: usize) -> bool {
    bytes.get(index).is_some_and(|byte| is_path_separator(*byte))
        && bytes.get(index + 1).is_some_and(|byte| is_path_separator(*byte))
}

const fn is_path_separator(byte: u8) -> bool {
    matches!(byte, b'\\' | b'/')
}

fn consume_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut cursor = start + 1;
    let mut escaped = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        cursor += 1;
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            break;
        }
    }
    cursor
}

fn skip_ascii_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn ascii_prefix_eq_ignore_case(bytes: &[u8], index: usize, expected: &[u8]) -> bool {
    bytes
        .get(index..index.saturating_add(expected.len()))
        .is_some_and(|actual| matches_ignore_ascii_case(actual, expected))
}

fn matches_ignore_ascii_case(actual: &[u8], expected: &[u8]) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn is_word_boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0
        || bytes
            .get(index - 1)
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
}

fn is_word_boundary_after(bytes: &[u8], index: usize) -> bool {
    bytes
        .get(index)
        .is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
}

fn is_url_boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0
        || bytes
            .get(index - 1)
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-' | b'.'))
}

fn is_path_boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0
        || bytes
            .get(index - 1)
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b':'))
}

fn is_disallowed_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn without_paths(input: &str) -> RedactedText {
        redact(input, RedactionPolicy { include_paths: false })
    }

    fn with_paths(input: &str) -> RedactedText {
        redact(input, RedactionPolicy { include_paths: true })
    }

    #[test]
    fn drive_unc_and_verbatim_paths_are_redacted() {
        for fixture in [
            r"C:\Users\Luis\private.txt",
            r"d:/Users/Luis/private.txt",
            r"E:relative\private.txt",
            r"\\private-server",
            r"\\server\share\private\file.txt",
            r"//server/share/private/file.txt",
            r"\\?\C:\Users\Luis\private.txt",
            r"\\?\UNC\server\share\private.txt",
            r"\\.\PhysicalDrive0",
            r"\??\C:\Users\Luis\private.txt",
        ] {
            let result = without_paths(fixture);
            assert_eq!(result.as_str(), REDACTED_PATH, "fixture: {fixture}");
            assert_eq!(result.counts().paths, 1, "fixture: {fixture}");
        }
    }

    #[test]
    fn quoted_path_preserves_only_the_delimiters() {
        let result = without_paths(r#"open "C:\Users\Luis\My File.txt""#);
        assert_eq!(result.as_str(), r#"open "<redacted:path>""#);
    }

    #[test]
    fn explicit_consent_keeps_paths_but_never_urls_or_secrets() {
        let input =
            r#"path="C:\Users\Luis\My File.txt" site=https://private.test/a?q=1 token=hunter2"#;
        let result = with_paths(input);

        assert_eq!(
            result.as_str(),
            r#"path="C:\Users\Luis\My File.txt" site=<redacted:url> <redacted:secret>"#
        );
        assert_eq!(result.counts().paths, 0);
        assert_eq!(result.counts().urls, 1);
        assert_eq!(result.counts().secrets, 1);
    }

    #[test]
    fn urls_and_standalone_queries_are_always_redacted() {
        let result = with_paths(concat!(
            "one=https://example.test/private?q=yes, two=ftp://host/file; ",
            "route?user=luis&x=1 mail=mailto:luis@example.test flag?private."
        ));
        assert_eq!(
            result.as_str(),
            concat!(
                "one=<redacted:url>, two=<redacted:url>; route<redacted:query> ",
                "mail=<redacted:url> flag<redacted:query>."
            )
        );
        assert_eq!(result.counts().urls, 3);
        assert_eq!(result.counts().query_strings, 2);
    }

    #[test]
    fn secret_headers_keys_and_auth_schemes_are_case_insensitive() {
        for fixture in [
            "password=hunter2",
            "password hunter2",
            "PassWd: hunter2",
            "TOKEN = abc.def",
            "token abc.def",
            "api_key='private value'",
            "X-API-KEY: abc",
            "Authorization: Bearer abc.def tail",
            "Cookie: session=abc; user=luis",
            "Set-Cookie: session=abc; Secure",
            "Bearer abc.def",
            "Basic YWxhZGRpbjpvcGVuc2VzYW1l",
        ] {
            let result = with_paths(fixture);
            assert_eq!(result.as_str(), REDACTED_SECRET, "fixture: {fixture}");
            assert_eq!(result.counts().secrets, 1, "fixture: {fixture}");
        }
    }

    #[test]
    fn environment_like_assignments_are_removed_even_with_path_consent() {
        for fixture in [
            r"HOME=C:\Users\Luis Other Words",
            r#"Path = "C:\Windows;C:\Tools""#,
            r"%APPDATA%=C:\Users\Luis\AppData",
            r"$home=/private/home",
            r"DISKPIE_CACHE=private-value",
        ] {
            let result = with_paths(fixture);
            assert_eq!(result.as_str(), REDACTED_ASSIGNMENT, "fixture: {fixture}");
            assert_eq!(result.counts().assignments, 1, "fixture: {fixture}");
        }
    }

    #[test]
    fn ordinary_structured_fields_do_not_match_environment_assignments() {
        let input = "count=7 outcome=succeeded token_count=3 C: not-a-path";
        assert_eq!(with_paths(input).as_str(), input);
    }

    #[test]
    fn control_and_format_runs_are_visible_markers() {
        let result = with_paths("a\0\u{001b}b\u{0085}c\u{202e}\u{2066}d");
        assert_eq!(result.as_str(), "a<redacted:control>b<redacted:control>c<redacted:control>d");
        assert_eq!(result.counts().control_runs, 3);
        assert!(!result.as_str().chars().any(is_disallowed_control));
    }

    #[test]
    fn redaction_is_deterministic_and_idempotent() {
        let input = r#""C:\Users\Luis\x" https://x.test/?a=b password=abc"#;
        let first = without_paths(input);
        let second = without_paths(first.as_str());
        assert_eq!(first.as_str(), second.as_str());
        assert_eq!(second.counts().total(), 0);
    }

    #[test]
    fn debug_output_never_repeats_redacted_content() {
        let result = with_paths(r"C:\Users\Luis\private.txt");
        let debug = format!("{result:?}");

        assert!(!debug.contains("Users"));
        assert!(debug.contains("byte_len"));
    }

    #[test]
    fn utf8_neighbors_survive_ascii_redaction_boundaries() {
        let result = without_paths("🙂 antes \"C:\\privado\\niño.txt\" después 東京");
        assert_eq!(result.as_str(), "🙂 antes \"<redacted:path>\" después 東京");
        assert!(std::str::from_utf8(result.as_str().as_bytes()).is_ok());
    }
}
