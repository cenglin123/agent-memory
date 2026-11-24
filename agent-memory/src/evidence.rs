//! Read-side projections of committed SQLite evidence (stage-2 S5).
//!
//! The archive fallback in `src/recall/fulltext/fallback.rs` replays committed
//! baselines and delta batches to rebuild the row state of archived sessions.
//! This facade re-exports exactly the read projections it needs, so the recall
//! layer keeps importing evidence views — never the capture engine — and the
//! boundary tripwire keeps its meaning.

pub use crate::capture_sqlite::{
    read_chunk_rows_fetch, read_delta_lines_fetch, sqlite_baselines, sqlite_baselines_in,
    sqlite_delta_batch_metas, sqlite_delta_batch_metas_in, BaselineRowLine, DeltaLine, ObjectFetch,
    SqliteBaseline, SqliteDeltaBatchMeta,
};
