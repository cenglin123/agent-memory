//! Atomic writer for deletable local caches.
//!
//! `src/recall` reads sources and immutable archive objects; a tripwire test
//! (`tests/recall_boundary.rs`) keeps every filesystem write API out of that
//! layer. The live-source caches are the one deliberate exception, and they go
//! through this module instead: the only caller is the conversation vector
//! builder, which passes the configured `cache_root` plus path components that
//! are validated here (no absolute components, no `..`, no empty names).
//!
//! The helper never accepts a full path, so a caller cannot aim it at a source
//! tree or an Archive generation by accident.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum CacheWriteError {
    /// A component is empty, absolute or a parent-directory hop.
    UnsafeComponent(String),
    Io(String),
}

impl std::fmt::Display for CacheWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheWriteError::UnsafeComponent(component) => {
                write!(formatter, "unsafe cache path component: {component}")
            }
            CacheWriteError::Io(message) => write!(formatter, "cache write failed: {message}"),
        }
    }
}

fn component_is_safe(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.contains('/')
        && !component.contains('\\')
        && !component.contains(':')
}

/// `<root>/<components…>`, validated to stay inside `root`.
pub fn join_checked(root: &Path, components: &[&str]) -> Result<PathBuf, CacheWriteError> {
    let mut path = root.to_path_buf();
    for component in components {
        if !component_is_safe(component) {
            return Err(CacheWriteError::UnsafeComponent((*component).to_string()));
        }
        path.push(component);
    }
    Ok(path)
}

/// Write `data` to `<root>/<components…>` through a sibling temp file and a
/// same-volume rename. Returns the destination path.
pub fn write_atomic(
    root: &Path,
    components: &[&str],
    data: &[u8],
) -> Result<PathBuf, CacheWriteError> {
    let path = join_checked(root, components)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| CacheWriteError::Io(format!("{}: {error}", parent.display())))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CacheWriteError::UnsafeComponent(path.display().to_string()))?
        .to_string();
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        "{file_name}.{}.{counter}.tmp",
        std::process::id()
    ));
    std::fs::write(&temporary, data)
        .map_err(|error| CacheWriteError::Io(format!("{}: {error}", temporary.display())))?;
    std::fs::rename(&temporary, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        CacheWriteError::Io(format!("{}: {error}", path.display()))
    })?;
    Ok(path)
}

/// Append `data` to `<root>/<components…>`, creating the file and its parents
/// when missing. Like [`write_atomic`], the caller passes validated components,
/// never a full path, so it cannot aim the append at a source tree.
pub fn append(
    root: &Path,
    components: &[&str],
    data: &[u8],
) -> Result<PathBuf, CacheWriteError> {
    let path = join_checked(root, components)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| CacheWriteError::Io(format!("{}: {error}", parent.display())))?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| CacheWriteError::Io(format!("{}: {error}", path.display())))?;
    file.write_all(data)
        .map_err(|error| CacheWriteError::Io(format!("{}: {error}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_cannot_escape_the_root() {
        let root = Path::new("C:/cache");
        assert!(join_checked(root, &["live", "oc", "vectors.bin"]).is_ok());
        for bad in ["..", ".", "", "a/b", "a\\b", "C:"] {
            assert!(
                join_checked(root, &["live", bad]).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn write_is_atomic_and_replaces_the_previous_bytes() {
        let root = std::env::temp_dir().join(format!("am-cache-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = write_atomic(&root, &["live", "oc", "vectors.bin"], b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_atomic(&root, &["live", "oc", "vectors.bin"], b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
