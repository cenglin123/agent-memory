//! Path spelling normalisation, kept outside `recall` so the recall layer never
//! resolves physical paths itself (see `tests/recall_boundary.rs`).
//!
//! Windows can name one directory two ways: the 8.3 short name
//! (`C:\Users\RUNNER~1\AppData\Local\Temp`) and the long name
//! (`C:\Users\runneradmin\AppData\Local\Temp`). A configuration path may carry
//! one and a capture-time path the other, so a plain string prefix comparison
//! silently stops matching. This module yields one spelling for a path that may
//! no longer exist (a deleted carrier normalises through its nearest existing
//! ancestor).

use std::path::{Path, PathBuf};

/// Canonical spelling of `path` as forward-slash text, with the Windows `\\?\`
/// verbatim prefix removed. Returns `None` only when no ancestor exists.
pub(crate) fn canonical_spelling(path: &Path) -> Option<String> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current: PathBuf = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            let mut resolved = canonical;
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            let text = resolved.to_string_lossy().to_string();
            let text = text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string();
            return Some(text.replace('\\', "/"));
        }
        let name = current.file_name()?.to_os_string();
        tail.push(name);
        if !current.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_leaf_normalises_through_its_parent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = dir.path().join("proj");
        std::fs::create_dir_all(&nested).expect("proj");
        let file = nested.join("carrier.jsonl");
        std::fs::write(&file, b"{}\n").expect("write");
        std::fs::remove_file(&file).expect("remove");

        let dotted = format!("{}/./proj/..", dir.path().display());
        let root = canonical_spelling(Path::new(&dotted)).expect("root spelling");
        let child = canonical_spelling(&file).expect("child spelling");
        assert!(child.to_ascii_lowercase().starts_with(&root.to_ascii_lowercase()));
        assert!(child.ends_with("proj/carrier.jsonl"));
        assert!(!child.starts_with("\\\\?\\"));
    }
}
