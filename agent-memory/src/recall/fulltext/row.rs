//! One index row, shared by the exact (`query`) and hybrid (`hybrid`) paths.
//!
//! The column list is fixed: both paths select [`ROW_COLUMNS`] and may append
//! their own trailing score column. Refs are rebuilt from the stored locator
//! (with the coordinate fallback used when a locator cannot be parsed), so a
//! hit from either path reads back through `memory_read` with the same range
//! and fingerprint checks.

use crate::recall::config::{SourceConfig, SourceKind};
use crate::recall::contract::{
    RANK_BASIS_FULLTEXT_INDEX, SCORE_KIND_NONE, UNIT_KIND_TOOL_INPUT, UNIT_KIND_TOOL_OUTPUT,
};
use crate::recall::fulltext::{Term, ORIGIN_ARCHIVE};
use crate::recall::hash::TextRange;
use crate::recall::providers::{ExactMeta, SourceHit};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::text::{fold_ascii, snippet};
use rusqlite::Connection;

pub(crate) const ROW_COLUMNS: &str = "rowid, text, tool_input, tool_output, session_id, ts, role, \
     kind, session_kind, title, rel_path, message_id, record_index, pointer, locator, range_len, \
     range_lines, range_sha256, row_sha256, origin";

pub(crate) struct Row {
    pub(crate) rowid: i64,
    pub(crate) text: String,
    pub(crate) tool_input: String,
    pub(crate) tool_output: String,
    pub(crate) session_id: String,
    pub(crate) ts: i64,
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) session_kind: String,
    pub(crate) title: String,
    pub(crate) rel_path: String,
    pub(crate) message_id: Option<String>,
    pub(crate) record_index: Option<u64>,
    pub(crate) pointer: Option<String>,
    pub(crate) locator: Option<String>,
    pub(crate) range_len: u64,
    pub(crate) range_lines: u64,
    pub(crate) range_sha256: String,
    pub(crate) row_sha256: Option<String>,
    pub(crate) origin: String,
}

impl Row {
    /// Read [`ROW_COLUMNS`] starting at `base` (the rowid sits at `base + 0`).
    pub(crate) fn from_query_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
        Ok(Row {
            rowid: row.get::<_, i64>(0)?,
            text: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            tool_input: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            tool_output: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            session_id: row.get::<_, String>(4)?,
            ts: row.get::<_, i64>(5).unwrap_or(0),
            role: row.get::<_, String>(6)?,
            kind: row.get::<_, String>(7)?,
            session_kind: row.get::<_, String>(8)?,
            title: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
            rel_path: row.get::<_, String>(10)?,
            message_id: row.get::<_, Option<String>>(11)?,
            record_index: row
                .get::<_, Option<i64>>(12)?
                .and_then(|value| u64::try_from(value).ok()),
            pointer: row.get::<_, Option<String>>(13)?,
            locator: row.get::<_, Option<String>>(14)?,
            range_len: row.get::<_, i64>(15).unwrap_or(0).max(0) as u64,
            range_lines: row.get::<_, i64>(16).unwrap_or(0).max(0) as u64,
            range_sha256: row.get::<_, String>(17)?,
            row_sha256: row.get::<_, Option<String>>(18)?,
            origin: row
                .get::<_, Option<String>>(19)?
                .unwrap_or_else(|| "live".to_string()),
        })
    }

    /// The indexed body of this row's channel.
    pub(crate) fn unit_text(&self) -> &str {
        match self.kind.as_str() {
            UNIT_KIND_TOOL_INPUT => &self.tool_input,
            UNIT_KIND_TOOL_OUTPUT => &self.tool_output,
            _ => &self.text,
        }
    }

    /// Rebuild the ref locator from the stored coordinates. The index is keyed
    /// by the configured source, so the locator shape follows the source kind.
    pub(crate) fn locator_for(&self, kind: &SourceKind) -> Locator {
        if let Some(stored) = self.locator.as_deref() {
            if let Ok(locator) = serde_json::from_str::<Locator>(stored) {
                return locator;
            }
        }
        match kind {
            SourceKind::Opencode => Locator::v2(
                &self.session_id,
                self.message_id.as_deref().unwrap_or_default(),
                self.pointer.as_deref().unwrap_or("/text"),
            ),
            SourceKind::Codex | SourceKind::ClaudeCode | SourceKind::Dsh | SourceKind::Hermes => {
                Locator::codex(
                    &self.session_id,
                    self.message_id.as_deref(),
                    self.record_index.unwrap_or(0),
                    self.pointer.as_deref().unwrap_or("/"),
                )
            }
            SourceKind::Kb | SourceKind::Device | SourceKind::Docs | SourceKind::Archive => {
                Locator::empty()
            }
        }
    }

    /// Rebuild the read-back reference of this row. The ref names the
    /// generation its locator resolves in. Live rows always carry the source's
    /// current generation; an OpenCode archive-fallback row can be a V1 part
    /// (`session_id` + `message_id` + `part_id`) rebuilt from evidence, and
    /// must read back through the V1 reader.
    pub(crate) fn build_ref(&self, source: &SourceConfig) -> HitRef {
        let locator = self.locator_for(&source.kind);
        let schema_gen = match &source.kind {
            SourceKind::Opencode
                if self.origin == ORIGIN_ARCHIVE && locator.part_id.is_some() =>
            {
                crate::recall::contract::SCHEMA_SQLITE_V1
            }
            _ => super::query::schema_gen_for(source),
        };
        HitRef {
            source_id: source.source_id.clone(),
            source_kind: source.kind.as_str().to_string(),
            root_sha256: crate::recall::hash::root_sha256(&source.root),
            rel_path: self.rel_path.clone(),
            schema_gen: schema_gen.to_string(),
            locator,
            fingerprint: Fingerprint {
                range_sha256: self.range_sha256.clone(),
                file_sha256: None,
                row_sha256: self.row_sha256.clone(),
            },
            text_range: TextRange::new(0, self.range_len, 0, self.range_lines),
        }
    }

    /// Build a hit without a rank: `score`, `score_kind` and `rank_basis` are the
    /// caller's decision (bm25 for exact relevance, RRF for hybrid).
    ///
    /// `mark_origin` is the index's fallback flag: only a fallback-armed index
    /// names `origin` on its hits, so an index built without an archive source
    /// answers byte-identically to the pre-S5 shape.
    pub(crate) fn into_hit(
        self,
        source: &SourceConfig,
        terms: &[Term],
        snippet_bytes: usize,
        mark_origin: bool,
    ) -> SourceHit {
        let unit_text = self.unit_text().to_string();
        let folded = fold_ascii(&unit_text);
        let mut distinct = 0usize;
        let mut first: Option<(usize, usize)> = None;
        for term in terms {
            let needle = fold_ascii(&term.text);
            if let Some(offset) = folded.find(needle.as_str()) {
                distinct += 1;
                first = Some(match first {
                    Some((current, current_len)) if current <= offset => (current, current_len),
                    _ => (offset, needle.len()),
                });
            }
        }
        let (first_byte, first_term_len) = first.unwrap_or((0, 1));
        let (display, snippet_truncated) =
            snippet(&unit_text, first_byte, first_term_len, snippet_bytes);
        let reference = self.build_ref(source);
        SourceHit {
            hit: reference,
            title: if self.title.is_empty() {
                self.session_id.clone()
            } else {
                self.title
            },
            snippet: display,
            snippet_truncated,
            score: None,
            score_kind: SCORE_KIND_NONE,
            rank_basis: RANK_BASIS_FULLTEXT_INDEX,
            distinct_terms: distinct,
            first_byte,
            sort_time: self.ts,
            sort_id: format!("{}:{}", self.session_id, self.kind),
            related: Vec::new(),
            exact: Some(ExactMeta {
                timestamp_ms: self.ts,
                role: self.role,
                kind: self.kind,
                session_kind: self.session_kind,
                session_hits: None,
                channels: None,
                origin: if mark_origin {
                    if self.origin == ORIGIN_ARCHIVE {
                        Some(crate::recall::contract::ORIGIN_ARCHIVE)
                    } else {
                        Some(crate::recall::contract::ORIGIN_LIVE)
                    }
                } else {
                    None
                },
            }),
        }
    }

    /// Load rows by rowid in one statement (hybrid's semantic-only candidates).
    pub(crate) fn load_by_rowids(
        conn: &Connection,
        rowids: &[i64],
    ) -> rusqlite::Result<Vec<Row>> {
        if rowids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = (0..rowids.len()).map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM units WHERE rowid IN ({})",
            placeholders.join(", ")
        );
        let mut statement = conn.prepare(&sql)?;
        let params: Vec<i64> = rowids.to_vec();
        let rows = statement.query_map(rusqlite::params_from_iter(params), Row::from_query_row)?;
        rows.collect()
    }
}
