# Archive P5 完成记录 · 覆盖报告与派生重放

范围：计划 `20261024-agent会话原始证据归档.md` 的 **P5** 阶段——CLI 采集/状态/
覆盖报告/回读/派生重放入口，覆盖状态分类与框架基准对账，派生代际检查点重放，
周期 scrub 与损坏记账，权限映射与重建期间的 fail-closed 语义。**P6 未做**
（无套装离线安装、预编译分发物、审计接入或真实数据试采）。

真实数据：**否**。全部用例只使用 `tempfile` 临时根、合成 JSONL、合成 SQLite
数据库与合成附件；未指向、未读取任何真实会话目录、真实数据库或真实附件。

Git：**验收时已提交**（本次 orchestrator 提交；executor 不自行执行 `git commit`）。
实现基线为 `de2021d`，P5 收尾修复随本轮验收提交。

## 1. 实施文件

| 文件 | 动作 | 职责 |
|---|---|---|
| `agent-memory/src/report.rs` | 新增 | P5 覆盖模型与对账：`ReportConfig` / `DeclaredSource` / `BaselineSpec`；`CategoryTotals`、`ProviderReport`、`ObservationWindow`、`BaselineReport`、`ReconciliationReport`、`DerivedReport`；声明源匹配、格式代际校验、目录枚举 / Codex `session_index.jsonl` / OpenCode 会话表三类基准对账；`coverage_with` 的评估入口 |
| `agent-memory/src/replay.rs` | 新增 | 派生重放：parser/schema 代际身份 `ReplayIdentity`、按源检查点状态、物化源字节并调用统一 parser、OpenCode 基线快照 + 行级差分重建观测点数据库、按 `ses_` 会话逐条解析、`derived:` 引用回读、`rebuilding` fail-closed 标记、清单摘要前后比对 |
| `agent-memory/src/scrub.rs` | 新增 | 周期 scrub：单写者锁一致视图、对象流式哈希复核、损坏/缺失记账 `state/scrub.json`、孤儿对象悬挂、staging 在写对象显式跳过、`--skip-if-locked` 显式跳过报告、覆盖投影 `ScrubCoverage` |
| `agent-memory/src/coverage.rs` | 修改 | 每源新增 `configured` / `category` / `categories` / `observation` / `corruption`；报告新增 `categories` / `providers` / `reconciliation` / `scrub` / `derived` / `derivedIdentity`；拆分 `coverage_base` / `coverage` / `coverage_with`；SQLite 健康失败计入 `read_failed`；解析失败、附件缺失、派生滞后分类 |
| `agent-memory/src/archive/mod.rs` | 修改 | 通用命名锁 `acquire_named_lock`；保留 `capture.lock`，新增 `derived.lock`（重放单写者，独立于采集） |
| `agent-memory/src/bin/agent-memory.rs` | 修改 | 新增 `coverage --config`、`replay`、`derived-read`、`scrub [--skip-if-locked]`；稳定退出码 `8/9/10`；`status` 增加 derived/scrub 投影；引用回传契约文档化 |
| `agent-memory/src/lib.rs` | 修改 | 注册 `report` / `replay` / `scrub` 模块；crate 范围更新为 P0–P5 |
| `agent-memory/tests/p5_report_replay_scrub.rs` | 新增 | P5 验收用例 16 项（全部合成临时根） |
| `agent-memory/src/replay.rs`（`#[cfg(test)]`） | 新增 | 2 项单元用例：源键稳定性、hex 标记解码严格性 |
| `agent-memory/README.md` | 修改 | P5 覆盖模型、对账口径、重放检查点/代际、scrub 一致视图、CLI 与退出码、布局 |
| `CURRENT.md` | 修改 | 任务状态 |

未触碰：`src/capture.rs`、`src/capture_jsonl.rs`、`src/capture_sqlite.rs`、
`src/attachment.rs`、`src/providers/**`、`src/model.rs`、`src/recall/**`、
`src/mcp.rs`、`tests/p0_formats.rs`、`tests/p1_archive.rs`、
`tests/p2_jsonl_incremental.rs`、`tests/p3_sqlite_consistency.rs`、
`tests/p4_attachments.rs`、`tests/recall_*`、`tests/support/**`、安装脚本与
分发物、Obsidian 库。

## 2. 覆盖报告（`src/report.rs`, `src/coverage.rs`）

### 状态分类

每个物理来源都带 `categories[]`（可多项）与 `category`（主分类）：

| category | 含义 |
|---|---|
| `covered` | 原始证据已提交；与附件完整性、派生新鲜度是不同维度，可并存 |
| `disabled` | 配置声明但 `enabled=false`；显式可见，不视为故障 |
| `unsupported` | 声明的 provider 或格式代际不在 P0 支持矩阵，或重放遇到未知 provider/kind |
| `read_failed` | 采集状态 `failed` / `degraded`，或 SQLite 最近观测不可用 |
| `parse_failed` | 采集解析探测失败，或派生重放解析失败 |
| `attachment_missing` | 已提交附件映射中存在 `missing` / `source_missing` |
| `derived_lag` | 当前 parser/model 检查点落后于已提交范围，或最近一次提交晚于派生时间 |
| `not_captured` | 已声明、已启用但尚无证据 |
| `corruption` | scrub 记账中该来源引用的对象损坏或缺失 |

主分类优先级：`unsupported > read_failed > parse_failed > attachment_missing >
corruption > disabled > derived_lag > not_captured > covered`。`covered` 不覆盖
同期存在的附件/派生问题，`CategoryTotals` 按维度分别计数。

`coverage.ok` 只反映当前视图：失败/降级状态、最近一轮异常、解析失败、附件
拒绝、基准不一致/不可用、scrub 损坏、派生索引损坏或声明代际不支持会置
`false`；历史累计计数、`not_captured` 与 `derived_lag` 本身不置 `false`。

### 数量与观测窗口

每源保留 P2/P3/P4 的 `discoveredLatest` / `capturedTotal` / `skippedTotal`
（语义见 `src/coverage.rs` 顶部），并新增：

```json
"observation": {
  "kind": "records" | "sqlite_observation",
  "watermarkAtMs": 0,
  "previousAtMs": 0,
  "intervalMs": 0,
  "records": 0,
  "offset": 0,
  "line": 0,
  "observationSeq": 0,
  "blindWindow": "……"
}
```

文件来源的窗口是**提交水位**：两次提交之间的写入、改写后回退不可见，文案显式
说明；SQLite 窗口来自 `lastObservedAtMs` / `previousObservedAtMs` /
`observationIntervalMs`，明确不是连续捕获。

### 框架基准对账

`config.local.json`（或 `--config`）按声明源给出 `baseline`：

| kind | 基准单位 | 归档单位 |
|---|---|---|
| `codex_session_index` | `session_index.jsonl` 中不同 `id` | 已覆盖 `rollout-*` 文件名中的 UUID |
| `opencode_session_table` | 只读打开实时库的 `session_v2` / `session` 的 id | 最新归档观测中的 session 行 id |
| `directory_enumeration` | 根下匹配 `pattern` 的文件 | 根下已覆盖的物理源路径 |

结果 `status` 为 `ok` / `mismatch` / `unavailable` / `disabled`。`mismatch`
逐项给出 `missingFromArchive` / `extraInArchive`；`unavailable` 保留读取错误
（含“声明源无 access 映射，按 fail-closed 不读归档”）。两者都使
`reconciliation.ok=false` 与 `report.ok=false`，CLI `coverage` 退出 `8`。

### 派生与 scrub 投影

`derived` 给出当前 parser/model/generation、`sourcesOk` / `sourcesLagging` /
`sourcesFailed`；派生索引损坏时报告异常但**不隐藏原始采集覆盖**；generation 处于
`rebuilding` 时置 `ok=false`（CLI 退出 8），因为派生回读正在 fail-closed。`scrub`
给出最近一次扫描的检查对象数、损坏/缺失数、孤儿数、staging 跳过数与异常；损坏
账本本身也作为异常报告，不当作“无损坏”。

## 3. 派生重放（`src/replay.rs`）

### 数据布局

```text
<root>/derived/state.json                                 检查点索引
<root>/derived/generations/g<N>/sources/<key>.json        每源派生物
<root>/derived/tmp/replay-*/                              物化临时源（运行后删除）
<root>/locks/derived.lock                                 重放单写者
```

`key` = 源路径 SHA-256 的前 32 位十六进制。manifest 采集事实只被读取，重放前后
用完整 manifest JSON 的 SHA-256 比对（`manifestDigestBefore/After`）。

### 代际与检查点

- 身份 = `(parserVersion, modelVersion, archiveSchemaVersion)`；每个身份拥有一个
  generation。新 parser 版本开新 generation，绝不改写旧 generation 的派生物。
- 每源检查点记录：capture generation / observation seq、记录数、末记录 SHA-256、
  输入对象摘要、artifact 相对路径 + SHA-256、冻结 access 标签。
- **复用条件**全部成立才跳过读取与解析：provider/kind/输入种类一致、记录数或
  观测序号一致、末记录/输入摘要一致、artifact 存在且哈希一致、generation 不是
  `rebuilding`。否则从原始对象重放该源，并报告
  `checkpoint did not match the archive`。
- **诚实边界**：provider parser 是整源解析，*有变化*的源整源重放；检查点单位是
  源，不是单条记录。“续建”指未变化源完全跳过，不是逐记录增量解析。
- 开始工作前先把 generation 标为 `rebuilding` 持久化；完成后写
  `complete` / `partial`。崩溃留下 `rebuilding`：派生回读一律拒绝，下一次重放
  不信任旧条目。
- `empty`（无完整记录且无对象字节）、`parse_failed`、`unsupported` 均写入检查点
  并显式报告，不静默当成功。

### 从 Archive 的物化方式

- JSONL / 压缩 rollout：按来源最大 capture generation 取对象（压缩源取
  `decoded` 对象），按 `sourceOffset` 顺序拼接为临时 `.jsonl`，调用当前
  `load_messages`。旧 generation 对象保留但不参与当前重放。
- 整文件：取该源最新 manifest 的 `whole_file` 对象，按原扩展名物化后解析。
- SQLite：`capture_sqlite::replay_sqlite` 取最新观测点行状态；用基线快照的
  `sqlite_master` DDL 建一个可读数据库，按行状态写入（`blob_hex` /
  `invalid_utf8_hex` 还原为 BLOB），再按 `session_v2` / `session` 枚举会话并调用
  OpenCode SQLite provider。非 `ses_` 形状的会话 id 显式记为异常，不猜。

### 回读与权限

`derived:<generation>:<sourceKey>` 回读 artifact 字节：校验 SHA-256；generation
为 `rebuilding` 时拒绝；条目 status 必须为 `ok`；`access` 为空或无请求域时
`AccessDenied`。失败不回落、不按残留映射放行。

## 4. 周期 scrub（`src/scrub.rs`）

- 扫描范围：全部已提交 manifest 的 `objects[]`、`records[].object` 与
  `attachments[].objectSha256`。
- **一致视图**：获取与采集相同的 `locks/capture.lock`；持锁写入方存在时默认
  拒绝（CLI 退出 `4`），`--skip-if-locked` 返回
  `status=skipped_writer_active` 的显式报告，绝不伪装成 clean。
- 逐对象流式重算 SHA-256（64 KiB 块），与内容地址比对；缺失对象与损坏对象写入
  `state/scrub.json`（含引用 captureId、来源路径、首次/最近发现时间、错误）。
- 覆盖投影把损坏记账变成 `report.ok=false`、`categories.corruption` 与来源级
  `corruption` 标记；不存在“扫描发现损坏但报告正常”的路径。
- 孤儿对象沿用 P1 口径悬挂，不删除；staging 文件显式列在 `stagingSkipped`，
  不算损坏；不自动修复、不删除证据、不承诺抗同域蓄意篡改（完整性 ≠ 真实性）。
- `state/scrub.json` 损坏时，`scrub` 直接 `Corrupt` 错误；`coverage` 报
  `scrub.status=ledger_corrupt` + 异常 + `ok=false`，不当作无损坏。

## 5. CLI 契约

```text
agent-memory coverage --root <root> [--config <report-config.json>]
agent-memory replay --root <root> --domain <domain> [--parser-version <v>] [--model-version <n>]
agent-memory derived-read --root <root> --derived-ref derived:<generation>:<sourceKey> --domain <domain> [--out <file>]
agent-memory scrub --root <root> [--skip-if-locked]
```

稳定退出码：`0` ok · `2` usage/config · `3` io/state（含损坏 manifest）·
`4` 锁被占用 · `5` 拒绝访问 · `6` 未找到/完整性 · `7` 采集失败 ·
`8` 覆盖异常（报告仍在 stdout）· `9` 重放部分失败（报告仍在 stdout）·
`10` scrub 发现损坏/缺失（报告仍在 stdout）。

引用回传契约：对象地址（64 hex）、记录 SHA-256（64 hex）、附件
`<captureId>#<seq>`、派生 `derived:<generation>:<sourceKey>`；回读前均校验
SHA-256 并重新执行核心库鉴权。`coverage` / `replay` / `scrub` 非 ok 时仍输出
完整 JSON，调用方按退出码分支，不解析人类文本。

## 6. 测试

```sh
cd agent-memory
cargo test
```

实测（2026-10-26，worktree `C:\wt\archive-p5-20261026`）：**exit 0，
364 passed / 0 failed / 1 ignored**（P5 原始 363 项 + 收尾新增 1 项 CLI 退出码 9
反向测试；见 `P5-FIX-COMPLETED.md`）。

| 目标 | 数量 |
|---|---|
| lib（单元；含新增 2 项 P5 单元用例） | 165 |
| `boundary_no_default_roots` | 5 |
| `p0_formats` | 4 |
| `p1_archive` | 8（未改） |
| `p2_jsonl_incremental` | 17（未改） |
| `p3_sqlite_consistency` | 33（未改） |
| `p4_attachments` | 24（未改） |
| **`p5_report_replay_scrub`** | **16（新增）** |
| `recall_boundary` / `recall_cli_mcp` / `recall_contract` | 6 / 7 / 9 |
| `recall_kb_device` / `recall_opencode` / `recall_read` | 25 / 22(+1 ignored) / 18 |
| doctests（5 个 compile-fail 反向用例） | 5 |

P5 用例（全部合成临时根）：

1. Codex `session_index.jsonl` 多一个 thread、目录枚举多一个 rollout → 两类
   baseline 均 `mismatch`，逐项 `missingFromArchive`，`ok=false`，CLI 退出 8
   `codex_index_and_directory_baselines_report_mismatch_explicitly`
2. OpenCode 实时会话表新增 `ses_two` → 与最新归档观测对账 `mismatch`，不静默成功
   `opencode_table_baseline_is_compared_with_the_archived_observation`
3. 未知格式代际 → `unsupported`；未启用源 → `disabled` 且不计故障；已声明未采集
   → `not_captured` `unsupported_and_disabled_declarations_are_explicit`
4. 采集失败 → `read_failed`；缺失附件 → `attachment_missing` 且附件计数/异常
   `read_failed_and_missing_attachment_are_categorized`
5. 整文件 OpenCode 源可采集但不可解析 → 派生 `parse_failed`、覆盖 `parse_failed`、
   `ok=false` `derived_parse_failure_is_a_parse_category_not_a_hidden_skip`
6. 新/旧 parser：v1 首建 generation 1；同身份全部复用；追加只重放变化源；
   v2 开 generation 2 且旧 artifact 保留；篡改 v2 artifact 后从原始证据重建；
   删除整个 `derived/` 后重建字节一致；manifest 摘要前后一致
   `replay_generations_checkpoints_and_rebuild_are_consistent`
7. 派生回读：access 为空拒绝；generation=`rebuilding` 拒绝，且 coverage `ok=false`、
   `derived.status=rebuilding`、CLI coverage 退出 8；未知引用 NotFound
   `derived_read_is_fail_closed_on_missing_access_and_rebuilding`
8. 删除实时 OpenCode 数据库后，从基线快照 + 行级差分重建观测点并解析出会话
   `opencode_derived_replay_comes_from_the_archived_observation`
9. 压缩 Codex rollout 从 `decoded` 对象重放，二次重放全部复用
   `compressed_codex_rollout_replays_from_the_decoded_archive_object`
10. 手工翻转对象字节 → scrub `corrupted` 并记账；coverage `ok=false` +
    `categories.corruption`；CLI `scrub` 退出 10、`coverage` 退出 8；staging 在写
    文件只计入 `stagingSkipped`
    `scrub_detects_tampering_and_coverage_reports_the_damage`
11. 持有 `capture.lock` 时默认拒绝（CLI 4）；`--skip-if-locked` 返回显式
    `skipped_writer_active`（CLI 0，非 clean）
    `scrub_refuses_a_concurrent_writer_unless_explicitly_skipping`
12. 损坏 manifest → `coverage` / `coverage_with` / `replay` 均报错，CLI 退出 3，
    不产出空报告 `corrupt_manifest_is_an_error_not_an_empty_report`
13. 派生 artifact 被删除 → 覆盖报索引不一致/`derived_lag`、`ok=false`，再次重放
    恢复；损坏派生索引 → 覆盖仍产出原始证据并报 `derived.status=corrupt` /
    `ok=false`，`replay` fail-closed；损坏 scrub 账本 → `ledger_corrupt` /
    `ok=false`
    `corrupt_derived_index_and_scrub_ledger_are_reported_without_hiding_coverage`
14. CLI JSON：coverage 分类/观测窗口/provider；record 引用回读逐字节一致；
    replay + `derived-read` 回读；未知派生引用退出 6；缺 `--domain` 退出 2
    `cli_json_exit_codes_and_reference_readback_are_stable`
15. coverage / replay / scrub 全程源文件字节与 mtime 不变
    `report_replay_and_scrub_leave_the_source_untouched`
16. CLI replay 部分失败（整文件 OpenCode 解析失败）→ stdout 报告 `ok=false`、
    `failedSources=1`、源 `parse_failed`，退出码 9；同一状态 coverage 退出 8
    `cli_replay_partial_failure_exits_nine`

外加 lib 单元：

- `source_keys_are_stable_and_bounded`
- `hex_marker_decoding_is_strict`

## 7. 已知限制

1. **P6 未做**：无离线安装脚本、预编译分发物、构建谱系、审计反向用例、套装
   文档接入；无任何真实会话/附件采集。真实数据 = **否**。
2. **检查点是源级**：provider parser 是整源解析，变化的源整源重放；未实现逐记录
   增量解析。未变化源完全不读不解析。
3. **SQLite 重放是逻辑观测点数据库**：schema 来自基线快照，行内容来自基线 +
   行级差分；不是实时库文件的逐字节副本，也不声称捕获观测间中间态。
4. **OpenCode 会话 id 契约**：移植 loader 只接受 `ses_` 前缀；其它 id 显式列为
   异常并跳过该会话，不猜测替代解析。
5. **派生回读是 artifact 级**：`derived-read` 返回整个 per-source artifact，不提供
   单条消息切片；access 是重放时该源当前证据的冻结交集。
6. **Codex 基准依赖文件名 UUID**：无 UUID 的 rollout 不计入归档会话 id，会以
   `mismatch` 显式呈现，而不是忽略。
7. **OpenCode 基准读取实时库**：只读打开；WAL 不可读或锁超时 → `unavailable`，
   不回退到归档数字掩盖实时差异。
8. **文件来源观测窗口是提交水位**：`skipped_unchanged` 运行不产生新 manifest，
   因此窗口不随每次扫描推进；盲区文案已显式说明，不把它表述为连续观测。
9. **scrub 只保证完整性**：不抗同域蓄意同时改写对象与 manifest；不修复、不删除、
   不回收孤儿；无内置调度器（由 operator/外部计划调用 CLI）。
10. **删除的源仍留在检查点索引**：generation 中保留其旧条目以便历史可查，但当前
    覆盖只按现存 manifest 行统计；不自动清理旧 generation。
11. **派生 artifact 未加密/未签名**：与归档同域信任边界；权限映射缺失时 fail-closed，
    但不构成抗本机同域攻击的边界。
12. **重放物化为整源字节**：为复用整源 parser，JSONL/整文件源会在内存中拼出完整源
    字节再解析；没有流式 parser API。超大源的内存占用未做上限控制。

## 8. 结论

P5 的 5 项目标均有实现与用例证据：CLI 采集/状态/覆盖/回读/重放 JSON 契约与稳定
退出码；七类覆盖状态 + 每源发现/已采/跳过数 + 观测水位/盲区；三类框架基准对账
及不一致异常态；带 parser/schema 代际检查点的派生重放与旧派生重建；单写者一致
视图下的 scrub 与损坏记账；权限/重建 fail-closed 与显式不支持状态。P1–P4 既有
断言未改动且保持通过。**P6 未做，真实数据 = 否。**
