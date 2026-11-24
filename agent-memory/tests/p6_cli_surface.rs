//! P6: CLI surface contract used by offline installation and audit.
//!
//! The suite audit runs the *installed* binary without a runtime root, so the
//! informational surface must work on its own: `--help` and `--version` exit 0
//! and print to stdout, while a bare invocation keeps the stable usage error.
//! No test here touches a real session directory.

use std::process::Command;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

#[test]
fn help_exits_zero_and_lists_every_command() {
    for flag in ["--help", "-h", "help"] {
        let out = cli().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag} must exit 0");
        assert!(out.stderr.is_empty(), "{flag} must not write stderr");
        let text = String::from_utf8(out.stdout).unwrap();
        for command in [
            "init",
            "capture",
            "read",
            "status",
            "reconcile",
            "coverage",
            "replay",
            "scrub",
            "derived-read",
            "attach",
            "attachments",
            "attachment-read",
            "memory_search",
            "memory_read",
            "memory_recent",
            "mcp",
        ] {
            assert!(
                text.contains(command),
                "{flag} output must document `{command}`: {text}"
            );
        }
        assert!(
            text.contains("--root <runtime-root>"),
            "{flag} output must document the runtime-root flag: {text}"
        );
    }
}

#[test]
fn version_exits_zero_and_prints_crate_version() {
    for flag in ["--version", "-V", "version"] {
        let out = cli().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag} must exit 0");
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            text.trim(),
            format!("agent-memory {}", env!("CARGO_PKG_VERSION")),
            "{flag} must print the crate version verbatim"
        );
    }
}

#[test]
fn bare_invocation_stays_a_usage_error() {
    let out = cli().output().unwrap();
    assert_eq!(out.status.code(), Some(2), "usage errors keep exit code 2");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("\"ok\":false"), "usage error is JSON on stderr: {stderr}");
    assert!(out.stdout.is_empty(), "usage error writes nothing to stdout");
}

#[test]
fn unknown_command_stays_a_usage_error() {
    let out = cli().arg("no-such-command").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("usage: agent-memory"));
}
