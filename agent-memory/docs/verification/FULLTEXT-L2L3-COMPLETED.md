# 全文检索优先 L2/L3：活源全文索引与精确检索 — 完成记录

- **状态**：完成（代码、测试、文档与 Python onboarding 均在本工作区落地）
- **真实数据 = 否**：全部用例使用临时目录中的合成 fixture；未读取任何真实会话、真实知识库内容或生产 Archive，未重建 `assets/dist`，未运行 61/62/90，未执行 `git commit`。
- **工作区**：`C:\wt\hybrid-retrieval-20261028`
- **执行日期**：2026-10-29

## 1. 实现

### 1.1 新增/修改的源文件

| 文件 | 内容 |
|---|---|
| `agent-memory/src/recall/fulltext/mod.rs` | 索引根与路径推导、`meta`/`doc_keys`/`sessions`/`units` schema、`Unit` 模型（含 `doc_key`/`doc_digest`）、`session_kind` 命名常量与三源派生、查询词解析（空白切词 + 双引号短语）、FTS5 短语转义、`Order`、水位结构、索引态（Ready/Missing/Stale/Corrupt）与降级原因码 |
| `agent-memory/src/recall/fulltext/build.rs` | `index_all` / `index_source` 与三源构建器：OpenCode（`session_message` 水位 + 行数 + schema 探测）、Codex/Claude Code（文件大小+mtime+偏移+行号）、设备记忆（文件大小+mtime）；单事务、内容 digest 去重、记录级 prune |
| `agent-memory/src/recall/fulltext/query.rs` | exact 检索：MATCH 表达式、短词候选校验、过滤/排序/`group_by_session`、候选窗口、ref 还原（存储 locator + 坐标兜底）、`bm25` 分值、降级回退载荷 |
| `agent-memory/src/recall/providers/claude_code.rs` | 新只读活源：转录枚举（含 `subagents/`）、记录投影（text/tool_use/tool_result）、注入与侧链策略、词面检索、`claude-jsonl-v1` ref 生成 |
| `agent-memory/src/recall/read.rs` | `claude_code` 回读（重新投影 + 区间/哈希校验 + 上下文）、`claude-jsonl-v1` locator 解析、strict-snapshot 拒绝、`verify_projection_range` |
| `agent-memory/src/recall/search.rs` | `mode` 与 7 个 exact-only 参数、非 exact 出现即拒绝、`degraded` 回退标注、exact 跨源时间排序、命中元数据字段 |
| `agent-memory/src/recall/contract.rs` | `degraded` 状态、`exact` detail 键、`claude-jsonl-v1` 代数、`claude_code` 源类型、exact 请求键/枚举/降级码/`bm25` 与 `fulltext_index` 常量 |
| `agent-memory/src/recall/config.rs` | `source.kind = claude_code`、v2 顶层 `cache_root`（显式、drive-absolute、禁止落在 Archive `archive/`/`derived/`）、`SourceKind::is_indexable` |
| `agent-memory/src/recall/providers/mod.rs` + 四个既有 provider | `SourceHit.exact: Option<ExactMeta>`（非 exact 恒 `None`，响应键集不变） |
| `agent-memory/src/recall/refs.rs` | `claude-jsonl-v1` locator schema 登记 |
| `agent-memory/src/mcp.rs` | `memory_search` schema 增加 8 个参数，描述写明「事实/标识符类检索请用 mode=exact」；两工具仍 `readOnlyHint` |
| `agent-memory/src/bin/agent-memory.rs` | `index --config <file> [--source <id>] [--rebuild]` 子命令与 usage |
| `agent-memory/tests/recall_fulltext.rs`（新增） | 20 个用例 + 1 个 `#[ignore]` 性能用例 |
| `agent-memory/tests/recall_claude_code.rs`（新增） | 5 个用例 |
| `agent-memory/tests/{recall_contract,recall_cli_mcp,recall_config_v2}.rs` | 4 处既有断言按新增合同更新（见 §6） |
| `agent-memory/README.md` | 新增「Live full-text index and exact mode (L2/L3)」整节；配置/源清单/CLI 同步 |
| `assets/references/agent-memory-archive.md` | 新增 §4.4；64 生成项与根边界说明 |
| `assets/scripts/recall_onboarding.py` | `--claude-code-root` / `--skip-claude-code`、v2 `cache_root` 生成与校验、`claude_code` 源与检查 |
| `assets/scripts/64-install-recall.ps1` | `-ClaudeCodeRoot` / `-SkipClaudeCode` 透传 |
| `assets/scripts/test_recall_onboarding.py` | 冻结形状更新 + 2 个新用例（cache_root、claude_code 根） |
| `CHANGELOG.md` | 顶部新增本条目 |

### 1.2 关键行为

- `index` 只写 `<cache_root>/live/<source_id>/fts.sqlite`；`memory_search`/`memory_read`（CLI 与 MCP）只以只读方式打开已存在的索引。
- 每条索引行：`text` / `tool_input` / `tool_output` 三通道之一 + `source_id`、`session_id`、`message_id`、`record_index`、`pointer`、序列化 `locator`、`rel_path`、`ts`、`role`、`kind`、`session_kind`、`title`、`range_len`、`range_lines`、`range_sha256`、`row_sha256`。`text_range = 0..range_len`、`line_start = 0`、`line_end = range_lines`，因此 `memory_read` 的区间与哈希校验对四类源都成立。
- 工具输出索引上限 20 000 **字符**（按字符截断，不劈开多字节字符），截断以 `truncated` 记录；ref 仍指向完整投影，读取时按投影校验。
- 重复构建幂等：`doc_keys.digest` 相同即跳过删除+插入；`>=` 水位重读边界行不会产生 `added/removed`。

## 2. 设计决定（规格未逐字规定处）

1. **`degraded` 是新状态而不是复用 `partial`**：规格要求 `source_status` 标 `degraded`。为不改变 `errors[]` 语义，`degraded` 进 `STATUS_PRIORITY`（位于 `truncated` 之后）与 `DEGRADED_STATUSES`（退出码 4/10），但**不进** `INCOMPLETE_STATUSES`（`errors[]` 仍只含六种 incomplete 状态），也不阻止 `next_offset`。非 exact 路径永不产生该状态。
2. **降级原因两级化**：`detail.reason` 写 `INDEX_MISSING|INDEX_STALE|INDEX_CORRUPT`（规格要求的码），`detail.exact.cause` 写精确原因（`source_identity_mismatch` / `source_kind_mismatch` / `source_root_mismatch` / `index_schema_version` / `index_meta_missing` / `index_unreadable` / `index_locked` / `index_query_failed: <sqlite>`）。短词回退只写 `detail.reason = short_terms_fallback` + `detail.exact.mode = scan_fallback`，**不**降级（扫描仍能回答）。
3. **`source_kind = "claude_code"`、`schema_gen = "claude-jsonl-v1"`**，locator 形状复用 codex 的 `{session_id, message_id?, record_index, pointer}`（同一行坐标语义），以便回读与 `hit_id` 计算共用一套规则。
4. **claude_code 是加性源**：`--claude-code-root` 缺省时 64 不添加该源（不报错、不推导）；显式给出但目录不存在才失败。既有配置中的该源在省略参数时原样保留。
5. **词面路径保持既有排除策略**：OpenCode 子会话、Codex/Claude 子代理转录与 `isSidechain` 记录不进入词面结果；索引仍收录它们，用 `session_kinds` 显式取舍。
6. **Codex 会话身份来自首行 `session_meta`**：`payload.id` 作为 session id、`payload.source` 作为 `session_kind` 依据；逐行读不到 source（只有首行有）时用文件首行补齐。标题使用 session id（不读 `state_5.sqlite`）。
7. **索引只覆盖 OpenCode SQLite V2**：V1/legacy JSON 源在 `index` 报 `unsupported`（`the live index covers SQLite V2 …`），词面路径不受影响。Codex 单独 `.zst`（无明文孪生）不索引（无追加偏移语义），明文+压缩孪生按既有 `dedupe_siblings` 取明文。
8. **跨源排序**：`newest`/`oldest` 在合并页上按时间全局排序（时间可跨源比较）；`relevance` 保持按源轮转合并，因为 BM25 分数不可跨语料比较（写入 README）。
9. **候选窗口**：单源最多读取 4000 个候选（硬上限 20000），`detail.exact.candidate_window` 说明窗口是否成为限制；`group_by_session` 与短词校验都在窗口内完成。
10. **`session_hits` 的计数口径**：等于该会话在候选窗口内通过的命中数（长词 MATCH + 短词子串校验之后）。
11. **设备记忆单元**：一个文件 = 一个单元，索引文本为 `文件名\n正文`（保持词面路径「文件名+正文同一文档」的语义），ref 的 `text_range` 指向正文 `0..len`，`session_kind = unknown`。
12. **timestamp 序列化**：命中 `timestamp` 为 RFC3339 UTC 字符串（与 `since`/`until` 同格式）；源无时间（`ts <= 0`）时省略该键。
13. **索引代际**：`schema_version = 1`；与配置 `source_root`（逐字）、`source_id`、`source_kind`、`root_sha256` 任一不符即 `INDEX_STALE`。不做「内容已更新」探测：增量由 `index` 完成，查询只读快照。
14. **rustfmt 不可用**：钉定工具链 `1.99.0` 未安装 rustfmt 组件（`cargo fmt` 报错），因此未做自动格式化；手写风格与周边一致（4 空格缩进、行宽 ≈100）。

## 3. 测试用例表

### 3.1 `agent-memory/tests/recall_fulltext.rs`（20 + 1 ignored）

| # | 用例 | 覆盖点 |
|---|---|---|
| 1 | `index_reports_every_enabled_source_and_builds_rows` | 四源 `built`、路径 `<cache>/live/<id>/fts.sqlite`、重复构建 `unchanged` |
| 2 | `exact_mode_matches_identifiers_and_reports_hit_metadata` | 标识符命中、`timestamp`/`role`/`kind`/`session_kind`/`snippet`/ref、`detail.exact.mode=index` |
| 3 | `exact_terms_are_anded_and_phrases_stay_contiguous` | 多词 AND、短语连续性、单词命中对照 |
| 4 | `tool_input_and_tool_output_are_separate_channels_and_output_is_truncated` | 三通道拆分、20 000 字符截断（头部可命中、尾部不可） |
| 5 | `opencode_incremental_add_update_and_delete` | 新增→`updated`、原地改写→行数不变且旧词消失、删行→整库重建 |
| 6 | `opencode_schema_change_triggers_a_full_rebuild` | `time_updated` 列出现 → `ts_column` 变化 → 重建 |
| 7 | `jsonl_append_change_shrink_and_delete` | 追加、同大小改写（mtime）、缩短、删除文件 |
| 8 | `device_file_change_and_delete_update_the_index` | 文件改写与删除、`session_kind=unknown`、`md-v1` ref |
| 9 | `deleting_the_index_and_rebuilding_produces_identical_hits` | 删除四源索引后重建，`hit_id` 与 `results` 逐字节一致 |
| 10 | `short_terms_fall_back_and_long_terms_are_verified_on_candidates` | 全短词回退（`short_terms_fallback`）、长短混合的候选子串校验 |
| 11 | `exact_filters_cover_time_role_kind_session_kind_and_session_id` | `session_kinds`/`roles`/`kinds`/`since`/`until`/`session_id`、非 exact 拒绝 `exact_only` |
| 12 | `orders_are_newest_oldest_and_bm25_relevance` | 三种排序、`score_kind=bm25`、exact 下 `limit`/`offset` 分页与 `next_offset` |
| 13 | `group_by_session_keeps_one_hit_per_session_with_counts` | 每会话一条 + `session_hits` |
| 14 | `session_kind_is_derived_for_all_three_sources` | opencode/codex/claude 三源 worker/interactive 派生 |
| 15 | `missing_stale_and_corrupt_indexes_degrade_explicitly` | `INDEX_MISSING`/`INDEX_STALE`（含 `cause`）/`INDEX_CORRUPT`、退出码 10 |
| 16 | `exact_hits_read_back_through_memory_read` | 四源 exact 命中均可 `memory_read` 回读原文 |
| 17 | `auto_mode_is_unchanged_by_the_index_and_rejects_exact_keys` | 同一请求在「建索引前 / 建索引后 / 显式 `mode=auto`」三次序列化逐字节一致；响应无 `exact`/`timestamp`/`kind`/`session_kind` 键；`roles`/`order`/`group_by_session` 在 auto 下拒绝；非法枚举与 RFC3339 拒绝 |
| 18 | `cli_index_builds_and_mcp_never_writes_the_cache` | CLI `index` 退出 0 与报告；MCP `tools/call` 前后 cache 文件树（路径/大小/mtime）完全一致 |
| 19 | `index_without_cache_root_is_an_explicit_error` | CLI 退出 2 + `detail.path=cache_root`；库层同样失败 |
| 20 | `index_source_selection_and_kb_are_reported_verbatim` | `--source` 选择、KB `skipped_not_indexable`、未知源 `unknown_source` |
| P | `large_fixture_exact_query_latency`（`#[ignore]`） | 20 万行构建 + 20 次查询 p95 |

### 3.2 `agent-memory/tests/recall_claude_code.rs`（5）

| # | 用例 | 覆盖点 |
|---|---|---|
| 1 | `lexical_search_projects_text_tool_input_and_tool_result` | 文本/工具入参/工具结果三类 unit、注入（isMeta）、system、侧链、子代理转录与半行均不返回、`ignored_types` |
| 2 | `session_id_filter_matches_the_filename_and_the_record` | 会话过滤与实际记录一致、无命中为 `empty` |
| 3 | `refs_read_back_with_exact_range_and_context` | 单段回读、上下文多段、伪造区间 `REF_STALE`、越界 `REF_OUT_OF_RANGE`、越根路径与错指针 `REF_LOCATOR_MISSING` |
| 4 | `missing_root_is_unavailable_and_never_empty` | 根不可读 → `unavailable` + `SOURCE_ROOT_INVALID` |
| 5 | `strict_snapshot_is_refused_for_claude_code` | `strict_snapshot_unsupported_for_claude_code_root` |

### 3.3 单元测试（`src/recall/fulltext/mod.rs`，4）

`terms_split_on_whitespace_and_keep_phrases`、`match_expression_quotes_and_ands_only_long_terms`、`session_kind_rules_are_named_and_cover_all_sources`、`source_id_directory_is_safe_and_collision_free`。

### 3.4 Python（`assets/scripts/test_recall_onboarding.py`，26）

新增/更新：`test_generated_v2_shape_is_frozen`（含 `cache_root`）、`test_cache_root_is_validated_and_preserved`、`test_claude_code_root_is_explicit_and_additive`。

## 4. 性能数据（合成 fixture，release 构建）

```
cargo test --release --locked --test recall_fulltext large_fixture_exact_query_latency -- --ignored --nocapture
PERF rows=200000 build_ms=4962 index_bytes=297238528 p95_ms=8 max_ms=11
```

- fixture：单文件 20 万条 Claude Code 记录（每条 ≈130 字符正文 + 时间戳/uuid），查询 20 个不同 `token#####` 词。
- 构建 4 962 ms（冷，单源）、索引 297 238 528 B（283.5 MiB）、单次 exact 查询 p95 **8 ms**、max 11 ms。
- 验收线 p95 ≤ 1 s：通过（余量 ≈125×）。
- 体积说明：合成行很短（≈130 字符），每行元数据（locator/标题/会话等）占比被放大；原型在 410 MB 真实 OpenCode 文本上测得 ≈3.3× 文本体积。
- 未在本机真实历史上建索引（任务边界：不读取真实会话）。

## 5. 命令与退出码

| 命令 | 结果 |
|---|---|
| `cargo test --locked`（工作区 `agent-memory/`） | **575 passed / 2 ignored / 0 failed**（基线 546 passed / 1 ignored；新增 29 passed + 1 ignored） |
| `cargo test --release --locked --test recall_fulltext large_fixture_exact_query_latency -- --ignored --nocapture` | 1 passed（数据见 §4），exit 0 |
| `python -m pytest test_recall_onboarding.py -q`（`assets/scripts/`） | **26 passed**，exit 0 |
| `cargo build --locked` | exit 0（无 warning） |
| `agent-memory index`（合成配置，无 `cache_root`） | exit 2，`{"ok":false,"code":"INVALID_REQUEST","detail":{"path":"cache_root"}}` |
| `agent-memory index`（合成四源配置） | exit 0，`sources[].status = built/updated/unchanged` |
| exact 查询含 degraded 源 | search exit 10（有结果 + 降级源） |

## 6. 既有断言的修改与原因（逐条）

1. `tests/recall_contract.rs::frozen_tables_have_no_duplicates_and_consistent_sets`
   - `STATUS_PRIORITY.len()` 8 → 9：新增 `degraded`（规格要求 exact 降级必须显式成状态）。
   - `DEGRADED_STATUSES.len()` 5 → 6，并断言 `degraded ∈ DEGRADED_STATUSES`、`degraded ∉ INCOMPLETE_STATUSES`：降级必须影响退出码但不得变成 `errors[]`。
   - `DETAIL_KEYS.len()` 11 → 12：新增 `exact` 块（索引态/回退原因/候选计数）。
2. `tests/recall_contract.rs::schema_generation_table_includes_codex_rollout`
   - `ALL_SCHEMA_GENS.len()` 6 → 7：新增 `claude-jsonl-v1`。
3. `tests/recall_cli_mcp.rs::mcp_search_schema_declares_the_session_id_filter`
   - `memory_search` schema 属性数 9 → 17：新增 `mode/since/until/roles/kinds/session_kinds/order/group_by_session`；同时新增对 `mode` 枚举与 `session_kinds` 枚举的断言。
4. `tests/recall_config_v2.rs::onboarding_generates_v2_and_the_binary_loads_and_searches_it`
   - 冻结的顶层键集合加入 `cache_root`：64 生成的 v2 现在显式写入索引缓存根（`<user-home>/.agent-memory/cache`），并断言其值。

未修改任何其它既有断言；无人为放宽。

## 7. 限制与未做

- 未在真实 OpenCode/Codex/Claude 历史上建索引或跑基准（任务边界）。
- 未重建 `assets/dist`、未运行 61/62/90、未 `git commit`。
- `cargo fmt` 不可用（工具链缺 rustfmt 组件），代码为手写格式。
- OpenCode 索引只支持 SQLite V2；V1 与 legacy JSON 源 `index` 报 `unsupported`（词面检索不受影响）。
- 单独 `.zst`（无明文孪生）的 Codex rollout 不索引（无追加偏移语义）。
- 索引不做「源已更新」的在线探测：查询读取的是最近一次 `index` 的快照。
- `group_by_session` 的 `session_hits` 计数限于候选窗口内；窗口上限 4000（硬上限 20000）。
- 短词（<3 字符）无法用 trigram 缩小候选，混合查询的候选上限与全短词回退行为已在上文说明。
- 索引体积对「短行」语料偏大（每行元数据固定开销）；真实长文本语料的体积比应更接近原型测得的 ≈3.3×。
- `relevance` 排序不跨源比较 BM25（多源时按源轮转合并）。
