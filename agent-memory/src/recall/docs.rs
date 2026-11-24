//! Process documents: the file set, the chunking rule and the glob matcher.
//!
//! Shared by the index builder and the scan fallback, so both answer with the
//! same chunks and the same refs. A docs source is recursive markdown; a file
//! is split at level-1..3 headings outside fenced code blocks.

use crate::recall::config::SourceConfig;
use std::path::{Path, PathBuf};

/// Default include when the source names none.
pub const DEFAULT_INCLUDE: [&str; 1] = ["**/*.md"];
/// Default excludes when the source names none: retired and discarded notes
/// must not silently re-enter retrieval.
pub const DEFAULT_EXCLUDES: [&str; 2] = ["_archive/**", "_trash/**"];
/// A chunk larger than this is split at the last line boundary below it.
pub const MAX_CHUNK_BYTES: usize = 4096;

/// One heading chunk inside a file: byte span plus the title path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start: usize,
    pub end: usize,
    pub title: String,
}

/// POSIX-ish glob, case-insensitive so `**/*.md` also matches `手册.MD` on
/// case-insensitive file systems. `**` crosses `/`, `*` and `?` do not.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let path = path.to_lowercase();
    fn matches(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p[0] == b'*' {
            if p.get(1) == Some(&b'*') {
                let mut rest = &p[2..];
                if rest.first() == Some(&b'/') {
                    rest = &rest[1..];
                    if matches(rest, s) {
                        return true;
                    }
                    for (index, byte) in s.iter().enumerate() {
                        if *byte == b'/' && matches(rest, &s[index + 1..]) {
                            return true;
                        }
                    }
                    return false;
                }
                if rest.is_empty() {
                    return true;
                }
                return (0..=s.len()).any(|index| matches(rest, &s[index..]));
            }
            let rest = &p[1..];
            for index in 0..=s.len() {
                if index > 0 && s[index - 1] == b'/' {
                    break;
                }
                if matches(rest, &s[index..]) {
                    return true;
                }
            }
            return false;
        }
        if p[0] == b'?' {
            return !s.is_empty() && s[0] != b'/' && matches(&p[1..], &s[1..]);
        }
        !s.is_empty() && s[0] == p[0] && matches(&p[1..], &s[1..])
    }
    matches(pattern.as_bytes(), path.as_bytes())
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = std::fs::read_dir(root)?;
    let mut paths: Vec<(PathBuf, bool)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        paths.push((entry.path(), file_type.is_dir()));
    }
    paths.sort();
    for (path, is_dir) in paths {
        if is_dir {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether a root-relative path belongs to the docs file set. Search and
/// readback both call this rule so a forged ref cannot bypass hidden-segment or
/// include/exclude filtering.
pub fn admit_rel_path(rel_path: &str, include: &[String], exclude: &[String]) -> bool {
    let rel = rel_path.replace('\\', "/");
    if rel.split('/').any(|part| part.starts_with('.')) {
        return false;
    }
    let included = if include.is_empty() {
        DEFAULT_INCLUDE
            .iter()
            .any(|pattern| glob_match(pattern, &rel))
    } else {
        include.iter().any(|pattern| glob_match(pattern, &rel))
    };
    if !included {
        return false;
    }
    if exclude.is_empty() {
        !DEFAULT_EXCLUDES
            .iter()
            .any(|pattern| glob_match(pattern, &rel))
    } else {
        !exclude.iter().any(|pattern| glob_match(pattern, &rel))
    }
}

/// The docs markdown set: recursive, hidden segments skipped, `include`
/// (default `**/*.md`) then `exclude` (default `_archive/**`, `_trash/**`).
/// Patterns are matched against the path relative to the source root.
pub fn files(source: &SourceConfig) -> std::io::Result<Vec<(String, PathBuf)>> {
    files_at(&source.root, &source.include, &source.exclude)
}

/// The same set against an explicit root (a strict-snapshot copy) with the
/// source's own patterns.
pub fn files_at(
    root: &str,
    include: &[String],
    exclude: &[String],
) -> std::io::Result<Vec<(String, PathBuf)>> {
    let root = Path::new(root);
    if !root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("docs root cannot be read: {}", root.display()),
        ));
    }
    let mut paths = Vec::new();
    collect_files(root, &mut paths)?;
    let root_str = root.to_string_lossy().replace('\\', "/");
    let root_trimmed = root_str.trim_end_matches('/');
    let mut files = Vec::new();
    for path in paths {
        let rel = path
            .to_string_lossy()
            .replace('\\', "/")
            .strip_prefix(&format!("{root_trimmed}/"))
            .map(str::to_string);
        let Some(rel) = rel else {
            continue;
        };
        if !admit_rel_path(&rel, include, exclude) {
            continue;
        }
        files.push((rel, path));
    }
    files.sort();
    Ok(files)
}

/// The file's display title: its first `# ` heading, else the file stem.
pub fn file_title(rel: &str, body: &str) -> String {
    for line in body.lines().take(40) {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }
    Path::new(rel)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(rel)
        .to_string()
}

fn section_title(file_title: &str, stack: &[(usize, String)]) -> String {
    // The H1 usually repeats the file title; do not print it twice.
    let mut parts: Vec<&str> = stack.iter().map(|(_, text)| text.as_str()).collect();
    if parts
        .first()
        .map(|part| *part == file_title)
        .unwrap_or(false)
    {
        parts.remove(0);
    }
    if parts.is_empty() {
        return file_title.to_string();
    }
    format!("{file_title} \u{203a} {}", parts.join(" \u{203a} "))
}

/// A fence line: three or more backticks or tildes, optionally followed by an
/// info string (which only matters on the opening line).
fn fence_marker(trimmed: &str) -> Option<(char, usize, bool)> {
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|ch| *ch == first).count();
    if run < 3 {
        return None;
    }
    let rest = trimmed[run..].trim();
    Some((first, run, rest.is_empty()))
}

/// Split one markdown body into heading chunks, ignoring headings inside
/// fenced code blocks and splitting oversized sections at line boundaries.
pub fn chunks(body: &str, file_title: &str) -> Vec<Chunk> {
    let mut sections: Vec<Chunk> = Vec::new();
    // Open fence: marker character and length; a closing fence uses the same
    // character, at least the same length, and no info string.
    let mut fence: Option<(char, usize)> = None;
    let mut current_start = 0usize;
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let text = line.trim_end_matches(['\r', '\n']);
        let trimmed = text.trim_start();
        if let Some((character, run, bare)) = fence_marker(trimmed) {
            match fence {
                Some((open, open_len)) if character == open && run >= open_len && bare => {
                    fence = None;
                }
                None => fence = Some((character, run)),
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let level = trimmed.chars().take_while(|ch| *ch == '#').count();
        if !(1..=3).contains(&level) || trimmed.chars().nth(level) != Some(' ') {
            continue;
        }
        let heading = trimmed[level..].trim().to_string();
        if start > current_start {
            sections.push(Chunk {
                start: current_start,
                end: start,
                title: section_title(file_title, &stack),
            });
        }
        while stack
            .last()
            .map(|(open_level, _)| *open_level >= level)
            .unwrap_or(false)
        {
            stack.pop();
        }
        stack.push((level, heading));
        current_start = start;
    }
    sections.push(Chunk {
        start: current_start,
        end: body.len(),
        title: section_title(file_title, &stack),
    });

    let mut chunks = Vec::new();
    for section in sections {
        let mut start = section.start;
        while start < section.end {
            let mut end = (start + MAX_CHUNK_BYTES).min(section.end);
            if end < section.end {
                if let Some(position) = body.as_bytes()[start..end]
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                {
                    if position > 0 {
                        end = start + position + 1;
                    }
                }
            }
            while end > start && !body.is_char_boundary(end) {
                end -= 1;
            }
            if end > start {
                chunks.push(Chunk {
                    start,
                    end,
                    title: section.title.clone(),
                });
            }
            start = end;
        }
    }
    chunks.retain(|chunk| !body[chunk.start..chunk.end].trim().is_empty());
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_is_case_insensitive_and_segment_aware() {
        assert!(glob_match("**/*.md", "a.md"));
        assert!(glob_match("**/*.md", "x/y/a.md"));
        assert!(glob_match("**/*.md", "x/手册.MD"));
        assert!(!glob_match("*.md", "x/a.md"));
        assert!(glob_match("_archive/**", "_archive/a/b.md"));
        assert!(!glob_match("_archive/**", "archive/a.md"));
    }

    #[test]
    fn headings_inside_fences_do_not_split() {
        let body = "# T\n\n```txt\n## not a heading\n```\n## Real\nbody\n";
        let chunks = chunks(body, "T");
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(chunks[1].title.contains("Real"));
    }

    #[test]
    fn an_unclosed_fence_keeps_the_rest_as_one_section() {
        let body = "# T\n```\n## still code\n";
        let chunks = chunks(body, "T");
        assert_eq!(chunks.len(), 1, "{chunks:?}");
    }
}
