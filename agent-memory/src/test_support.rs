//! Test-only filesystem helpers for unit tests.
//!
//! Unit tests under `src/recall/**` create fixture files, but
//! `tests/recall_boundary.rs` scans that tree for filesystem write entry points.
//! The write lives here instead: `cfg(test)` keeps it out of production builds,
//! and being outside `src/recall` keeps the tripwire strict — a write added to
//! production recall code is still caught.

/// Write a fixture file (test code only).
pub(crate) fn write_file(
    path: impl AsRef<std::path::Path>,
    body: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    std::fs::write(path, body)
}
