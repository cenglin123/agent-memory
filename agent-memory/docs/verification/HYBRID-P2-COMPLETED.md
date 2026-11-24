# 统一混合检索 P2：使用事件与统计完成记录

状态：**P2 完成**（本文件是本阶段唯一 sentinel）。
真实数据：**否**。全部用例使用临时目录合成 fixture（合成 KB stub、合成 Codex rollout、合成 evidence 文件与临时事件库）；未读取任何真实会话、真实知识库或生产 Archive，未写入任何真实运行时根，未实现 P3 embedding 或 P4 图关联。

范围：`src/recall/telemetry.rs` 的 append-only 事件存储、幂等、外部锚点、聚合计数、`events stats|export`、有界保留与 `events prune`；`telemetry.retention_days` 配置键；事件库与 Archive/派生/replay 的隔离。

## 1. 实现

### 1.1 存储（`src/recall/telemetry.rs`）

- 位置只来自 v2 配置 `telemetry.path`（惯例 `<运行时根>/state/retrieval_events.sqlite`），**不推导默认路径**。`telemetry.enabled=true` 时该键仍是必填（P0 规则不变）。
- 只有 `events record` 创建数据库和父目录（`SQLITE_OPEN_READ_WRITE | SQLITE_OPEN_CREATE`）。`stats`/`export` 用 `SQLITE_OPEN_READ_ONLY`，`prune` 用 `READ_WRITE`（不带 `CREATE`），因此读命令无法创建文件，缺失库按空结果处理。MCP、`memory_search`、`memory_read`、`replay`、`report`、`coverage`、`scrub`、`capture` 完全不引用本模块。
- schema（`EVENTS_STORAGE_SCHEMA_VERSION = 1`，写入 `events_meta.schema_version`）：
  - `retrieval_events(seq, idempotency_key UNIQUE, kind, hit_id, ts, source_id, session_id, task_id, evidence, actor, query, expires_at)`；
  - `retrieval_event_counts(hit_id, kind, count, first_ts, last_ts, PRIMARY KEY(hit_id, kind))`；
  - `events_meta(key, value)`；
  - 触发器 `retrieval_events_append_only`：除"把非空 `query` 置为 NULL 且其余列不变"外，任何 `UPDATE` 都被 `RAISE(ABORT)` 拒绝，明细只追加。
- 版本不匹配 → `schema_unsupported`；库中存在但无事件 schema → `schema_missing`；两者都是 `EVENTS_STORAGE_ERROR` / exit 11。`busy_timeout` 2 s，超时/损坏/不可写统一走该错误码。
- 未启用 WAL：事件库保持单文件语义，避免 `-wal`/`-shm` 旁文件造成的"字节不变"歧义。

### 1.2 幂等与计数

- `idempotency_key` 由调用方给出；缺省时对**规范化事件内容**（排序后的键集合：kind/hit_id/ts/source_id/session_id/task_id/evidence/actor/query/expires_at）求 SHA-256。
- 写入用 `INSERT ... ON CONFLICT(idempotency_key) DO NOTHING`；`changes()==0` 即重复：`ok=true, wrote=false`、`code=EVENTS_DUPLICATE`、`duplicate=true`、exit 0，计数不动。
- 聚合表与明细在**同一事务**内更新（`count = count + 1`，`first_ts = MIN(...)`，`last_ts = MAX(...)`），统计与明细不会分叉。

### 1.3 外部锚点（§3.4，机械判定）

- `retrieved`/`read`：只作曝光/诊断，无锚点。
- `applied`：schema 层先要求 `task_id` + 至少一条 evidence；`confirmed`：要求 `actor` + 至少一条 evidence。
- 写盘前逐条核验 evidence：必须是**存在的普通文件**，且文件字节中**包含该事件的 `hit_id` 字符串**。任一不满足 → `EVIDENCE_UNVERIFIED`（exit 2，`wrote=false`，不落盘），`error.reason` 分别为 `evidence_missing` / `evidence_not_a_file` / `evidence_unreadable` / `evidence_without_hit_id`，`error.path = evidence[i]`。
- 反例覆盖：无 evidence、evidence 不存在、evidence 不含 hit_id、缺 actor/task_id。

### 1.4 统计、导出、保留

- `events stats [--hit-id <id>]`：输出每个 hit 的 `retrieved/read/applied/confirmed`、`events`、`last_event_at`，以及 `totals`（hits/events/四类合计）。数据源是与明细同事务维护的聚合表。
- `events export [--include-query]`：按 `seq` 输出 JSON Lines（信封写 stderr）。默认**完全不含 `query` 键**；`--include-query` 时才输出 `query`，且只有 `expires_at > now` 的行给出正文，过期行为 `null`。
- `telemetry.retention_days`（u32，默认 365，范围 1–3650，越界 `out_of_range`、类型错 `invalid_type`、未知键仍 `unknown_key`；v1 仍不接受 telemetry）。
- `events prune` 单事务两步：① 把 `expires_at < now` 的非空查询正文置空（事件行、聚合计数不变，触发器的唯一允许更新）；② `DELETE` `ts < now - retention_days` 的明细。返回 `pruned_query_text` / `pruned_events` / `remaining_events` / `preserved_count_rows`；库不存在时返回 0 且**不创建**。
- 保留期删除只删明细，聚合表不动，因此 `stats` 计数不倒退，`export` 只剩保留期内的明细。

### 1.5 隔离

- 配置层机械拒绝 `telemetry.path` 落在任一 archive 源的 `<root>/archive` 或 `<root>/derived` 下（`reserved_area`），无论 telemetry 是否启用；`state/` 目录仍允许，与计划 §3.3 的位置一致。
- 事件库不进入 `archive/objects`、`derived/` 或 replay 输入；反向测试证明 replay 前后事件库字节不变、派生树无 events 文件、把事件库改成垃圾后 replay 仍 exit 0 且不修复它。

### 1.6 CLI（`src/bin/agent-memory.rs`）

- `events <record|prune|stats|export> --config <file> [--event <json|@file>] [--hit-id <id>] [--include-query]`；`export` 的 JSON Lines 独占 stdout，信封走 stderr（与 `read`/`derived-read` 同一惯例）。
- 退出码：`0` 成功（含重复提交）· `2` 事件非法或锚点不可核验 · `11` 遥测关闭/缺失或存储失败。

### 1.7 决定与解读（规格未逐字规定处）

- `EVIDENCE_UNVERIFIED` 使用 exit `2`（请求级拒绝，调用方可修正），规格只要求"非零 + 明确错误码"。
- `--include-query` 对已过期行输出 `"query": null`（而不是省略键），使导出字段集在该模式下稳定；默认模式则**不出现** `query` 键。
- 保留期以事件 `ts` 与进程当前 UTC 时间比较（不注入时钟）；测试用固定的过去时间戳（`2020-01-01T00:00:00Z`）与当前时间构造两侧样本。
- `stats`/`export` 在库缺失时返回 `ok=true` 的空结果，不报错；只有库存在但不可读/不是事件库才报 `EVENTS_STORAGE_ERROR`。这样读命令不会因为"还没记录过"而失败，同时仍然不创建文件。

## 2. 测试

`cargo test --locked`：**489 passed / 1 ignored / 0 failed**（P1 基线 472 passed / 1 ignored；旧测试无回归，净增 17）。

### 2.1 新增 `tests/recall_telemetry.rs`（17 项，全部临时目录合成）

| 用例 | 覆盖 |
|---|---|
| `mcp_search_and_read_never_create_or_modify_event_storage` | telemetry 开启时 MCP 两个工具不创建库与父目录；`events record` 建库后 MCP 前后字节完全一致；`readOnlyHint` 仍为 true、`side_effects` 仍为空 |
| `memory_search_and_read_never_produce_applied_or_confirmed` | CLI search+read 后事件库字节不变，`applied`/`confirmed` 恒 0，`read` 也不自动产生 |
| `retrieved_and_read_events_are_recorded_and_counted` | retrieved/read 正向写入、位置回显、stats 四类计数、export 顺序 |
| `applied_requires_an_external_anchor_in_the_evidence_file` | 正向（临时文件写入 hit_id）；反例：无 evidence（INVALID_REQUEST）、evidence 不存在（evidence_missing）、evidence 不含 hit_id（evidence_without_hit_id） |
| `confirmed_requires_actor_and_verifiable_evidence` | 正向；反例：evidence 不含 hit_id、缺 actor |
| `duplicate_submissions_are_idempotent_and_counts_do_not_move` | 内容哈希去重与显式 `idempotency_key` 去重；第二次 `wrote=false/duplicate=true`、exit 0、计数与导出行数不变 |
| `stats_and_export_agree_per_hit` | 两个 hit、6 条事件；stats 总计与逐 hit 计数 == 从 export 行统计的计数；`--hit-id` 过滤；`last_event_at` 为最大 ts |
| `export_never_contains_query_text_by_default` | 默认导出无 `query` 键，stdout 不含查询正文 |
| `include_query_exports_only_unexpired_bodies` | 未过期行有正文，已过期行为 null，默认模式两者都没有 |
| `prune_blanks_expired_query_text_and_keeps_counts` | `pruned_query_text=1`、`pruned_events=0`；stats 前后一致；`--include-query` 已为空 |
| `retention_deletes_old_details_but_keeps_aggregate_counts` | `retention_days=1`；旧明细删除 1 条，聚合计数保留（仍 2 个 hit），export 只剩新明细，二次 prune 幂等 |
| `unwritable_and_corrupt_event_storage_fail_non_zero` | 父路径是普通文件 → exit 11 `EVENTS_STORAGE_ERROR`；垃圾数据库 → record/stats/export/prune 全部 exit 11 且 `ok=false` |
| `telemetry_disabled_never_creates_event_storage` | record/prune/stats/export 均 `TELEMETRY_DISABLED` exit 11，文件与父目录均未创建 |
| `replay_ignores_the_event_store_and_leaves_it_untouched` | 合成 archive + capture 后记录 2 条事件；replay 前后事件库字节不变、输出不含 events、派生树无 events 文件；把事件库改成垃圾后 replay 仍 exit 0 且未修复 |
| `v2_telemetry_accepts_retention_days_and_rejects_out_of_range_or_unknown_keys` | 默认 365；1/3650 接受；0/3651 `out_of_range`；字符串/布尔/小数 `invalid_type`；未知键 `unknown_key`；v1 仍拒绝 telemetry |
| `event_storage_may_not_live_inside_archive_objects_or_derived` | `archive/objects/...`、`derived/generations/...` → `reserved_area`；`state/...` 接受 |
| `replay_report_coverage_and_scrub_never_reference_the_event_store` | 反向 tripwire：`replay.rs`/`report.rs`/`coverage.rs`/`scrub.rs`/`capture.rs`/`mcp.rs`/`recall/{search,read,ops}.rs` 均不出现 `retrieval_events` 或 `recall::telemetry` |

### 2.2 新增/更新的单元测试（`src/recall/telemetry.rs`，共 10 项）

新增：`content_hash_is_stable_and_key_order_independent`、`record_event_never_writes_when_disabled_or_invalid`、`append_only_trigger_rejects_row_updates_except_query_blanking`、`prune_reports_zero_without_creating_storage`、`stats_and_export_do_not_create_storage`。

### 2.3 修改的既有断言（逐条原因）

1. `tests/recall_telemetry.rs::mcp_search_and_read_are_read_only_and_create_no_event_storage` → 更名为 `mcp_search_and_read_never_create_or_modify_event_storage`，并补"建库后 MCP 前后字节一致"。原因：P2 下事件库**应**由 `events record` 创建，原断言只覆盖"永不创建"，需同时覆盖"不修改"。
2. `tests/recall_telemetry.rs::events_record_is_non_zero_and_writes_nothing_in_p0` → 删除。原因：P2 实现存储后，合法 retrieved/read 必须 `wrote=true` 且 exit 0；其"telemetry 关闭 → 11"与"非法 applied → 2"的意图保留在 `telemetry_disabled_never_creates_event_storage` 与 `applied_requires_an_external_anchor_in_the_evidence_file`。
3. `tests/recall_telemetry.rs::events_prune_is_non_zero_and_deletes_nothing_in_p0` → 删除。原因：prune 现已实现；"不创建库"的意图转入 `prune_reports_zero_without_creating_storage`（单元）与 `telemetry_disabled_never_creates_event_storage`（集成）。
4. 单元 `p0_record_never_writes_and_fails_non_zero` → 改为 `record_event_never_writes_when_disabled_or_invalid`：去掉"已启用也必然失败"的 P0 断言，保留 disabled/非法请求/unverifiable anchor 不落盘。
5. 单元 `p0_prune_never_deletes_and_fails_non_zero` → 改为 `prune_reports_zero_without_creating_storage`：断言改为"库不存在时返回 0 且不创建"。
6. `enabled()` 测试辅助函数新增 `retention_days` 字段（`TelemetryConfig` 新增字段，无行为断言变化）。

## 3. 命令与退出码

| 命令（工作区 `C:\wt\hybrid-retrieval-20261028`） | 结果 |
|---|---|
| `cargo test --locked`（`agent-memory`） | **489 passed / 1 ignored / 0 failed** |
| `cargo test --locked --test recall_telemetry` | 17 passed / 0 failed |
| `powershell ... assets\scripts\check-docs.ps1` | exit 0（6 项、56 个 Markdown） |
| `git diff --check` | exit 0 |

未运行 `cargo fmt`：钉定工具链 `1.99.0-x86_64-pc-windows-msvc` 未安装 `rustfmt` 组件（`cargo fmt` 报错退出 1，不是格式差异）。

未执行 `62` / `90`，也未重建 `assets/dist/agent-memory/`（由 orchestrator 负责）：源码树哈希已变化，本执行器不产出分发物，因此 62/90 的构建谱系项必然不通过。

## 4. 限制与已知边界

- **无真实数据**：无真实查询分布、无真实并发、无生产规模事件量；只承诺合成临时库下的行为。
- **保留期使用墙钟**：prune 与 export 的"现在"取进程 UTC，没有可注入时钟；测试用固定历史时间戳构造过期样本。跨时区不影响（全部按 UTC 字面比较）。
- **锚点是"包含 hit_id 的子串"判定**：规格要求"文件内容包含 hit_id"，实现严格按字节子串核对；不校验产物类型、不解析 plan/commit 结构，也不判断 hit_id 是否属于语义相关段落。
- **evidence 路径解析**：接受绝对路径或相对进程工作目录的路径，只要求 `is_file()`；不限制必须位于某个根内（配置层只对 `telemetry.path` 做保留区约束）。
- **`stats` 只读聚合表**：明细被保留期删除后，`last_event_at` 仍保留在 `retrieval_event_counts`，但无法再导出对应明细（有意为之：删除即失去历史）。
- **锁超时未单测**：`busy_timeout` 2 s 的并发超时路径没有专门用例（需要第二个连接长时间持锁），仅覆盖不可写与损坏库；超时与二者共用 `EVENTS_STORAGE_ERROR`。
- **事件库未被加密/签名**：append-only 由 SQLite 触发器约束（可被有意 `DROP TRIGGER` 绕过），不是防篡改存储；这是本地诊断记录，不是审计证据层。
- **`query_text_ttl_days` 仍只是配置值**：wrapper 用它计算 `expires_at`；本阶段不生成 wrapper，因此 TTL 不参与 prune（prune 只看事件自带的 `expires_at`）。

## 5. 未做

- 未实现 P3（embedding 执行、融合、held-out 评估）与 P4（图关联、上下文组装）。
- 未实现宿主侧 wrapper，也未让 MCP 产生任何事件（`readOnlyHint` 与只读语义保持原样）。
- 未改 `assets/dist/agent-memory/`、`build-manifest.json` 或 61/62/90 谱系；未执行 git commit（提交由 orchestrator 执行）。
- 未读取任何真实会话、知识库或生产 Archive 数据，未在任何真实运行时根下创建事件库。

## 6. 收口（orchestrator）

- 实现提交：`ca562fe`；分发物 `ce85c5f`、谱系 `b744d49`（此后因收口修复再次重建，见 CHANGELOG）。
- fresh verifier：`zhipuai-coding-plan/glm-5.3-flash`（与执行者不同 lineage；首次进程中断后续接原会话完成），target `ca562fe`，verdict **通过**，无阻断，6 条非阻断。
- orchestrator 当场修复 N1 / N4（硬约束优先，改动小于派发成本）：
  - N1：`RetrievalEvent::query_ttl_error` 机械约束 `expires_at ≤ ts + query_text_ttl_days`，伪日期 fail-closed；`record_event` 在锚点检查前拒绝（exit 2，不落盘）。
  - N4：`prune_events` 改用 `ensure_schema_read`，不再在非事件库文件中建表。
  - 回归测试：`query_text_cannot_outlive_the_configured_ttl`、`prune_never_initialises_schema_in_a_foreign_database`；`cargo test --locked` 491 passed / 1 ignored / 0 failed。
- 非阻断遗留（后续认领）：N2 无 BEFORE DELETE 触发器；N3 保留区词法匹配可被 `..`/符号链接绕过；N5 缺省幂等键含调用方 `ts`；N6 `ts` 不校验时钟偏差。
- 本机安装：OpenCode Desktop 运行中占用已安装二进制，61/90 的「已安装且与分发物一致」项延后到全部阶段完成后统一重装复验。
