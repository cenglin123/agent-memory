//! Device-note frontmatter: the small YAML subset authored by the memory
//! skill. Deliberately not a YAML parser: flat `key: value` lines, inline
//! lists, quoted or bare scalars. An unclosed block is body text, never a
//! silent content loss.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Default, Clone)]
pub struct DeviceMeta {
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub status: Option<String>,
    pub verified_at: Option<String>,
}

pub fn split(text: &str) -> (DeviceMeta, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return (DeviceMeta::default(), text);
    };
    if first.trim_end_matches(['\r', '\n']).trim() != "---" {
        return (DeviceMeta::default(), text);
    }
    let mut meta = DeviceMeta::default();
    let mut offset = first.len();
    for line in lines {
        offset += line.len();
        let content = line.trim_end_matches(['\r', '\n']);
        if content.trim() == "---" {
            return (meta, &text[offset..]);
        }
        let Some((key, value)) = content.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        match key {
            "summary" => meta.summary = scalar(value),
            "tags" => meta.tags = list(value),
            "links" => meta.links = list(value),
            "status" => meta.status = scalar(value),
            "verified_at" => meta.verified_at = scalar(value),
            _ => {}
        }
    }
    (DeviceMeta::default(), text)
}

fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value == "null" || value == "~" {
        return None;
    }
    let value = unquote(value);
    if value.is_empty() { None } else { Some(value) }
}

fn list(value: &str) -> Vec<String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value);
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in inner.chars() {
        match quote {
            Some(q) => {
                current.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '"' | '\'' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                ',' | '，' => push_item(&mut items, &mut current),
                _ => current.push(ch),
            },
        }
    }
    push_item(&mut items, &mut current);
    items
}

fn push_item(items: &mut Vec<String>, current: &mut String) {
    let item = unquote(current.trim());
    if !item.is_empty() {
        items.push(item);
    }
    current.clear();
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
        }
    }
    value.to_string()
}

pub fn title(meta: &DeviceMeta, body: &str, fallback: &str) -> String {
    if let Some(summary) = meta.summary.as_deref() {
        if !summary.trim().is_empty() {
            return summary.trim().to_string();
        }
    }
    if let Some(heading) = first_heading(body) {
        return heading;
    }
    fallback.to_string()
}

fn first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("# ") else {
            continue;
        };
        let heading = rest.trim();
        if !heading.is_empty() {
            return Some(heading.to_string());
        }
    }
    None
}

pub fn tags(meta: &DeviceMeta, body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tag in meta.tags.iter() {
        push_unique(&mut out, tag);
    }
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let re = TAG_RE.get_or_init(|| {
        Regex::new(r"(?:^|[\s])#([\p{Han}\w-]{1,32})").expect("device hashtag regex")
    });
    for cap in re.captures_iter(body) {
        push_unique(&mut out, &cap[1]);
    }
    out
}

pub fn links(meta: &DeviceMeta, body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for link in meta.links.iter() {
        push_unique_path(&mut out, link);
    }
    static LINK_RE: OnceLock<Regex> = OnceLock::new();
    let re = LINK_RE.get_or_init(|| {
        Regex::new(r"\[\[([^\]\|#]+)").expect("device wikilink regex")
    });
    for cap in re.captures_iter(body) {
        push_unique_path(&mut out, &cap[1]);
    }
    out
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() && !out.iter().any(|item| item == value) {
        out.push(value.to_string());
    }
}

fn push_unique_path(out: &mut Vec<String>, value: &str) {
    let mut value = value.trim().to_string();
    if value.to_ascii_lowercase().ends_with(".md") {
        value.truncate(value.len() - 3);
    }
    if !value.is_empty() && !out.iter().any(|item| item == &value) {
        out.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_frontmatter_and_inline_lists() {
        let text = "---\r\nschema: 1\r\nsummary: \"摘要：A, B\"\r\ntags: [网络, \"BT\"]\r\nstatus: active\r\nverified_at: null\r\n---\r\n正文 #额外 [[2026-10-01_x]]\r\n";
        let (meta, body) = split(text);
        assert_eq!(meta.summary.as_deref(), Some("摘要：A, B"));
        assert_eq!(meta.tags, vec!["网络", "BT"]);
        assert_eq!(meta.status.as_deref(), Some("active"));
        assert!(meta.verified_at.is_none());
        assert!(body.starts_with("正文"));
        assert_eq!(tags(&meta, body), vec!["网络", "BT", "额外"]);
        assert_eq!(links(&meta, body), vec!["2026-10-01_x"]);
    }

    #[test]
    fn unclosed_block_is_body() {
        let text = "---\nsummary: x\nbody";
        let (meta, body) = split(text);
        assert!(meta.summary.is_none());
        assert_eq!(body, text);
    }
}
