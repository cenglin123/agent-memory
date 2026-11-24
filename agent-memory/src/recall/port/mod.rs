//! Pure port primitives (no response codes, no coverage, no provider imports).
//!
//! `providers/` depends on `port/`; the reverse dependency is asserted absent
//! by `tests/recall_boundary.rs`.

pub mod blocks;
pub mod legacy_json;
pub mod model;
pub mod probe;
pub mod project;
pub mod reader;
pub mod sqlite_v1;
pub mod sqlite_v2;

pub use blocks::{PrefixDigest, PrefixFingerprint};
pub use model::{Health, PartData, PartRecord, PortError, SessionMeta};
pub use project::{
    classify_part, compact_json, is_injected, PartOutcome,
};
pub use reader::{read_line_bounded, resolve, BoundedLine, Reader, Resolved};
