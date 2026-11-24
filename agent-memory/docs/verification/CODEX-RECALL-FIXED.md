# Codex recall — verification fix record (F1–F5)

- Date: 2026-10-28
- Base: `c3172b5` (feat: add Codex unified recall and session filtering)
- Trigger: independent verification `FAIL` (F1 blocker; F2/F3 deviations;
  F4/F5 observations) — `<user-home>\AppData\Local\Temp\opencode\recall-codex-verification.md`
- Real session data used: **no**（真实数据=否）— every fixture is synthetic and
  written under a per-case temporary root. No Archive write, no capture, no
  `write_rollout`, no Git commit, no distribution rebuild, no
  `build-manifest.json` change.
- Result: `cargo test --locked` → **413 passed / 1 ignored** (406 / 1 before
  this fix; 7 new reverse tests; no regression).

## F1 — storage enumeration errors are fail-loud

`src/recall/providers/codex.rs::collect_rollout_files` now returns
`std::io::Result<()>`:

- Top-level `read_dir`/entry-iteration failure propagates; `search_at` returns
  `unavailable`, `DB_UNAVAILABLE`, `detail.reason = "codex_storage_unreadable"`,
  and the OS error in `errors[].message`. It is never a healthy `empty`.
- A failure below the top level (unreadable subdirectory) is counted with
  `health.error()` → `partial` / `RECORD_SKIPPED`; sibling directories are still
  scanned.
- `file_type()` failures are counted the same way; symlinks remain excluded.

Evidence (`tests/recall_codex.rs`, Windows ACL deny `icacls /deny *S-1-1-0:(RD)`):

- `unreadable_storage_root_is_unavailable_with_the_os_reason` — root denied:
  `unavailable` + `codex_storage_unreadable` + exit 4 + message contains
  `cannot be enumerated`; zero hits.
- `unreadable_subdirectory_is_partial_and_other_carriers_still_scan` — `2026`
  denied, `2027` readable: `partial` / `RECORD_SKIPPED`, one hit from `2027`,
  `coverage.total_sessions = 1`, `scanned_sessions = 1`.

The ACL guard restores the ACE in `Drop`; when `icacls` cannot apply/verify the
denial, the case skips instead of asserting a false negative.

## F2 — filename-UUID prefilter before metadata reads

- `infer_session_id_from_filename` is now `pub(crate)` and reused (no second UUID
  regex). For a request with `session_id`, a rollout whose filename carries a
  different UUID is skipped before `scan_session_file` opens the file.
- Files without a filename UUID still go through the existing
  `scan_session_file` metadata filter.
- A standard-name non-target that is unreadable therefore cannot degrade a
  filtered query; an unreadable target (or a no-UUID file that cannot be
  filtered by name) still fails loud.

Evidence (Windows exclusive lock with `share_mode(0)`):

- `locked_non_target_rollout_is_skipped_before_metadata_read` — target B is
  filtered while A is locked: `ok`, exit 0, two B hits, `total_sessions = 1`,
  `scanned_sessions = 1`.
- `locked_target_rollout_fails_loud_instead_of_empty` — target B locked:
  `partial` / `RECORD_SKIPPED`, zero hits, exit 4, `total_sessions = 0` (never
  `empty`).
- `non_target_without_filename_uuid_still_uses_metadata_filtering` — no-UUID
  file for session A is excluded by metadata while B is returned.

## F3 — one 64 MiB ceiling for search and readback

- `CODEX_SCAN_LIMIT_BYTES = 64 MiB` lives once in the Codex provider and is
  imported by `read.rs`.
- Search tracks the decompressed offset inside each carrier. A line whose end
  would pass the ceiling is not projected or emitted; the carrier stops, the
  source reports `partial` / `RECORD_SKIPPED` with `detail.reason =
  "read_limit"`, and other carriers keep scanning. `hit_id`, `ref` shape and
  default-budget behavior are unchanged.
- Readback keeps the same ceiling; a search hit can no longer be “emitted but
  unreadable” under a legal `max_bytes_per_source > 64 MiB`.
- Line buffering remains bounded to `limit + 1` (F5 read path unchanged).

Evidence:

- `records_beyond_the_shared_read_ceiling_are_not_emitted` — 70 × ~1 MiB
  records, then the target; `max_bytes_per_source = 128 MiB`; result is
  `partial` / `RECORD_SKIPPED` / `reason=read_limit`, zero hits, no budget
  detail.
- `compressed_readback_fails_at_the_decompressed_scan_limit` still passes for a
  direct (hand-written) ref past the ceiling.

## F4 — readback is confined to the configured `storage`

- `read_codex` now resolves the configured storage tree (`codex_storage_dir`,
  default `<root>/sessions`) and rejects a carrier outside it with
  `REF_LOCATOR_MISSING`, after the existing symlink-aware carrier resolution.
  Plain and `.zst` siblings are both covered.
- `archived_sessions` is therefore unreachable through a hand-written ref unless
  `storage` explicitly points there, matching the enumeration rule.

Evidence (`readback_is_confined_to_the_configured_storage_tree`):

- Byte-identical copy under `<root>/archived_sessions` → plain ref and
  compressed-only sibling ref both `REF_LOCATOR_MISSING`.
- A source with `storage = <root>/archived_sessions` searches and reads that tree
  successfully.

## F5 — drained oversize bytes are charged

- A `too_long` line now calls `health.consume(consumed)` where `consumed`
  includes the streamed/drained remainder. The byte budget is no longer
  under-reported; an oversize line larger than the budget stops the scan after
  being counted (`partial` / `RECORD_SKIPPED`, `reason=oversize`, budget detail).
- Ceiling-limited too-long lines are charged and stop that carrier with
  `read_limit`.

Evidence:

- `an_oversize_record_counts_the_bytes_it_read` — a ~20 KiB record with an
  8 KiB budget: `partial` / `RECORD_SKIPPED` / `reason=oversize`, no hits,
  `budget.reason = BUDGET_BYTES`, `consumed_bytes >= 8192`.
- `port/reader.rs` unit tests keep proving `buffer.len() == limit + 1` and
  drain/stop semantics.

## Corrected wording

`CODEX-RECALL-COMPLETED.md` now states that the “non-target damage does not
pollute a filtered query” property applies to **content-damaged** non-targets;
unreadable non-targets are either skipped by the filename-UUID prefilter or,
without a UUID, fail loud. It also records the shared 64 MiB ceiling, the charged
oversize bytes, storage-confined readback, and the new test counts.

## Residual limitations

- A non-target rollout **without** a filename UUID that cannot be read still
  degrades a filtered query (`partial`); it cannot be identified without opening
  at least its metadata. This is fail-loud, never a false empty.
- For standard filenames the UUID is authoritative for the prefilter: a rollout
  renamed so its filename UUID differs from `session_meta.id` is skipped by a
  filtered query (an unfiltered query still finds it).
- Records beyond the 64 MiB decompressed ceiling are not searchable; the source
  reports `partial`/`read_limit` instead of emitting unreadable refs.
- Windows directory junctions are reparse points that `is_symlink()` does not
  report; junction escapes remain a general link-hardening follow-up (also true
  for the pre-existing markdown read path).
- `FileChange` / `ImageView` / `WebSearch` / `reasoning` / `compacted` are still
  counted in `ignored_types` but not indexed; duplicate call/item outputs remain
  distinct records.
- Windows x86_64 / Rust 1.99.0 verified only. macOS/Linux were not exercised.
- The prebuilt binary and `build-manifest.json` still describe the previous
  source tree; rebuild/lineage refresh belongs to the orchestrator.
