//! Unified read-only recall (P0/P1 slice).
//!
//! This module implements the parts of the "最小统一回忆" contract that can be
//! validated without touching real data:
//!
//! * the single explicit JSON configuration (`--config`, `version = 1`),
//! * hit identity (`hit_id`), references (`ref`), root/range fingerprints,
//! * OpenCode body search over legacy JSON storage, SQLite V1 and SQLite V2,
//! * `memory_read` with multi-segment (`segments[]`) readback,
//! * the source status / error code / exit code tables as code constants.
//!
//! KB (`ask.py --json`) and device adapters, the CLI and the stdio MCP wrapper
//! are P2/P3 and are intentionally absent: requesting them reports an explicit
//! `unsupported` source status instead of a silent empty result.
//!
//! Nothing here resolves default roots: every path comes from the explicit
//! configuration value, and `port/` only contains pure primitives
//! (`port` never references `providers`).

pub mod budget;
pub mod brief;
pub mod config;
pub mod device_meta;
pub mod docs;
pub mod contract;
pub mod domain;
pub mod eval;
pub mod fulltext;
pub mod graph;
pub mod hash;
pub mod ops;
pub mod pointer;
pub mod port;
pub mod providers;
pub mod read;
pub mod recent;
pub mod recent_hook;
pub mod refs;
pub mod search;
pub mod semantic;
pub mod snapshot;
pub mod telemetry;
pub mod text;

pub use budget::Budget;
pub use config::{
    load_config, InterpreterConfig, KbInterpreter, RecallConfig, SourceConfig, SourceKind,
};
pub use contract::{codes, status};
pub use domain::{classify_access, AccessDecision, DEFAULT_DOMAIN};
pub use ops::{execute, ExecutionOptions, Op};
pub use read::{memory_read, memory_read_with, ReadOptions, ReadRequest, ReadResponse};
pub use recent::{memory_recent, memory_recent_with, RecentOptions, RecentRequest, RecentResponse};
pub use search::{
    memory_search, memory_search_with, SearchOptions, SearchRequest, SearchResponse,
};
pub use telemetry::{EventsResponse, TelemetryConfig};
