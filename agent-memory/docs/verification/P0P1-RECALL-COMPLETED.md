# 最小统一回忆 P0/P1 — 完成记录（sentinel）

状态：**P0/P1 范围内完成**；复审后的 B1–B4 修正见 `agent-memory/docs/verification/P0P1-FIX-COMPLETED.md`。P2（KB/device 接入与聚合）与 P3（CLI/stdio MCP）未开始。
真实数据：**否**。全部用例只使用运行时写入 `<temp>` 的合成 fixture；未读取任何真实笔记、真实 OpenCode 库、真实凭据或用户级配置。

## 1. 验证命令与结果

```sh
cd agent-memory
cargo test        # exit 0 — 221 passed, 0 failed, 1 ignored
cargo tree --depth 1   # exit 0 — 仅 9 个直接依赖，无 tauri
```

- `cargo test` 组成：144 lib 单测 + 72 集成测试 + 5 compile-fail doctest。
  集成分布：`boundary_no_default_roots` 5、`p0_formats` 4、`p1_archive` 8、
  `recall_boundary` 6、`recall_contract` 9、`recall_opencode` 22（另 1 ignored）、`recall_read` 18。
- 唯一 ignored：`scale_fixture_full_scan_within_latency_target`
  （1000 会话 / 100000 part，`data` 合计 35,679,000 B ≈ 34 MiB，> 32 MiB 对照预算）。已实测并记录：
  - release：0.892 s / 0.960 s / 0.903 s（3 次，取最差 0.960 s）
  - debug：5.12 s / 5.32 s / 5.41 s（目标 ≤ 10 s、目标值 5 s）
  - 同一用例内 32 MiB 对照：`scanned_sessions < total_sessions` + 源级 `truncated` + `BUDGET_BYTES`
  - 触发方式：`cargo test --release --test recall_opencode scale_fixture_full_scan_within_latency_target -- --ignored`
- 性能修复记录 1：初版按 message 逐条查询 part（无索引时退化为全表扫描），
  100000 part 需 ~19 s；改为单条 session 级 JOIN 查询后降至 ~1.2 s（release）。
- 性能修复记录 2：规模 fixture 的单 part 正文由 80 字符提升到 300 字符（35.7 MB），
  使 32 MiB 对照预算可真实触发；全量扫描仍 < 1 s（release）。

## 2. 实现文件（全部在 `agent-memory/**` 内）

| 文件 | 职责 |
|---|---|
| `src/recall/mod.rs` | 模块入口与公开导出 |
| `src/recall/config.rs` | 唯一显式 JSON 配置：`version=1`、`sources[]` 四键、`source_order`、`interpreter.kb`、必填/未知键、盘符路径、UNC 拒绝、`db`/`storage` 位于 `root` 内（大小写不敏感）、零启用源 |
| `src/recall/contract.rs` | 冻结常量：24 个错误码、8 个状态与优先级、6 个不完整状态、5 个降级状态、warning 分集合、`detail` 键集、`ref` 八键、`memory_read` 响应键集、退出码推导 |
| `src/recall/hash.rs` | `sha256`、`root_sha256` 规范化（整条路径小写、`\`→`/`、折叠分隔符、保留 `c:/`）、`TextRange`（0-based 半开）、流式文件哈希 |
| `src/recall/refs.rs` | `Locator`（按代际字段，无 `table`/`rowid`）、`Fingerprint`、`HitRef`、`hit_id`、V2 pointer 校验 |
| `src/recall/text.rs` | 词面规则（ASCII 折叠、ASCII run、CJK 整段 + bigram）、`LexMatch`（含首词项长度）、以首命中为中心的 snippet、UTF-8 边界截断、行扩展 |
| `src/recall/budget.rs` | 预算默认值与 `kb_hard_timeout_ms = max(1000, min(18000, max_elapsed - 2000))` |
| `src/recall/search.rs` | `memory_search` 请求逐键校验、来源分发、`source_rank` 交错、`pagination.sources_incomplete`、`next_offset` 三条件、`errors[]`/退出码 |
| `src/recall/read.rs` | `memory_read` 请求/`ref` 严格校验（含 `rel_path` = 解析载体）、文本源按区间 seek 读取、上下文行扩展、OpenCode 单段/多段回读、`max_bytes` 收窄与指纹重算、`REF_*` fail-closed |
| `src/recall/snapshot.rs` | 观测根整树 `path+size+mtime` 前后 diff；C 级 `window_only`；已知 A/B 路径与既有源载体变化判为违约；新建/未完成文件只记窗口变化 |
| `src/recall/port/model.rs` | 归一模型、`SessionMeta.parent_id`、健康计数 `records/skipped/errors/oversize/bytes`、`PortError{NotFound|Locked|Corrupt|Schema|Io}`（只含变体与 message，不含码/ detail） |
| `src/recall/port/project.rs` | `project_part_text`：`text` 原文；`tool` = 名称 + 完整 `state.input`（缺省 `{}`）+ 完整 `state.output`（缺省空串，非字符串 → 结构损坏）；递归排序 `compact_json`；`injected` 三谓词；类型分类与 `ignored_types` 字面量 |
| `src/recall/port/blocks.rs` | 64 KiB 对齐块读取、UTF-8 边界、前缀指纹（`PrefixFingerprint`/`PrefixDigest`）、指纹相同但字节不符时回落逐字节比较、单对象上界读取（legacy JSON 读取路径使用） |
| `src/recall/port/probe.rs` | V1/V2 必需表列矩阵、能力探测、`session_v2=absent|incomplete` 回退、只读打开 |
| `src/recall/port/sqlite_v1.rs` | V1 会话/消息/part 读取与单 part 定位（session 级 JOIN，`(time_created,id)` 全序，超预算 part 在 SQL 侧按长度跳过并计数） |
| `src/recall/port/sqlite_v2.rs` | V2 `session_v2`/`session_message` 读取、`content[i]` 与 `/text` 两种 part 形态、`(seq,id)` 全序、可选 `parent_id`、按 part 计 oversize |
| `src/recall/port/legacy_json.rs` | 旧 JSON 三条固定模板、`parentID` 映射、RFC3339/毫秒时间、路径组件校验（防穿越）、相对路径与 `row_sha256`、块读取 + 读取前上界检查 |
| `src/recall/port/reader.rs` | 代际解析顺序（`db` 存在 → SQLite；否则 `storage` → 旧 JSON；否则不可用）与统一 Reader |
| `src/recall/providers/mod.rs` | `SourceHit`/`SourceOutcome`、`port_code`/`port_detail`（唯一的 `DB_*`/`SCHEMA_UNSUPPORTED` 映射与 detail 塑形）、源内排序 |
| `src/recall/providers/opencode.rs` | 预算推进、`coverage`、`ignored_types`、`CONTENT_UNRESOLVED`、单对象超限、`truncated/partial/empty/ok`、命中缓存（前缀指纹 + 字节校验） |
| `tests/support/mod.rs` | 临时根、每用例显式配置、环境白名单（重定向 15 项 / 透传 `PATH`/`SystemRoot`/`ComSpec`，其余清空并还原）、V1/V2/旧 JSON/规模 fixture 生成器、fixture 体量查询 |
| `tests/recall_contract.rs` | 冻结表、退出码、配置与请求逐键反例、信封键集 |
| `tests/recall_opencode.rs` | 正文命中、工具投影全文、注入/元数据、子会话、CRLF、V2 两分支与额外形态、代际回退、锁定/损坏/缺库、预算覆盖、分页、未配置源、规模 + 32 MiB 对照 |
| `tests/recall_read.rs` | 精确回读、`rel_path` 校验、上下文、多段/单段、`max_bytes`、`REF_*`、`WARN_FILE_STALE`、副作用分类与并发窗口（C 级非空 / A/B 违约） |
| `tests/recall_boundary.rs` | 无 home/XDG/env 读取、`port` 不引用 `providers`、依赖白名单、用户级配置反证 |
| `tests/fixtures/recall/kb_stub/ask.py` | 离线 KB 替身（ok/degraded/error/timeout/契约版本/mode 缺失分支），供 P2 复制到临时 kb.root，不执行 |
| `agent-memory/src/lib.rs` | 仅新增 `pub mod recall;` |

## 3. 支持矩阵

| 能力 | 状态 | 证据 |
|---|---|---|
| 显式 JSON 配置（版本/必填/未知键/顺序/绝对路径/解释器） | 完成 | `recall_contract` 9 例 |
| `hit_id`/`ref`/`root_sha256`/`text_range`/`range_sha256` | 完成 | 单测 + `recall_opencode`/`recall_read` |
| OpenCode 旧 JSON 正文检索 | 完成 | `legacy_json_refs_preserve_escapes_and_crlf` 等 |
| OpenCode SQLite V1 正文检索 | 完成 | `v1_*`、`damaged_*`、`budget_*` |
| OpenCode SQLite V2（`content[i]` + `/text`） | 完成 | `v2_*` |
| schema 能力探测 / 缺表列 / V1 回退 | 完成 | `v1_fallback_reports_session_v2_state`、`unavailable_unsupported_and_corrupt_are_distinct` |
| 健康/跳过计数（`records/skipped/errors/bytes`） | 完成（库内 API，不新增 `detail` 键） | `damaged_rows_are_partial_and_visible` |
| `parent_id` / `ocsr-` 子会话排除 | 完成 | `sub_sessions_and_injected_parts_are_not_addressable`、V2 无 `parent_id` 列用例 |
| `project_part_text`（tool 全量、无 preview） | 完成 | `tool_projection_keeps_full_output_and_input_middle` |
| `injected` 三谓词 + 反例 | 完成 | 单测 + `v1_body_and_tool_projection_hits` |
| `ignored_types` / `CONTENT_UNRESOLVED` / `partial` | 完成 | `v1_body_*`、`damaged_*` |
| 预算推进 / `coverage` / `truncated` / `BUDGET_BYTES` / `BUDGET_TIME` / 单对象 oversize | 完成 | `budget_progress_*`、`time_budget_*`、`oversize_*` |
| 64 KiB 分块 + 前缀指纹 | 完成（legacy JSON 读取路径 + 前缀指纹命中缓存） | `port/blocks` 单测、`read_json_object` |
| 稳定排序（distinct → 首偏移 → 时间 → id） | 完成 | 源内排序 + 分页用例 |
| `memory_read` 单段/多段/上下文/`max_bytes` | 完成 | `recall_read` 18 例 |
| `ref.rel_path` 与解析载体一致（SQLite = 配置 db，legacy = locator 推导 part 文件） | 完成 | `opencode_ref_rel_path_must_match_the_resolved_carrier` |
| `REF_SOURCE_MISMATCH`/`REF_ROOT_MISMATCH`/`REF_STALE`/`REF_LOCATOR_MISSING`/`REF_OUT_OF_RANGE`/`REF_READ_FAILED` | 完成 | `ref_validation_is_fail_closed` 等 |
| `WARN_FILE_STALE`（文件/行级漂移只告警） | 完成 | `range_changes_are_stale_and_file_changes_only_warn`、`opencode_row_change_*` |
| `side_effects[]`（C 级整树 diff、`window_only`、不误判新文件） | 完成（分类器 + 并发窗口端到端：非空 C 级 / A/B 违约） | `snapshot_window_*`、`memory_read_reports_a_non_empty_window`、`memory_read_fails_closed_*` |
| 分层边界（port 不产生 `DB_*`/detail） | 完成 | `providers::{port_code,port_detail}` + `recall_boundary` 静态扫描 |
| 无默认根 / 无配置发现 / 无 Tauri / 依赖白名单 | 完成 | `recall_boundary` 6 例 + `cargo tree` |
| KB（`ask.py --json`）与 device 适配器 | **未实现（P2）** | 请求时显式 `unsupported` + `reason=adapter_pending_p2` |
| CLI / stdio MCP | **未实现（P3）** | — |

## 4. 范围与偏差（如实记录）

1. **未做真实数据验证**：未读取真实库/会话/笔记，未生成
   `tests/fixtures/recall/kb_python_baseline.json`（该文件按计划需在真实库上
   单独授权的一次性只读导出，属 P2/后续授权项）。`tests/fixtures/recall/kb_stub/ask.py`
   已作为离线替身落地（不执行、不接触真实索引），供 P2 KB 适配器用例复制使用。
2. **路线：复用现有移植模型，未重复 vendor**：计划 §3.4 的移植清单以 cc-switch 的
   `SessionMessage`/`SessionBlock` 为目标，而该模型与 provider 移植已存在于本 crate
   的 Archive 切片（`src/model.rs`、`src/providers/*`，基线 v4.0.20 /
   `85ad4969b3399500baaea9d46b1e08f87aba80e1`）。recall 复用该模型，只补充计划需要的
   原语（代际探测、part 投影、块读取、行映射）；`NOTICE` 已登记该路线与文件清单，
   没有第二份 vendor、没有 cc-switch 依赖。
3. **编排层路径**：计划 manifest 写的是 `agent-memory/src/providers/**`，但该
   目录已是 Archive 切片（Claude/Codex/OpenCode 证据读取）。recall 的编排放在
   `src/recall/providers/`，保持 `providers → port` 单向且不污染既有模块。
4. **规模用例默认 ignored**：1000/100000 fixture（35.7 MB）构建约 15 s、扫描约
   0.9 s（release），为保持默认 `cargo test` 时长而显式隔离；两档实测与 32 MiB
   对照已记录在 §1。
5. **`source_status.detail` 只使用冻结键**：schema 失败的缺失表/列写入
   `detail.reason`（逗号分隔），不新增 `detail.missing`；健康计数
   `records/skipped/errors/bytes` 保留在 port/provider API，不进入信封。
   `DB_*`/`SCHEMA_UNSUPPORTED` 映射只在 `providers/`（`port_code`/`port_detail`）。
6. **A/B 违约端到端用例**：`recall_read` 以并发写窗口验证了非空 C 级报告与
   已知 A/B 路径的 `SIDE_EFFECT_VIOLATION` 失败；归属不可判的新文件只标
   `window_only`（`snapshot` 分类器），不把任何新文件都判成泄漏。

## 5. 已知限制

- P2/P3 全部未实现：KB embedding/BM25/graph 管线、device 日期事件、CLI、
  stdio MCP、`--strict-snapshot`、KB 子进程超时与 ask 信封映射。
- Archive P2–P6 未推进；本记录不改变 Archive 的采集语义与水位。
- 未实现任何持久索引、学习/晋升、跨源 score 归一化或跨源去重。
- 仅在 Windows 上验证（路径契约本身声明 Windows-only）。
- `side_effects[]` 只证明"窗口内有变化"，不归属写入者。
