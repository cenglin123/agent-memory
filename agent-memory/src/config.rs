//! Headless replacement for the cc-switch `crate::config` surface that the
//! ported session providers rely on.
//!
//! Differences from upstream (see NOTICE):
//! - no Tauri / app settings / GUI coupling;
//! - **no default real-root resolution.** Nothing in this crate resolves a
//!   session root or provider config against the real user home. The only
//!   ambient switch is `CC_SWITCH_TEST_HOME`, which authorizes a *synthetic*
//!   home for `~` expansion; when it is absent, home resolution fails closed
//!   (`None`) and callers must inject explicit roots/config instead. This is
//!   the single isolation point ("统一隔离解析"): the OpenCode base-dir, Codex
//!   config dir and Claude config dir defaults are gone, so nothing can bypass
//!   it. Note that on Windows `dirs::home_dir()` is backed by
//!   `SHGetKnownFolderPath(FOLDERID_Profile)` and ignores `HOME` /
//!   `USERPROFILE`, so "set a temporary HOME" is not an isolation guarantee —
//!   which is exactly why this crate never calls `dirs::home_dir()`;
//! - `OPENCODE_DB` / `CODEX_SQLITE_HOME` / `CODEX_HOME` env redirection is NOT
//!   honored (upstream honored some of these); ambient env must never redirect
//!   this crate at a real session store;
//! - `atomic_write` uses `tempfile + rename` (same-volume rename is atomic on
//!   Windows and Unix); upstream's ReplaceFileW retry loop is not ported.
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use crate::error::AppError;

/// Fail-closed home resolution, used only for `~` expansion.
///
/// Returns the `CC_SWITCH_TEST_HOME` synthetic home, or `None` when the
/// override is absent. There is deliberately no real-home fallback (upstream's
/// `dirs::home_dir()` chain is not ported): without an explicitly authorized
/// synthetic root, home-relative paths stay unresolvable and callers must skip
/// them instead of guessing the user profile. On Windows a "temporary HOME /
/// USERPROFILE" would not help anyway, because `dirs::home_dir()` ignores both.
pub fn get_home_dir() -> Option<PathBuf> {
    test_home_override()
}

/// `CC_SWITCH_TEST_HOME` override (kept from upstream so ported tests keep
/// pointing at synthetic temp roots instead of real user directories). This is
/// the only ambient variable consulted for root resolution.
fn test_home_override() -> Option<PathBuf> {
    let raw = std::env::var_os("CC_SWITCH_TEST_HOME")?;
    let lossy = raw.to_string_lossy();
    let trimmed = lossy.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Whether `path` is a WSL share path (`\\wsl$\...` / `\\wsl.localhost\...`).
#[cfg(windows)]
pub(crate) fn is_wsl_path(path: &Path) -> bool {
    use std::path::Prefix;
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        match prefix.kind() {
            Prefix::UNC(server, _) | Prefix::VerbatimUNC(server, _) => {
                let s = server.to_string_lossy();
                s.eq_ignore_ascii_case("wsl$") || s.eq_ignore_ascii_case("wsl.localhost")
            }
            _ => false,
        }
    } else {
        false
    }
}

#[cfg(not(windows))]
pub(crate) fn is_wsl_path(_path: &Path) -> bool {
    false
}

/// Atomic write: write a unique sibling temp file, fsync, then rename over the
/// target (same-volume rename is atomic on Windows and Unix).
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::message("invalid path: no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| AppError::message("invalid path: no file name"))?
        .to_string_lossy()
        .to_string();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        "{file_name}.tmp.{}.{ts}.{counter}",
        std::process::id()
    ));

    let write_result = (|| -> Result<(), AppError> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| AppError::io(&tmp, e))?;
        file.write_all(data)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|e| AppError::io(&tmp, e))?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        AppError::io(path, e)
    })
}

/// SQLite cannot lock on WSL shares; ported tests skip in that case.
#[allow(dead_code)] // used from #[cfg(test)] code in the ported providers
pub(crate) fn sqlite_unsupported_in_temp_dir() -> bool {
    is_wsl_path(&std::env::temp_dir())
}
