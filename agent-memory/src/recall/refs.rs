//! References, locators and hit identity (§3.2).
//!
//! `hit_id = sha256(source_id|rel_path|locator|text_range|range_sha256)` over
//! compact JSON with a fixed field order. `file_sha256` / `row_sha256` and the
//! display snippet never enter the hash.

use crate::recall::contract::{
    SCHEMA_ARCHIVE_RECORD_V1, SCHEMA_CLAUDE_JSONL_V1, SCHEMA_CODEX_ROLLOUT_V1, SCHEMA_DSH_JSONL,
    SCHEMA_JSON_LEGACY_V1, SCHEMA_MD_V1, SCHEMA_SQLITE_V1, SCHEMA_SQLITE_V2, SOURCE_KIND_CODEX,
};
use crate::recall::hash::{sha256_hex, TextRange};
use serde::{Deserialize, Serialize};

/// Stage-2 S5 archive fallback positioning, carried only by hits whose units
/// were built from committed evidence instead of the live source
/// (`Locator::archive_fallback`). The live coordinate fields keep their exact
/// meaning; this block only tells readback where the bytes come from now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveLocator {
    /// JSONL providers: the archived record's content address (the SHA-256 of
    /// the line bytes, the same digest `state/records.json` indexes). Readback
    /// resolves it to the committed capture and rebuilds the carrier from the
    /// same manifest group. SQLite (OpenCode) evidence keeps `None`: a row is
    /// addressed by the live locator shape inside the replayed evidence, so
    /// the marker alone is the routing decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_sha256: Option<String>,
    /// OpenCode only: epoch milliseconds of the committed delta that deleted
    /// this session (0/absent = the session still exists in the evidence
    /// state). Present on units rebuilt from a session the user deleted in
    /// OpenCode — the deletion is part of the evidence, the content stays
    /// recoverable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<i64>,
}

impl ArchiveLocator {
    pub fn record(record_sha256: &str) -> Self {
        ArchiveLocator {
            record_sha256: Some(record_sha256.to_string()),
            deleted_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locator {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_message_id: Option<String>,
    /// Decompressed, 0-based line ordinal (`codex-rollout-v1` only). Missing
    /// for every other generation, so their `hit_id` input is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_index: Option<u64>,
    /// `dsh-jsonl` only: the event envelope's `seq`. It is a coordinate, not
    /// an addressing key (`record_index` + `pointer` address the unit); it
    /// lets a reader confirm the event identity after a repair. Absent for
    /// every other generation, so their serialization is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_seq: Option<u64>,
    /// `dsh-jsonl` only: the generation carrier path relative to the source
    /// root. It repeats `ref.rel_path` on purpose so the locator alone names
    /// the generation file it addresses. Absent for every other generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// `archive-record-v1`: SHA-256 of the committed capture record. Together
    /// with `projection_offset` it is the only locator input, so `capture_id`
    /// and `generation` can never leak into `hit_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_sha256: Option<String>,
    /// `archive-record-v1`: start offset inside the decoded record projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_offset: Option<u64>,
    /// Stage-2 S5: present only on archive-fallback hits (units built from
    /// committed evidence of a session that no longer exists in the live
    /// source). Absent on every live and `archive-record-v1` ref, so their
    /// serialization and `hit_id` input are unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_fallback: Option<ArchiveLocator>,
}

impl Locator {
    pub fn empty() -> Self {
        Locator::default()
    }

    pub fn is_empty(&self) -> bool {
        self.session_id.is_none()
            && self.message_id.is_none()
            && self.part_id.is_none()
            && self.session_message_id.is_none()
            && self.record_index.is_none()
            && self.event_seq.is_none()
            && self.carrier.is_none()
            && self.pointer.is_none()
            && self.record_sha256.is_none()
            && self.projection_offset.is_none()
            && self.archive_fallback.is_none()
    }

    pub fn compact_json(&self) -> String {
        serde_json::to_string(self).expect("locator serializes")
    }

    /// Same carrier coordinates, ignoring the `archive_fallback` marker. The
    /// marker routes a ref into the evidence readback; the mirrored or
    /// rebuilt reader reports live-form locators, so the context window's
    /// "hit part in its session" position check must compare coordinates
    /// only (found by rework 3's end-to-end check: context reads of
    /// archive-fallback refs failed with REF_LOCATOR_MISSING).
    pub fn same_coordinates(&self, other: &Locator) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.archive_fallback = None;
        right.archive_fallback = None;
        left == right
    }

    pub fn v1(session_id: &str, message_id: &str, part_id: &str) -> Self {
        Locator {
            session_id: Some(session_id.to_string()),
            message_id: Some(message_id.to_string()),
            part_id: Some(part_id.to_string()),
            session_message_id: None,
            record_index: None,
            event_seq: None,
            carrier: None,
            pointer: None,
            record_sha256: None,
            projection_offset: None,
            archive_fallback: None,
        }
    }

    pub fn v2(session_id: &str, session_message_id: &str, pointer: &str) -> Self {
        Locator {
            session_id: Some(session_id.to_string()),
            message_id: None,
            part_id: None,
            session_message_id: Some(session_message_id.to_string()),
            record_index: None,
            event_seq: None,
            carrier: None,
            pointer: Some(pointer.to_string()),
            record_sha256: None,
            projection_offset: None,
            archive_fallback: None,
        }
    }

    /// A SQLite V2 text part in the optional `session_part` table.
    pub fn v2_part(session_id: &str, session_message_id: &str, part_id: &str) -> Self {
        Self {
            part_id: Some(part_id.to_string()),
            ..Self::v2(session_id, session_message_id, "/text")
        }
    }

    pub fn codex(
        session_id: &str,
        message_id: Option<&str>,
        record_index: u64,
        pointer: &str,
    ) -> Self {
        Locator {
            session_id: Some(session_id.to_string()),
            message_id: message_id.map(|value| value.to_string()),
            part_id: None,
            session_message_id: None,
            record_index: Some(record_index),
            event_seq: None,
            carrier: None,
            pointer: Some(pointer.to_string()),
            record_sha256: None,
            projection_offset: None,
            archive_fallback: None,
        }
    }

    /// `dsh-jsonl`: logical line + block pointer, with the event `seq` kept as
    /// a coordinate. `message_id` stays empty because a DSH unit is addressed
    /// by its event, not by a message row.
    pub fn dsh(
        session_id: &str,
        record_index: u64,
        pointer: &str,
        event_seq: Option<u64>,
        carrier: Option<&str>,
    ) -> Self {
        Locator {
            session_id: Some(session_id.to_string()),
            message_id: None,
            part_id: None,
            session_message_id: None,
            record_index: Some(record_index),
            event_seq,
            carrier: carrier.map(str::to_string),
            pointer: Some(pointer.to_string()),
            record_sha256: None,
            projection_offset: None,
            archive_fallback: None,
        }
    }

    /// `archive-record-v1`: content-addressed record handle. `capture_id` and
    /// `generation` are deliberately absent — the same content captured twice
    /// keeps the same locator and therefore the same `hit_id`.
    pub fn archive(record_sha256: &str, projection_offset: u64) -> Self {
        Locator {
            session_id: None,
            message_id: None,
            part_id: None,
            session_message_id: None,
            record_index: None,
            event_seq: None,
            carrier: None,
            pointer: None,
            record_sha256: Some(record_sha256.to_string()),
            projection_offset: Some(projection_offset),
            archive_fallback: None,
        }
    }

    /// Stage-2 S5: mark this locator as an archive-fallback reference. Only
    /// fallback units carry it; the live coordinate fields are untouched.
    pub fn with_archive_fallback(mut self, fallback: ArchiveLocator) -> Self {
        self.archive_fallback = Some(fallback);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub range_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitRef {
    pub source_id: String,
    pub source_kind: String,
    pub root_sha256: String,
    pub rel_path: String,
    pub schema_gen: String,
    pub locator: Locator,
    pub fingerprint: Fingerprint,
    pub text_range: TextRange,
}

#[derive(Serialize)]
struct HitIdInput<'a> {
    source_id: &'a str,
    rel_path: &'a str,
    locator: &'a Locator,
    text_range: &'a TextRange,
    range_sha256: &'a str,
}

pub fn hit_id(ref_: &HitRef) -> String {
    let input = HitIdInput {
        source_id: &ref_.source_id,
        rel_path: &ref_.rel_path,
        locator: &ref_.locator,
        text_range: &ref_.text_range,
        range_sha256: &ref_.fingerprint.range_sha256,
    };
    let compact = serde_json::to_string(&input).expect("hit id input serializes");
    sha256_hex(compact.as_bytes())
}

pub fn locator_schema(source_kind: &str, schema_gen: &str) -> Option<&'static [&'static str]> {
    match (source_kind, schema_gen) {
        (_, SCHEMA_MD_V1) => Some(&[]),
        (crate::recall::contract::SOURCE_KIND_DOCS, crate::recall::contract::SCHEMA_DOCS_MD_V1) => {
            Some(&["pointer"])
        }
        (_, SCHEMA_JSON_LEGACY_V1) | (_, SCHEMA_SQLITE_V1) => {
            Some(&["session_id", "message_id", "part_id"])
        }
        (_, SCHEMA_SQLITE_V2) => Some(&["session_id", "session_message_id", "part_id", "pointer"]),
        (SOURCE_KIND_CODEX, SCHEMA_CODEX_ROLLOUT_V1) => {
            Some(&["session_id", "message_id", "record_index", "pointer"])
        }
        (crate::recall::contract::SOURCE_KIND_CLAUDE_CODE, SCHEMA_CLAUDE_JSONL_V1) => {
            Some(&["session_id", "message_id", "record_index", "pointer"])
        }
        (crate::recall::contract::SOURCE_KIND_DSH, SCHEMA_DSH_JSONL) => {
            Some(&["session_id", "carrier", "record_index", "pointer"])
        }
        (
            crate::recall::contract::SOURCE_KIND_HERMES,
            crate::recall::contract::SCHEMA_HERMES_SQLITE_V1,
        ) => Some(&["session_id", "message_id", "record_index", "pointer"]),
        (crate::recall::contract::SOURCE_KIND_ARCHIVE, SCHEMA_ARCHIVE_RECORD_V1) => {
            Some(&["record_sha256", "projection_offset"])
        }
        _ => None,
    }
}

/// Validate the RFC 6901 shape of a `codex-rollout-v1` pointer.
///
/// The pointer is a *coordinate*, not an addressing language: readback
/// re-projects the line and compares the projected pointer byte for byte. Only
/// the shape is checked here (leading `/`, `~0`/`~1` escapes).
pub fn is_valid_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() || !pointer.starts_with('/') {
        return false;
    }
    pointer[1..].split('/').all(|token| {
        let mut chars = token.chars();
        while let Some(ch) = chars.next() {
            if ch == '~' && !matches!(chars.next(), Some('0') | Some('1')) {
                return false;
            }
        }
        true
    })
}

/// Validate a V2 JSON pointer (`/text` or `/content/{i}`) and return `i`.
pub fn parse_content_pointer(pointer: &str) -> Option<Option<usize>> {
    if pointer == "/text" {
        return Some(None);
    }
    let rest = pointer.strip_prefix("/content/")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if rest.len() > 1 && rest.starts_with('0') {
        return None;
    }
    rest.parse::<usize>().ok().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ref(rel_path: &str) -> HitRef {
        HitRef {
            source_id: "oc".into(),
            source_kind: "opencode".into(),
            root_sha256: "r".into(),
            rel_path: rel_path.into(),
            schema_gen: "sqlite-v1".into(),
            locator: Locator::v1("s1", "m1", "p1"),
            fingerprint: Fingerprint {
                range_sha256: "abc".into(),
                file_sha256: None,
                row_sha256: Some("row".into()),
            },
            text_range: TextRange::new(0, 4, 0, 1),
        }
    }

    #[test]
    fn hit_id_is_64_hex_and_rel_path_sensitive() {
        let a = hit_id(&sample_ref("opencode.db"));
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        let b = hit_id(&sample_ref("other.db"));
        assert_ne!(a, b, "rel_path must distinguish otherwise identical hits");
    }

    #[test]
    fn hit_id_ignores_row_fingerprint() {
        let mut a = sample_ref("opencode.db");
        let first = hit_id(&a);
        a.fingerprint.row_sha256 = Some("different".into());
        assert_eq!(first, hit_id(&a));
    }

    #[test]
    fn locator_never_contains_table_or_rowid() {
        for locator in [Locator::v1("s", "m", "p"), Locator::v2("s", "sm", "/text")] {
            let json = locator.compact_json();
            assert!(!json.contains("table"));
            assert!(!json.contains("rowid"));
        }
    }

    #[test]
    fn locator_serialization_is_unchanged_for_existing_generations() {
        assert_eq!(
            Locator::v1("s", "m", "p").compact_json(),
            r#"{"session_id":"s","message_id":"m","part_id":"p"}"#
        );
        assert_eq!(
            Locator::v2("s", "sm", "/text").compact_json(),
            r#"{"session_id":"s","session_message_id":"sm","pointer":"/text"}"#
        );
        assert_eq!(
            Locator::codex("s", None, 3, "/payload/content").compact_json(),
            r#"{"session_id":"s","record_index":3,"pointer":"/payload/content"}"#
        );
        assert_eq!(
            Locator::codex("s", Some("m"), 0, "/payload/output").compact_json(),
            r#"{"session_id":"s","message_id":"m","record_index":0,"pointer":"/payload/output"}"#
        );
    }

    #[test]
    fn v2_pointer_parsing() {
        assert_eq!(parse_content_pointer("/text"), Some(None));
        assert_eq!(parse_content_pointer("/content/0"), Some(Some(0)));
        assert_eq!(parse_content_pointer("/content/12"), Some(Some(12)));
        assert_eq!(parse_content_pointer("/content/01"), None);
        assert_eq!(parse_content_pointer("/content/"), None);
        assert_eq!(parse_content_pointer("/content/x"), None);
        assert_eq!(parse_content_pointer("/other"), None);
    }
}
