# Codex unified recall — completion record

- Date: 2026-10-28
- Plan: `docs/plans/active/20261027-codex-unified-recall.md` (phases 1–3)
- Executor: deepseek-flash (Plan-Execution Executor)
- Real session data used: **no**（真实数据=否）— every fixture is synthetic and
  lives under a per-case temporary root. No Archive write, no capture, no
  `write_rollout`, no production runtime root, no Git commit (the executor does
  not commit).
- Distribution artifacts: **not rebuilt** and `build-manifest.json` **not
  modified** (reserved for the orchestrator).

## Implemented files

| File | Change |
|---|---|
| `src/recall/config.rs` | `SourceKind::Codex`; `kind = "codex"` accepts `db`/`storage`; kind error text lists four kinds; path containment unchanged |
| `src/recall/contract.rs` | `SCHEMA_CODEX_ROLLOUT_V1 = "codex-rollout-v1"`; `ALL_SCHEMA_GENS` 4→5; `SOURCE_KIND_CODEX` |
| `src/recall/refs.rs` | optional `Locator.record_index` (serialized only when set, so OpenCode locator JSON and `hit_id` are unchanged); `Locator::codex`; codex `locator_schema`; JSON-pointer shape validator |
| `src/recall/search.rs` | optional `session_id` (exact match, ≤200 characters, blanks/control chars rejected); filter passed to providers; filtered `coverage`; KB/device `empty` + `session_filter_not_applicable`; Codex strict-snapshot refusal; Codex dispatch |
| `src/recall/providers/codex.rs` | **new**: storage enumeration (`sessions` default, symlinks skipped, sibling dedupe, path order; top-level IO error → `codex_storage_unreadable`, subdirectory error → `partial`; standard filenames prefiltered by UUID before metadata read), metadata enumeration via `scan_session_file`, explicit titles, per-line projection, aligned search/read 64 MiB ceiling, charged oversize reads, lexical hits, budget/health/`ignored_types`, `coverage` |
| `src/recall/providers/opencode.rs` | filter after session enumeration, before part reads; `coverage.total_sessions` filtered; `coverage.session_filter` |
| `src/recall/providers/mod.rs` | registers the `codex` provider |
| `src/recall/read.rs` | codex schema check; strict-snapshot refusal; no observed-root window; carrier must be inside the configured `storage`; `read_codex` record/pointer/range re-projection, context windows, `max_bytes`, shared 64 MiB decompressed scan cap, bounded line reads, symlink refusal, plain/`.zst` sibling carrier, half-line tolerance |
| `src/providers/codex.rs` | `scan_session_file` and `load_thread_titles_from_paths` → `pub(crate)`; new `recall_user_text` (single injection predicate shared with the UI title logic) |
| `src/mcp.rs` | `memory_search.inputSchema.properties.session_id` (`string`, 1..200) |
| `tests/support/mod.rs` | Codex fixtures: `codex_source`, record builders, `build_codex_rollout[_in]`, `build_codex_zst`, `build_state_db`, `build_session_index`; `insert_v1` exposed |
| `tests/recall_codex.rs` | **new**: 31 Codex tests |
| `tests/recall_opencode.rs` | V1/V2/legacy `session_id` filter, zero hits, damaged non-target, budget, hit-id stability |
| `tests/recall_contract.rs` | Codex config/unknown keys/outside root, schema table, `session_id` validation, codex locator parsing |
| `tests/recall_cli_mcp.rs` | MCP schema declares `session_id` |
| `tests/recall_boundary.rs` | reverse tripwire: `src/recall` never references Archive/capture/`write_rollout`/write APIs |
| `tests/boundary_no_default_roots.rs` | decoy `~/.codex/sessions` under a locked synthetic home/`CODEX_HOME` is never discovered without an explicit codex source |
| `README.md` | configuration example, `session_id` semantics, Codex projection/limitations, strict-snapshot exception, test count |
| `CHANGELOG.md` | capability record for this change |

## Contract decisions

- Codex `root` is the Codex home. `storage` defaults to `<root>/sessions`;
  `archived_sessions` is read only when `storage` is explicitly pointed there.
  `db` defaults to `<root>/state_5.sqlite` when that file exists and is used for
  titles only.
- `schema_gen = "codex-rollout-v1"`; `rel_path` is the read-root-relative path
  with any `.zst` suffix removed. Locator:
  `{session_id, message_id?, record_index, pointer}`.
- `hit_id` formula is unchanged. Because the locator gains `record_index` only
  for Codex and the logical `rel_path` ignores compression, plain and zstd
  carriers of the same bytes share one `hit_id`.
- `session_id` is applied after session enumeration and before any body read.
  `coverage.total_sessions` is the filtered population; `coverage.session_filter`
  records the id; budget/truncation/partial semantics are unchanged. KB and
  device sources answer `empty` with `detail.reason =
  "session_filter_not_applicable"` (exit 0).
- Codex is a live append source: no observed-root `Window`, no `side_effects[]`.
  `--strict-snapshot` is refused (`DB_UNAVAILABLE`, reason
  `strict_snapshot_unsupported_for_codex_home`) before any copy is attempted.

## Test result

`cargo test --locked` → **413 passed / 1 ignored** (previous baseline: 368
passed / 1 ignored; 45 new tests, no regression).

New test counts: `recall_codex.rs` 31, `recall_contract.rs` +4,
`recall_opencode.rs` +3, `recall_cli_mcp.rs` +1, `recall_boundary.rs` +1,
`boundary_no_default_roots.rs` +1, `refs.rs` unit +1, `port/reader.rs` unit +3.

Reverse cases exercised:

- Configuration: unknown key, `db`/`storage` outside root, relative segment,
  `source_order` mismatch, invalid kind literal.
- Request: empty/whitespace/too-long/control-character/non-string `session_id`;
  codex locator without `record_index`, with a malformed pointer, or with an
  extra key.
- `session_id`: V1/V2/legacy filtering; zero-hit `empty` with exit 0 and no
  budget detail; a content-damaged non-target session does not turn the target
  `partial`; an unreadable non-target with a standard UUID filename is skipped
  before its metadata is read, while an unreadable target (or a file without a
  filename UUID) fails loud (`partial`, never `empty`);
  damaged target still reports `partial`; filtered budget still truncates; hit
  ids identical with and without the filter; Codex opens only the target.
- Codex projection: injected prefixes, whole-text XML wrappers, developer/system
  roles, subagent rollouts, IDE context before the request marker; unknown types
  counted; a half-written final line is not an error.
- Identity: plain vs zstd identical `hit_id`/locator/range/row fingerprints;
  plain+zstd dedupe to one logical session; compressed-only readback.
- Readback: every search hit reads back; context before/after spans records;
  `max_bytes` truncation; wrong pointer, wrong record index, zero-byte range,
  changed bytes, out-of-range `text_range`, wrong schema, wrong root all fail
  closed; row-only drift is a warning; a symlinked carrier (final component or
  intermediate directory) is refused instead of being followed out of the
  root; the decompressed scan limit trips `REF_READ_FAILED`; bounded line
  reading never buffers more than limit+1 bytes.
- Search: an oversize record is charged for the bytes actually read, reported as
  `partial`/`RECORD_SKIPPED` with `reason=oversize`; the drained bytes consume
  the byte budget (a line larger than the budget therefore stops the scan).
- Boundaries: no default `.codex` discovery; Codex strict-snapshot leaves the
  tree byte-for-byte unchanged; `src/recall` contains no archive/capture/write
  reference; three-source interleaving and pagination unchanged.

## Known limitations

- No Codex observed-root window; append-only classification is out of scope.
  Structural read-only behavior is enforced by the no-write boundary test.
- `--strict-snapshot` is unsupported for Codex; a scoped snapshot
  (`storage` + `db` only) is deferred.
- Only `storage` is enumerated and only carriers inside it are readable.
  `archived_sessions` requires explicit configuration for both search and
  readback.
- Titles are best-effort: a locked/missing state DB silently falls back to the
  session index, then to the first user message.
- `FileChange`, `ImageView`, `WebSearch`, `reasoning`, `compacted` and other
  record types are counted in `ignored_types` but not indexed yet.
- A tool output may appear twice when both `function_call_output` and
  `CommandExecution.aggregated_output` exist; the records have distinct
  coordinates and `hit_id`s. De-duplication is deferred.
- Codex readback streams from the start of the carrier. Search and readback
  share the same 64 MiB decompressed ceiling: search never emits a record that
  ends past it (it reports `partial` with `reason=read_limit`), and a line is
  buffered to at most 64 MiB + 1.
- Symbolic links are refused by both enumeration and readback. Windows
  directory junctions are reparse points that `is_symlink()` does not report;
  junction-based escapes remain a general link-hardening follow-up (also true
  for the pre-existing markdown read path).
- Windows x86_64, Rust 1.99.0 verified only. macOS/Linux were not exercised for
  this change.
- The prebuilt Windows binary and `build-manifest.json` still describe the
  previous source tree; rebuild/lineage refresh belongs to the orchestrator.

## Verification commands

```sh
cd agent-memory
cargo check --locked --all-targets
cargo test --locked
```
