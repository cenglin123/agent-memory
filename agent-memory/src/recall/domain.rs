//! Access-domain selection and the Archive read predicate (§3.2).
//!
//! The effective domain is `request > configuration > default`. An Archive
//! record is readable **iff** its access label set is non-empty and the
//! effective domain is one of those labels. Empty labels fail closed. A
//! non-default label set is unreachable under the implicit `default` domain:
//! the source reports `unavailable` with a denial code instead of a healthy
//! empty result.

use crate::recall::contract::{codes, status, RecallError};
use serde_json::Value;

pub const DEFAULT_DOMAIN: &str = "default";
pub const MAX_DOMAIN_LEN: usize = 64;

/// Parse and validate a domain string. Domains are lowercase, ASCII and
/// deliberately small so they can be compared verbatim across providers.
pub fn parse_domain_value(value: &Value, path: &str) -> Result<String, RecallError> {
    let domain = match value {
        Value::String(domain) if !domain.is_empty() => domain,
        Value::String(_) => {
            return Err(RecallError::invalid(
                path,
                "empty_value",
                "domain must not be empty",
            ))
        }
        _ => {
            return Err(RecallError::invalid(
                path,
                "invalid_type",
                format!("{path} must be a string"),
            ))
        }
    };
    if !is_valid_domain(domain) {
        return Err(RecallError::invalid(
            path,
            "invalid_domain",
            format!(
                "domain must match ^[a-z0-9][a-z0-9._-]{{0,{}}}",
                MAX_DOMAIN_LEN - 1
            ),
        ));
    }
    Ok(domain.clone())
}

pub fn is_valid_domain(domain: &str) -> bool {
    let bytes = domain.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_DOMAIN_LEN {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'-')
    })
}

/// Outcome of the Archive access predicate. `Denied` and `Unreachable` both
/// map to `unavailable` + a denial code; neither is a healthy empty result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    Readable,
    Denied(&'static str),
    Unreachable(&'static str),
}

impl AccessDecision {
    pub fn is_readable(&self) -> bool {
        matches!(self, AccessDecision::Readable)
    }

    /// `(status, code)` for a rejected record. `None` when readable.
    pub fn rejection(&self) -> Option<(&'static str, &'static str)> {
        match self {
            AccessDecision::Readable => None,
            AccessDecision::Denied(code) | AccessDecision::Unreachable(code) => {
                Some((status::UNAVAILABLE, code))
            }
        }
    }
}

/// Evaluate the frozen Archive read predicate.
///
/// * empty `access` -> `DOMAIN_DENIED` (fail-closed)
/// * `effective` in `access` -> readable
/// * implicit `default` with a non-default-only label set -> `DOMAIN_UNREACHABLE`
/// * otherwise -> `DOMAIN_DENIED`
pub fn classify_access(
    access: &[String],
    effective: &str,
    explicit_selection: bool,
) -> AccessDecision {
    if access.is_empty() {
        return AccessDecision::Denied(codes::DOMAIN_DENIED);
    }
    if access.iter().any(|label| label == effective) {
        return AccessDecision::Readable;
    }
    if !explicit_selection && effective == DEFAULT_DOMAIN {
        return AccessDecision::Unreachable(codes::DOMAIN_UNREACHABLE);
    }
    AccessDecision::Denied(codes::DOMAIN_DENIED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn domain_values_are_lowercase_and_bounded() {
        for valid in ["default", "work", "work-2026", "a.b_c", "a1"] {
            assert!(is_valid_domain(valid), "{valid} must be valid");
        }
        for invalid in ["", "Default", "-work", "work space", "工作", "a/b"] {
            assert!(!is_valid_domain(invalid), "{invalid} must be invalid");
        }
        assert!(!is_valid_domain(&"a".repeat(MAX_DOMAIN_LEN + 1)));
    }

    #[test]
    fn empty_labels_fail_closed() {
        assert_eq!(
            classify_access(&[], "default", false),
            AccessDecision::Denied(codes::DOMAIN_DENIED)
        );
        assert_eq!(
            classify_access(&[], "work", true),
            AccessDecision::Denied(codes::DOMAIN_DENIED)
        );
    }

    #[test]
    fn membership_is_required() {
        assert_eq!(
            classify_access(&labels(&["work", "default"]), "default", false),
            AccessDecision::Readable
        );
        assert_eq!(
            classify_access(&labels(&["work"]), "work", true),
            AccessDecision::Readable
        );
        assert_eq!(
            classify_access(&labels(&["work"]), "private", true),
            AccessDecision::Denied(codes::DOMAIN_DENIED)
        );
    }

    #[test]
    fn non_default_labels_are_unreachable_without_an_explicit_domain() {
        assert_eq!(
            classify_access(&labels(&["work"]), "default", false),
            AccessDecision::Unreachable(codes::DOMAIN_UNREACHABLE)
        );
        // A configured non-default domain is an explicit selection.
        assert_eq!(
            classify_access(&labels(&["work"]), "work", true),
            AccessDecision::Readable
        );
    }

    #[test]
    fn rejections_map_to_unavailable_and_a_denial_code() {
        assert_eq!(AccessDecision::Readable.rejection(), None);
        assert_eq!(
            AccessDecision::Denied(codes::DOMAIN_DENIED).rejection(),
            Some((status::UNAVAILABLE, codes::DOMAIN_DENIED))
        );
        assert_eq!(
            AccessDecision::Unreachable(codes::DOMAIN_UNREACHABLE).rejection(),
            Some((status::UNAVAILABLE, codes::DOMAIN_UNREACHABLE))
        );
    }
}
