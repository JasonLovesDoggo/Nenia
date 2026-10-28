//! Read-only discovery and bounded, best-effort metadata. Raw archival is independent.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Maximum JSON document or individual JSONL record parsed for metadata/indexing.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_FIELD_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Auto,
    Codex,
    Claude,
    Pi,
    Plaud,
    #[default]
    Generic,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Plaud => "plaud",
            Self::Generic => "generic",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub path: PathBuf,
    pub source: Source,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub source: Source,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub started_at: Option<String>,
    /// Nonempty physical records, or top-level array entries for a small JSON document.
    pub events: u64,
    pub malformed_lines: u64,
    pub skipped_oversized_lines: u64,
}

/// Find local stores without opening files. Honors the providers' home overrides.
/// Missing roots are normal; arbitrary export directories use `discover_paths`.
pub fn discover(home: &Path) -> io::Result<Vec<Candidate>> {
    discover_with(home, |key| std::env::var_os(key))
}

fn discover_with(
    home: &Path,
    mut environment: impl FnMut(&str) -> Option<OsString>,
) -> io::Result<Vec<Candidate>> {
    let mut root = |key: &str, fallback: PathBuf| {
        environment(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| match path.strip_prefix("~") {
                Ok(suffix) => home.join(suffix),
                Err(_) => path,
            })
            .unwrap_or(fallback)
    };
    let codex = root("CODEX_HOME", home.join(".codex"));
    let claude = root("CLAUDE_CONFIG_DIR", home.join(".claude"));
    let pi = root("PI_CODING_AGENT_DIR", home.join(".pi").join("agent"));
    let pi_sessions = root("PI_CODING_AGENT_SESSION_DIR", pi.join("sessions"));
    let mut found = BTreeMap::new();
    for (root, source) in [
        (codex.join("sessions"), Source::Codex),
        (codex.join("archived_sessions"), Source::Codex),
        (claude.join("projects"), Source::Claude),
        (pi_sessions, Source::Pi),
    ] {
        match fs::symlink_metadata(&root) {
            Ok(_) => collect(&root, source, true, &mut found)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(found.into_values().collect())
}

/// Explicit files may have any extension. Directories include JSONL/NDJSON/JSON/TXT/MD.
/// Symlinks (including a supplied root or any ancestor) are never followed.
pub fn discover_paths(paths: &[PathBuf]) -> io::Result<Vec<Candidate>> {
    let mut found = BTreeMap::new();
    for root in paths {
        collect(root, source_for_path(root), false, &mut found)?;
    }
    Ok(found.into_values().collect())
}

fn collect(
    root: &Path,
    source: Source,
    jsonl_only: bool,
    found: &mut BTreeMap<PathBuf, Candidate>,
) -> io::Result<()> {
    for ancestor in root.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
            return Ok(());
        }
    }
    let explicit_file = fs::symlink_metadata(root)?.is_file() && !jsonl_only;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let extension = entry
            .path()
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let supported = extension.eq_ignore_ascii_case("jsonl")
            || extension.eq_ignore_ascii_case("ndjson")
            || (!jsonl_only
                && ["json", "txt", "md"]
                    .iter()
                    .any(|e| extension.eq_ignore_ascii_case(e)));
        if explicit_file || supported {
            let path = fs::canonicalize(entry.path())?;
            let source = if source == Source::Generic {
                source_for_path(&path)
            } else {
                source
            };
            found
                .entry(path.clone())
                .or_insert(Candidate { path, source });
        }
    }
    Ok(())
}

fn source_for_path(path: &Path) -> Source {
    let components: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    if components
        .windows(2)
        .any(|c| c[0] == ".codex" && (c[1] == "sessions" || c[1] == "archived_sessions"))
    {
        Source::Codex
    } else if components
        .windows(2)
        .any(|c| c[0] == ".claude" && c[1] == "projects")
    {
        Source::Claude
    } else if components
        .windows(3)
        .any(|c| c[0] == ".pi" && c[1] == "agent" && c[2] == "sessions")
    {
        Source::Pi
    } else {
        Source::Generic
    }
}

/// Parse metadata without rejecting unknown formats, invalid UTF-8 or partial records.
/// Only an actual reader I/O error is returned. Memory is bounded independently of file size.
pub fn inspect<R: BufRead>(reader: R, source_hint: Source, path: &Path) -> io::Result<Metadata> {
    let mut metadata = Metadata {
        source: if source_hint == Source::Auto {
            source_for_path(path)
        } else {
            source_hint
        },
        ..Metadata::default()
    };
    scan(reader, |record| {
        metadata.events += 1;
        match record {
            Record::Json(value) => inspect_value(value, source_hint == Source::Auto, &mut metadata),
            Record::Raw(bytes, oversized) => {
                if oversized {
                    metadata.skipped_oversized_lines += 1;
                } else if looks_json(bytes) || std::str::from_utf8(bytes).is_err() {
                    metadata.malformed_lines += 1;
                }
                if !looks_json(bytes) && metadata.title.is_none() {
                    if let Ok(text) = std::str::from_utf8(bytes) {
                        metadata.title = title(text);
                    }
                }
            }
        }
        true
    })?;
    if metadata.title.is_none() {
        metadata.title = path.file_stem().and_then(|s| s.to_str()).and_then(title);
    }
    Ok(metadata)
}

/// Extract at most `max_bytes` of searchable UTF-8. This never modifies archived bytes.
/// Images, audio, binary/base64 fields and oversized JSON records are omitted.
pub fn index_text<R: BufRead>(reader: R, max_bytes: usize) -> io::Result<String> {
    let mut text = String::new();
    if max_bytes == 0 {
        return Ok(text);
    }
    scan(reader, |record| {
        match record {
            Record::Json(value) => collect_text(value, &mut text, max_bytes, 0),
            Record::Raw(bytes, _) if !looks_json(bytes) => {
                append_text(&mut text, &String::from_utf8_lossy(bytes), max_bytes);
            }
            Record::Raw(_, _) => {}
        }
        text.len() < max_bytes
    })?;
    Ok(text)
}

fn inspect_value(value: &Value, detect: bool, metadata: &mut Metadata) {
    let kind = value["type"].as_str().unwrap_or("");
    if detect && metadata.source == Source::Generic {
        metadata.source = if kind == "session_meta"
            || kind == "response_item"
            || kind == "turn_context"
        {
            Source::Codex
        } else if kind == "session" && value.get("version").is_some() && value.get("cwd").is_some()
        {
            Source::Pi
        } else if value.get("sessionId").is_some()
            && matches!(
                kind,
                "user" | "assistant" | "system" | "summary" | "custom-title"
            )
        {
            Source::Claude
        } else {
            Source::Generic
        };
    }
    let header = if kind == "session_meta" {
        &value["payload"]
    } else {
        value
    };
    if metadata.session_id.is_none() {
        metadata.session_id = field(header, &["session_id", "sessionId"]).or_else(|| {
            matches!(kind, "session_meta" | "session")
                .then(|| field(header, &["id"]))
                .flatten()
        });
    }
    if metadata.cwd.is_none() {
        metadata.cwd = field(header, &["cwd", "working_directory"])
            .or_else(|| field(&value["payload"], &["cwd"]));
    }
    if metadata.started_at.is_none() {
        metadata.started_at = timestamp(header).or_else(|| timestamp(value));
    }
    let explicit_title = match kind {
        "custom-title" => field(value, &["customTitle", "title"]),
        "session_info" => field(value, &["name"]),
        "summary" => field(value, &["summary"]),
        _ => field(header, &["title", "session_name"]).or_else(|| {
            matches!(metadata.source, Source::Generic | Source::Plaud)
                .then(|| field(header, &["name"]))
                .flatten()
        }),
    };
    if let Some(text) = explicit_title {
        metadata.title = title(&text);
    } else if metadata.title.is_none() {
        let user = if value["payload"]["role"] == "user" {
            &value["payload"]["content"]
        } else if value["payload"]["type"] == "user_message" {
            &value["payload"]["message"]
        } else if value["message"]["role"] == "user" || kind == "user" {
            &value["message"]["content"]
        } else {
            &value["prompt"]
        };
        let mut text = String::new();
        collect_text(user, &mut text, 1024, 0);
        metadata.title = title(&text);
    }
}

fn field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value[*name].as_str().filter(|s| !s.is_empty()))
        .map(|s| truncate(s, MAX_FIELD_BYTES).to_owned())
}

fn timestamp(value: &Value) -> Option<String> {
    for key in ["timestamp", "started_at", "created_at"] {
        if let Some(text) = value[key].as_str().filter(|s| !s.is_empty()) {
            return Some(truncate(text, 128).to_owned());
        }
        if let Some(number) = value[key].as_i64() {
            let millis = if number.unsigned_abs() < 100_000_000_000 {
                number.checked_mul(1000)?
            } else {
                number
            };
            return chrono::DateTime::from_timestamp_millis(millis).map(|t| t.to_rfc3339());
        }
    }
    None
}

fn title(text: &str) -> Option<String> {
    let result: String = text
        .split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .take(160)
        .collect();
    let result = result.trim().to_owned();
    (!result.is_empty()).then_some(result)
}

fn truncate(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn append_text(output: &mut String, text: &str, limit: usize) {
    if output.len() >= limit || text.is_empty() || text.starts_with("data:") {
        return;
    }
    // Long unbroken base64 blobs are not useful search terms, even in generic exports.
    if text.len() > 1024
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b))
    {
        return;
    }
    let part = truncate(text, limit - output.len());
    output.push_str(part);
    if output.len() < limit {
        output.push('\n');
    }
}

fn collect_text(value: &Value, output: &mut String, limit: usize, depth: u8) {
    if depth > 32 || output.len() >= limit {
        return;
    }
    match value {
        Value::String(text) => append_text(output, text, limit),
        Value::Array(values) => {
            for value in values {
                collect_text(value, output, limit, depth + 1);
                if output.len() >= limit {
                    break;
                }
            }
        }
        Value::Object(fields) => {
            if matches!(
                value["type"].as_str(),
                Some("image" | "image_url" | "input_image" | "audio" | "input_audio")
            ) {
                return;
            }
            for (key, value) in fields {
                if key == "data" && !value.is_object() && !value.is_array() {
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "base64"
                        | "bytes"
                        | "image_url"
                        | "audio"
                        | "signature"
                        | "encrypted_content"
                        | "id"
                        | "parentId"
                        | "uuid"
                        | "sessionId"
                        | "session_id"
                        | "type"
                        | "role"
                        | "timestamp"
                        | "created_at"
                        | "model"
                        | "modelId"
                        | "usage"
                ) {
                    continue;
                }
                collect_text(value, output, limit, depth + 1);
                if output.len() >= limit {
                    break;
                }
            }
        }
        _ => {}
    }
}

enum Record<'a> {
    Json(&'a Value),
    Raw(&'a [u8], bool),
}

fn looks_json(bytes: &[u8]) -> bool {
    matches!(
        bytes.iter().find(|b| !b.is_ascii_whitespace()),
        Some(b'{' | b'[' | b'"' | b'}' | b']')
    )
}

/// Parse small whole JSON documents (including pretty JSON); otherwise stream JSONL/text.
fn scan<R: BufRead>(mut reader: R, mut visit: impl FnMut(Record<'_>) -> bool) -> io::Result<()> {
    let mut prefix = Vec::new();
    reader
        .by_ref()
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut prefix)?;
    if prefix.len() <= MAX_RECORD_BYTES {
        if let Ok(value) = serde_json::from_slice::<Value>(&prefix) {
            if let Value::Array(values) = &value {
                for value in values {
                    if !visit(Record::Json(value)) {
                        break;
                    }
                }
            } else {
                visit(Record::Json(&value));
            }
            return Ok(());
        }
    }
    let mut reader = BufReader::new(Cursor::new(prefix).chain(reader));
    let mut line = Vec::new();
    while let Some(oversized) = read_line(&mut reader, &mut line)? {
        if line.iter().all(u8::is_ascii_whitespace) && !oversized {
            continue;
        }
        let parsed = if oversized {
            None
        } else {
            serde_json::from_slice::<Value>(&line).ok()
        };
        let record = match &parsed {
            Some(value) => Record::Json(value),
            None => Record::Raw(&line, oversized),
        };
        if !visit(record) {
            break;
        }
    }
    Ok(())
}

/// Unlike `read_until`, an arbitrarily long line cannot allocate arbitrarily much memory.
fn read_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    line.clear();
    let mut seen = false;
    let mut oversized = false;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok(seen.then_some(oversized));
        }
        seen = true;
        let newline = bytes.iter().position(|&b| b == b'\n');
        let consumed = newline.map_or(bytes.len(), |n| n + 1);
        let data_len = newline.unwrap_or(bytes.len());
        let keep = data_len.min(MAX_RECORD_BYTES - line.len());
        line.extend_from_slice(&bytes[..keep]);
        oversized |= keep < data_len;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(oversized));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory() -> tempfile::TempDir {
        // macOS's /var and /tmp are symlinks; tests use the real temporary root.
        tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }

    fn metadata(input: &[u8]) -> Metadata {
        inspect(Cursor::new(input), Source::Auto, Path::new("export.jsonl")).unwrap()
    }

    #[test]
    fn codex_rollout_and_partial_final_record() {
        let input = concat!(
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"id\":\"synthetic-codex\",\"cwd\":\"/fiction/project\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Build a tiny archive 🌙\"}]}}\n",
            "{\"type\":\"event_msg\",\"payload\":"
        );
        let got = metadata(input.as_bytes());
        assert_eq!(got.source, Source::Codex);
        assert_eq!(got.session_id.as_deref(), Some("synthetic-codex"));
        assert_eq!(got.cwd.as_deref(), Some("/fiction/project"));
        assert_eq!(got.started_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(got.title.as_deref(), Some("Build a tiny archive 🌙"));
        assert_eq!(got.events, 3);
        assert_eq!(got.malformed_lines, 1);
    }

    #[test]
    fn claude_user_message_and_custom_title() {
        let input = concat!(
            "{\"type\":\"user\",\"sessionId\":\"synthetic-claude\",\"cwd\":\"/fiction/claude\",\"timestamp\":\"2026-01-02T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"Fix the test\"}}\n",
            "{\"type\":\"custom-title\",\"sessionId\":\"synthetic-claude\",\"customTitle\":\"My chosen title\"}\n"
        );
        let got = metadata(input.as_bytes());
        assert_eq!(got.source, Source::Claude);
        assert_eq!(got.session_id.as_deref(), Some("synthetic-claude"));
        assert_eq!(got.title.as_deref(), Some("My chosen title"));
        assert_eq!(got.events, 2);
    }

    #[test]
    fn pi_tree_header_and_renaming() {
        let input = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"synthetic-pi\",\"timestamp\":\"2026-01-03T00:00:00Z\",\"cwd\":\"/fiction/pi\"}\n",
            "{\"type\":\"message\",\"id\":\"entry-id\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"Explore storage\"}}\n",
            "{\"type\":\"session_info\",\"id\":\"second-entry\",\"parentId\":\"entry-id\",\"name\":\"Storage experiment\"}\n"
        );
        let got = metadata(input.as_bytes());
        assert_eq!(got.source, Source::Pi);
        assert_eq!(got.session_id.as_deref(), Some("synthetic-pi"));
        assert_eq!(got.title.as_deref(), Some("Storage experiment"));
        assert_eq!(got.events, 3);
    }

    #[test]
    fn pretty_json_export_and_opaque_plaud_hint() {
        let input = b"{\n  \"name\": \"Field notes\",\n  \"transcript\": [\n    {\"text\": \"A small meeting transcript\"}\n  ]\n}\n";
        let got = inspect(Cursor::new(input), Source::Plaud, Path::new("meeting.json")).unwrap();
        assert_eq!(got.source, Source::Plaud);
        assert_eq!(got.title.as_deref(), Some("Field notes"));
        assert_eq!(got.malformed_lines, 0);
        assert_eq!(got.events, 1);
        assert!(index_text(Cursor::new(input), 1024)
            .unwrap()
            .contains("meeting transcript"));
        assert_eq!(metadata(input).source, Source::Generic);
    }

    #[test]
    fn json_array_records_and_timestamp() {
        let input =
            br#"[{"session_id":"s","timestamp":1700000000000,"prompt":"Hello"},{"text":"World"}]"#;
        let got = metadata(input);
        assert_eq!(got.events, 2);
        assert_eq!(got.session_id.as_deref(), Some("s"));
        assert_eq!(got.started_at.as_deref(), Some("2023-11-14T22:13:20+00:00"));
        assert_eq!(got.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn malformed_unicode_and_no_final_newline_are_tolerated() {
        let input = b"{broken}\n\xff\xfe\nplain final line";
        let got = metadata(input);
        assert_eq!(got.events, 3);
        assert_eq!(got.malformed_lines, 2);
        assert_eq!(got.title.as_deref(), Some("plain final line"));
        let text = index_text(Cursor::new(input), 1024).unwrap();
        assert!(text.contains("plain final line"));
        assert!(text.contains('\u{fffd}'));
    }

    #[test]
    fn oversized_line_does_not_hide_later_metadata() {
        let mut input = br#"{"data":""#.to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_RECORD_BYTES * 3));
        input
            .extend_from_slice(b"\"}\n{\"session_id\":\"after-blob\",\"text\":\"Visible text\"}\n");
        let got = metadata(&input);
        assert_eq!(got.events, 2);
        assert_eq!(got.skipped_oversized_lines, 1);
        assert_eq!(got.session_id.as_deref(), Some("after-blob"));
        assert_eq!(
            index_text(Cursor::new(input), 100).unwrap(),
            "Visible text\n"
        );
    }

    #[test]
    fn indexing_omits_media_and_caps_at_utf8_boundary() {
        let input = br#"{"message":{"content":[{"type":"image","source":{"data":"not searchable"}},{"type":"text","text":"find this"}]},"encrypted_content":"secret","signature":"skip"}"#;
        assert_eq!(index_text(Cursor::new(input), 1024).unwrap(), "find this\n");
        let capped = index_text(Cursor::new("🌙🌙🌙".as_bytes()), 5).unwrap();
        assert_eq!(capped, "🌙\n");
        assert_eq!(index_text(Cursor::new(b"anything"), 0).unwrap(), "");
    }

    #[test]
    fn markdown_and_empty_input() {
        let got = metadata(b"# Architecture notes\n\nObject storage wins.\n");
        assert_eq!(got.title.as_deref(), Some("# Architecture notes"));
        assert_eq!(got.events, 2);
        assert_eq!(got.malformed_lines, 0);
        assert_eq!(metadata(b"").events, 0);
        assert_eq!(metadata(b"[]").events, 0);
    }

    #[test]
    fn generic_json_data_envelopes_are_searchable() {
        let input = br#"{"data":{"content":"Wrapped export","data":"opaque blob"}}"#;
        assert_eq!(
            index_text(Cursor::new(input), 1024).unwrap(),
            "Wrapped export\n"
        );
    }

    #[test]
    fn source_hint_overrides_detection() {
        let got = inspect(
            Cursor::new(br#"{"type":"session_meta","payload":{"id":"id"}}"#),
            Source::Generic,
            Path::new("trace"),
        )
        .unwrap();
        assert_eq!(got.source, Source::Generic);
    }

    #[test]
    fn discovery_is_deterministic_and_deduplicates_roots() {
        let dir = temporary_directory();
        for name in [
            ".codex/sessions/2026/01/trace.jsonl",
            ".codex/archived_sessions/old.jsonl",
            ".claude/projects/project/session.jsonl",
            ".pi/agent/sessions/project/session.jsonl",
            "exports/notes.md",
            "exports/meeting.JSON",
            "exports/not-a-trace.png",
        ] {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"synthetic").unwrap();
        }
        let automatic = discover_with(dir.path(), |_| None).unwrap();
        assert_eq!(automatic.len(), 4);
        assert_eq!(
            automatic
                .iter()
                .filter(|c| c.source == Source::Codex)
                .count(),
            2
        );
        let explicit =
            discover_paths(&[dir.path().to_path_buf(), dir.path().join("exports")]).unwrap();
        assert_eq!(explicit.len(), 6);
        assert!(explicit.windows(2).all(|c| c[0].path < c[1].path));
        assert_eq!(
            discover_paths(&[dir.path().join("exports/not-a-trace.png")])
                .unwrap()
                .len(),
            1
        );
        assert!(discover_with(&dir.path().join("missing-home"), |_| None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn provider_environment_roots_and_tilde_expansion() {
        let dir = temporary_directory();
        let entries = [
            (
                "CODEX_HOME",
                "custom-codex",
                "sessions/a.jsonl",
                Source::Codex,
            ),
            (
                "CLAUDE_CONFIG_DIR",
                "custom-claude",
                "projects/a.jsonl",
                Source::Claude,
            ),
            (
                "PI_CODING_AGENT_SESSION_DIR",
                "custom-pi",
                "a.jsonl",
                Source::Pi,
            ),
        ];
        for (_, root, suffix, _) in &entries {
            let path = dir.path().join(root).join(suffix);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"synthetic").unwrap();
        }
        let found = discover_with(dir.path(), |key| {
            entries
                .iter()
                .find(|(name, _, _, _)| *name == key)
                .map(|(_, root, _, _)| OsString::from(format!("~/{root}")))
        })
        .unwrap();
        assert_eq!(found.len(), 3);
        for (_, _, _, source) in entries {
            assert!(found.iter().any(|candidate| candidate.source == source));
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_never_follows_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = temporary_directory();
        let outside = temporary_directory();
        fs::write(outside.path().join("private.jsonl"), b"synthetic").unwrap();
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        symlink(
            outside.path().join("private.jsonl"),
            dir.path().join("direct.jsonl"),
        )
        .unwrap();
        assert!(discover_paths(&[dir.path().to_path_buf()])
            .unwrap()
            .is_empty());
        assert!(discover_paths(&[dir.path().join("linked")])
            .unwrap()
            .is_empty());
        assert!(discover_paths(&[dir.path().join("linked/private.jsonl")])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn io_failure_is_not_reported_as_success() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected read failure"))
            }
        }
        let error = inspect(BufReader::new(Broken), Source::Auto, Path::new("x")).unwrap_err();
        assert!(error.to_string().contains("injected read failure"));
    }
}
