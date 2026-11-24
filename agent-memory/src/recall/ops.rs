//! Shared execution of the two read-only operations.
//!
//! The CLI and the stdio MCP wrapper both call [`execute`], so `code`, exit
//! codes, pagination and `isError` cannot drift between transports: the MCP
//! layer only adds a transport envelope around the same [`Executed`] value.

use crate::recall::config::RecallConfig;
use crate::recall::contract::{codes, is_error_code, RecallError};
use crate::recall::read::{memory_read_with, ReadOptions, ReadRequest, ReadResponse};
use crate::recall::recent::{memory_recent_with, RecentOptions, RecentRequest, RecentResponse};
use crate::recall::search::{memory_search_with, SearchOptions, SearchRequest, SearchResponse};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    Read,
    Recent,
}

impl Op {
    pub fn parse(name: &str) -> Option<Op> {
        match name {
            "memory_search" => Some(Op::Search),
            "memory_read" => Some(Op::Read),
            "memory_recent" => Some(Op::Recent),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Op::Search => "memory_search",
            Op::Read => "memory_read",
            Op::Recent => "memory_recent",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecutionOptions {
    pub strict_snapshot: bool,
}

#[derive(Debug, Clone)]
pub struct Executed {
    pub op: Op,
    /// Frozen response envelope (search or read), ready to serialize.
    pub value: Value,
    pub exit_code: i32,
    /// §3.3: `REF_*`, `INVALID_REQUEST`, `INVALID_OFFSET`,
    /// `KB_CONTRACT_UNSUPPORTED` are errors; `partial`/`truncated`/`warnings`
    /// are not.
    pub is_error: bool,
}

impl Executed {
    pub fn json(&self) -> String {
        serde_json::to_string(&self.value).expect("response envelope serializes")
    }
}

pub fn execute(
    config: &RecallConfig,
    op: Op,
    request: &Value,
    options: &ExecutionOptions,
) -> Executed {
    match op {
        Op::Search => {
            let (response, is_error) = match SearchRequest::parse(request) {
                Ok(parsed) => {
                    let response = memory_search_with(
                        config,
                        &parsed,
                        &SearchOptions {
                            strict_snapshot: options.strict_snapshot,
                            eval_freeze_decay: false,
                        },
                    );
                    let is_error = response
                        .errors
                        .iter()
                        .any(|entry| is_error_code(entry.code));
                    (response, is_error)
                }
                Err(error) => (SearchResponse::request_failure(&error), true),
            };
            Executed {
                op,
                value: to_value(&response),
                exit_code: response.exit_code,
                is_error,
            }
        }
        Op::Read => {
            let (response, is_error) = match ReadRequest::parse(request) {
                Ok(parsed) => {
                    let response = memory_read_with(
                        config,
                        &parsed,
                        &ReadOptions {
                            strict_snapshot: options.strict_snapshot,
                        },
                    );
                    let is_error = !response.ok;
                    (response, is_error)
                }
                Err(error) => (ReadResponse::request_failure(&error), true),
            };
            Executed {
                op,
                value: to_value(&response),
                exit_code: response.exit_code,
                is_error,
            }
        }
        Op::Recent => {
            let (response, is_error) = match RecentRequest::parse(request) {
                Ok(parsed) => {
                    let response = memory_recent_with(
                        config,
                        &parsed,
                        &RecentOptions {
                            strict_snapshot: options.strict_snapshot,
                            ..RecentOptions::default()
                        },
                    );
                    let is_error = response
                        .errors
                        .iter()
                        .any(|entry| is_error_code(entry.code));
                    (response, is_error)
                }
                Err(error) => (RecentResponse::request_failure(&error), true),
            };
            Executed {
                op,
                value: to_value(&response),
                exit_code: response.exit_code,
                is_error,
            }
        }
    }
}

/// Configuration load failure rendered in the frozen request-failure shape, so
/// both transports report `INVALID_REQUEST` instead of a transport-specific
/// error string.
pub fn config_failure(op: Op, error: &RecallError) -> Executed {
    match op {
        Op::Search => {
            let response = SearchResponse::request_failure(error);
            Executed {
                op,
                value: to_value(&response),
                exit_code: response.exit_code,
                is_error: true,
            }
        }
        Op::Read => {
            let response = ReadResponse::request_failure(error);
            Executed {
                op,
                value: to_value(&response),
                exit_code: response.exit_code,
                is_error: true,
            }
        }
        Op::Recent => {
            let response = RecentResponse::request_failure(error);
            Executed {
                op,
                value: to_value(&response),
                exit_code: response.exit_code,
                is_error: true,
            }
        }
    }
}

pub fn invalid_request(op: Op, path: &str, reason: &str, message: &str) -> Executed {
    config_failure(
        op,
        &RecallError::invalid(path, reason, message.to_string()),
    )
}

pub fn unknown_operation(name: &str) -> Executed {
    let error = RecallError::invalid(
        "op",
        "unknown_operation",
        format!("unknown operation: {name}"),
    );
    Executed {
        op: Op::Search,
        value: serde_json::json!({
            "contract_version": crate::recall::contract::CONTRACT_VERSION,
            "ok": false,
            "code": codes::INVALID_REQUEST,
            "message": error.message,
        }),
        exit_code: 2,
        is_error: true,
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("response envelope serializes")
}
