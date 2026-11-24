# STAGE2-S1 COMPLETED — OpenCode SQLite 证据采集重构（规模可行 + 凭据不入库）

- 工作区：`C:\wt\stage2-s1`（HEAD `71c9a1d`，未 commit，由 orchestrator 提交）
- 计划：`docs/plans/active/20261030-第二阶段收尾-归档生产化与索引收敛.md` §1 / §4 S1
- 状态：**完成**（全套测试通过；规模预算达标）
- **真实数据 = 否**：全程未读取 `<user-home>/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory` 及任何真实会话/数据库/生产 Archive；全部测试使用合成 fixture 与 tempfile 根。8.8 GB 规模仅以合成库（837 MiB、250,001 行）近似。
- 格式版本：`sqlite-baseline@2`（块清单/DDL/水位入 manifest）、`sqlite-delta@2`（`deltaSchema` 入 manifest）、`sqlite-observation@2`；@1 状态显式报「unsupported observation schema … not trusted」并重做基线（生产为空，无迁移）。

## 实现（对照要求）

| # | 要求 | 落地 |
|---|---|---|
| 1 | 表白名单 | `SQLITE_TABLE_WHITELIST = [session, message, part, session_v2, session_message, project, todo]`（常量集中于 `capture_sqlite.rs`，README 列出）。白名单外**永不读行内容**，每次观测仅 `SELECT count(*)` 记 `{table, row_count}` 事实（`excludedTables`，capture/coverage 均输出）；schema fingerprint 只含白名单表。反例测试以 `SENTINEL-SECRET-*` 填充 credential/account/kv/event，采集后对整个运行时根逐文件字节扫描，哨兵零命中 |
| 2 | 基线不再整库拷贝 | 删除 backup API 快照；基线 = 一个只读事务内按白名单表、按主键/rowid 顺序分块导出（≤ 32 MiB/块，`BASELINE_CHUNK_MAX_BYTES`），内容寻址 + zstd（role `sqlite_baseline_chunk`），manifest 记块清单/每表行数/CREATE TABLE DDL/每表水位/`baselineDigest`。触发：首次、白名单 schema 变化、`--rebaseline`、状态不可用/不可信（degraded 语义不变）、链字节 > 基线 50%（`BASELINE_CHAIN_BYTE_PERCENT`）。按条数/512 KiB/15 分钟规则**已删除**（常量与文档一并移除） |
| 3 | 增量按水位 | 有 `time_updated` 按 `(time_updated, pk)` 水位；无则 `seq`/`time_created`/`created`；全无的表全表读取按行哈希比对（状态只存 pk→hash）。删除检测：每次观测只 `SELECT pk` 与状态键集比对，`op=delete` 的 `before` 从证据库（基线块+delta 链）读回，取不到则 `before=null` 并计 `missingBeforeRows`。状态只存每表水位+键集+小表行哈希+非白名单行数+过滤器+选中会话集，**不存行内容** |
| 4 | 盲区与校验 | `blindWindow` 声明水位盲区（内容变而 `time_updated` 不变、水位以下写入、整段回改）；`--verify` 或 7 天周期（`VERIFY_INTERVAL_MS`，记 `lastVerifyAtMs`）逐表 pk→hash 与证据重放比对，不一致行补录为 `verifyCorrection` delta 并计 `verifyMismatchesTotal`；不改源、不整库拷贝 |
| 5 | 会话时间过滤 | `--session-since <rfc3339>`（`session_since_ms`）：session/session_v2 按 `time_updated ≥ 阈值` 选会话，子表按选中会话集过滤（基线经 TEMP 表 JOIN，源只读），`project` 全量；过滤写入状态与报告；变化 → 重基线 `filter_changed` |
| 6 | 重放修复 | 物化改用 manifest 内 DDL：`PRAGMA foreign_keys=OFF` + 父表优先插入（`PARENT_FIRST_TABLE_ORDER`：project → session/session_v2 → message/session_message → part/todo，其余靠后），完成后 `PRAGMA foreign_key_check` 只报告不失败。测试覆盖子表行先于父表出现在 delta（(table,key) 排序自然形成）的乱序重放 |
| 7 | 格式版本 | 见上；`load_observation` 先探 schema 标签，@1 显式报错为不可信状态 → degraded 重基线，非 panic |
| 8 | 性能与内存 | 规模测试 `tests/p3_sqlite_scale.rs`（`#[ignore]`，已实测，见下） |
| 9 | 其他不变 | JSONL/legacy JSON/whole-file 路径零改动（p1/p2/p4 测试原样通过）；崩溃顺序（对象→回读验证→manifest→索引→观测状态→水位）与 fail-closed 鉴权保持；adopt 逻辑改为：基线从 manifest 事实重建状态，delta 重算观测与已提交行集比对 |

## 设计决定（与任务书的偏差或细化）

1. **水位 SQL 取 `WHERE col >= ?` + Rust 侧 `(value, pk)` 过滤**，非任务书示意 `col > ? OR (col = ? AND pk > ?)`：主键为任意文本时 SQL 端无法可靠比较键序；`>=` 读回后过滤语义等价，行序无关。未创建索引（源只读）。
2. **无水位列的表不分大小一律走全表行哈希路径**（任务书只定义 ≤10,000 行情形）；超过 `FULL_SCAN_HASH_MAX_ROWS` 的表在报告中列名（`oversizeFullScanTables`），代价可见。
3. **delta 单元的 `duplicatePayloadRows` 只统计本单元变更行**（基线单元统计全部行）；跨代共存的既存事实由最近一次基线保持。
4. **`--rebaseline` 跳过内容去重**，即使导出 digest 与上一代相同也提交新代（显式指令语义：要的是新基线，不是 "no change"）。
5. **delta 行序固定为 `(table, key, op)`**：子表行（session_message）自然排在父表行（session_v2）之前，作为 FK 乱序重放的固定用例；行状态重放本就不受顺序影响。
6. **unchanged 观测也推进观测状态**（水位/键集前进），否则每次都会重读同一批候选行；drift-degraded 运行不写状态（计划外 schema 不得进入 diff）。
7. **p3 fixture 升级为真实 OpenCode 列形**（message/part/session_message 带 `time_updated`，真实库即如此）：水位策略按真实列生效；24 行 part 使小 delta 不致立刻触发 50% 链阈值。
8. **崩溃窗口内 failpoint 位于 manifest+索引提交之后、观测状态写入之前**（与既有契约一致），保证 adopt 场景可测。

## 测试

命令：`cargo test --locked --no-fail-fast`（agent-memory/）
结果：**629 passed / 0 failed / 3 ignored**（2 ignored 为既有 recall 规模用例；+1 ignored 为本任务新增规模用例）。基线为 617 passed / 2 ignored；净增 12 个通过用例，`cargo build` 0 warning，`git diff --check` 干净。规模用例：`cargo test --release --test p3_sqlite_scale -- --ignored --nocapture` 通过。

新增/重写用例（p3_sqlite_consistency.rs 33→40，+replay 单测 +scale 用例）：

| 用例 | 覆盖 |
|---|---|
| credential_sentinels_never_reach_the_runtime_root | 凭据哨兵永不入库（运行时根字节扫描） |
| unknown_columns_are_verbatim_and_unknown_tables_are_never_read | 白名单外表只有行数；白名单表未知列原样保存 |
| initial_capture_is_a_chunked_whitelist_baseline | 首次基线分块（4 块/逐块哈希回读/DDL/水位入 manifest） |
| watermark_deltas_replay_to_each_observation_point | 水位增量新增/修改/删除 + 逐观测点重放 + before 取自证据 |
| tables_without_a_watermark_column_use_row_hashes | 小表行哈希路径（project 无时间戳列：增/改/删/无误报） |
| schema_change_triggers_a_new_baseline | schema 变化触发基线（unknown_table_or_column / schema_change，旧代可读） |
| explicit_rebaseline_commits_a_new_generation | `--rebaseline` |
| chain_bytes_over_half_the_baseline_trigger_a_baseline、delta_chain_byte_threshold_triggers_a_baseline | 链字节超基线 50% 触发基线 |
| session_since_filter_scopes_children_and_project_stays_full、filter_change_rebaselines | `--session-since` 过滤与过滤变化重基线（`filter_changed`） |
| verify_finds_watermark_blind_changes_and_corrects_them | `--verify` 发现并补录水位盲区改动（内容改/时间戳不动） |
| v2_only_database_is_captured_and_replays（+replay.rs 单测 sqlite_materialization_is_foreign_key_safe） | V2 外键、子表行先于父表的重放与物化、foreign_key_check 干净 |
| v1_observation_state_is_rejected_and_rebaselined | @1 状态显式报错重基线 |
| lock_timeout_is_an_explicit_health_error、health_flag_is_cleared_… | `sqlite_locked` 不推进水位（保持既有语义） |

### 既有断言修改逐条说明（旧设计行为被本规格推翻才改）

- p3 全文件随证据模型重写（backup-API 快照、逐行全表比对、8/512KiB/15min 链阈值、@1 状态格式均被本规格推翻）；逐项语义对应关系在上表，未推翻的行为（锁失败、崩溃顺序、fail-closed、legacy JSON、source_missing、去重拒绝、degraded 一致性）保持原断言或仅随字段更名（`snapshot_sha256→baseline_digest`、`snapshot_object→baseline_chunks[]`）。
- `duplicate_payload_rows` 在 delta 单元改为只计变更行 → 测试 10 的迁移断言 2→1（规格细化，见设计决定 3）。
- `reverted_changes_are_reported_as_a_blind_window` 的回改 SQL 显式还原 `time_updated`（水位法以该列为键，规格定义的盲区即含「整段回改」）。
- 崩溃 adopt 断言不变；`crash_before_manifest_commit` 的孤儿对象数 1→4（整库镜像 → 每表一块）。
- 高频写入测试（13）基线数断言改为比例界 `2..=runs/3+1`（旧 8 条/15min 阈值被 50% 链规则取代）。

## 规模实测（release，`tests/p3_sqlite_scale.rs`，i7/Windows/NVMe）

合成库：WAL 模式，`part` 200,000 行（data ≈2.5 KB）+ `message` 50,000 行（≈1 KB）+ session/project/todo/credential，db 文件 837 MiB；构建 508 s（不计入预算）。

| 阶段 | 实测 | 预算 |
|---|---|---|
| 首次基线 | **2.81 s**，250,001 行，**19 块**，未压缩 567,194 KiB → 压缩 2,230 KiB（比 254:1——合成负载高度重复，真实负载比会低；S3 实测校准），峰值内存 161 MiB | — |
| 稳态采集（改 100 + 增 100 + 删 10） | **2.80 s**（预算 ≤10 s ✓），delta 仅 3 KiB，模式=delta（未重基线），峰值内存 **139 MiB**（预算 <300 MiB ✓） | ✓ |
| `--verify` 全量校验 | 5.61 s，0 mismatches，峰值内存 259 MiB（pk→hash 流式比对；行内容从不整表驻留） | — |

内存测法：测试二进制内 `#[global_allocator]` 计数分配器，测量段前后 `LIVE/PEAK` 原子计数取峰值（方法已写明于测试文件头注释）。

## 命令与退出码

- `cargo test --locked --no-fail-fast` → exit 0（629 passed / 0 failed / 3 ignored）
- `cargo test --release --test p3_sqlite_scale -- --ignored --nocapture` → exit 0
- `cargo build --locked` → 0 warning
- CLI 冒烟（合成库）：`capture --session-since 1970-01-01T00:00:03Z` → ok=true、`rowsDiscovered=4`（2 会话+子行+project）、`excludedTables=[credential]`；`capture --rebaseline` → gen 2、`rebaseline_requested`；`coverage` → `sessionSinceMs=3000`、`excludedTables={credential:1}`；哨兵零命中。

## 修改文件

- `agent-memory/src/capture_sqlite.rs`（重写：分块基线/水位增量/校验/过滤/@2 状态）
- `agent-memory/src/archive/mod.rs`（`ManifestSqliteChunk`、`ManifestSqlite` 基线块事实、`SqliteState` digest/字节/过滤器/校验字段）
- `agent-memory/src/capture.rs`（`CaptureOptions` + `capture_source_with_options`）
- `agent-memory/src/replay.rs`（FK 安全物化：DDL 来自 manifest、foreign_keys=OFF、父表优先、foreign_key_check；FK 单测）
- `agent-memory/src/coverage.rs`（`SqliteCoverage`：baselineDigest/Bytes、excludedTables、sessionSinceMs、校验字段、盲区文本）
- `agent-memory/src/bin/agent-memory.rs`（`--session-since` / `--rebaseline` / `--verify`）
- `agent-memory/README.md`（SQLite 章节重写、manifest 事实、崩溃顺序、SQLite replay、构建/测试数字）
- `agent-memory/tests/p3_sqlite_consistency.rs`（重写，33→40 用例）
- `agent-memory/tests/p3_sqlite_scale.rs`（新增，`#[ignore]` 规模用例）

## 限制与未做

- [UNCERTAIN] 压缩比 254:1 由合成负载的重复性抬高，真实 8.8 GB 库的基线压缩比与绝对耗时要等 S3 小范围试采校准（计划 §4 S3）。
- [UNCERTAIN] 水位候选读取无索引（源只读，按规格不创建），靠 SQLite 全页扫描：稳态 2.8 s @ 837 MiB 合成库；真实库冷缓存下的绝对值待 S3 实测。若超预算，后续可为「建议索引」输出 DDL 供用户自行采纳（仍不自动创建）。
- 非白名单表只有行数事实，不保留 schema（DDL）——计划 §5 D2 的 `event` 表待结构厘清后再决定入白名单与否。
- 无水位列且超大的表走全表哈希路径（正确但代价无界），已在报告中列名告警；真实白名单表均有时间戳/seq 列，当前不触发。
- 未做：S2 采集驱动（capture-all）、61/62/64/90 套件分发物、assets/dist 重建、git commit（orchestrator 执行）；`docs/verification/` 下历史 sentinel 未改动。

---

# §返工 1（对应验收报告 STAGE2-S1-VERIFICATION.md，被验 07b95b1；本节只追加不改写前文）

全部修复在工作区 `C:\wt\stage2-s1`（父提交 f5f61c0，未 commit）；Python 均以 `python -I` 运行、临时文件在专用目录 `%TEMP%\s1-rework-7741`；未读取任何真实数据（约束同前文）。

## B1（阻断）`--verify` 复用 `--session-since` 过滤

- 实现：`run_verification_and_correct` 从观测状态取 `selected_sessions` 与 `session_since_ms`，逐表白名单扫描按同一范围过滤——session/session_v2 加 `WHERE time_updated >= 阈值`，子表逐行按选中会话集过滤（有序归并与有界回退两条路径都过滤）；期望侧本就只含范围内行（基线块按范围导出、delta 只含范围内行），因此范围外行**不补录、不计数、不进入键集**。校验不再回写任何键集/行哈希（观测键集由下一次常规观测在同范围内自行重建），只更新 `last_verifyAtMs`/`verifyMismatchesTotal`；修正单元提交后仅把观测文件的 `capture_seq` 重新封到修正单元序号（崩溃窗口门需要的序号绑定，键不动）。
- 测试：`verify_honors_session_since_filter`——首次 `--session-since` 基线（范围 1 会话+1 message+1 part+project）→ 同参 `--verify`：`verifyMismatches=0`、mode=unchanged、零 delta、replay 仍 4 行（旧会话行不入证据）；范围内盲改 → verify 补 1 update；范围内新增 → 常规增量 1 insert、0 delete；最终 replay 与「源库按同一过滤逐行」一致，且所有 delta 行键不含范围外键。另有常规 `verify_finds_watermark_blind_changes_and_corrects_them`（无过滤路径）保持通过。

## N1 同水位值、低键新行漏采

- 实现：候选判定改为 `value == previous_value` 时 `key > previous_key || (键不在上次键集中)`；键集可信性由 `TableState.primary_keys_known` 门控（崩溃 adopt 的基线状态键集为空且不可信，此时不启用该规则，防止把已归档行误报为 insert）。
- 测试：`same_watermark_value_smaller_keys_are_captured`——24 行同 `time_updated=1000`（键 p100..p123，水位键 p123）基线后，观测后插入 100 行 p000..p099（全部更小键同值）→ 1 次增量 `rows_inserted=100`，replay 124 行与源一致。

## N2 旧会话重入范围一次性补采既有子行

- 实现：`observe_changes` 重构为两阶段——先观测 session/session_v2 并更新选中会话集，得到本次新进入范围的会话（`selected - previous.selected`）；再观测子表时对新进入会话经 TEMP 表 `session_id IN (…)` 一次性读取其全部既有子行，未跟踪键记为 insert（它们此前从未归档）。水印不受补采影响（补采行低于子表水位，此后按键集正常删除检测）。
- 测试：`session_reentering_scope_backfills_child_rows`——阈值 4000 基线（old/mid/new 中后两者在范围内）后 `UPDATE old SET time_updated=7000` → 增量 `rows_inserted=3`（会话+既有 message+既有 part）、0 delete，replay 三会话齐整。
- README 已声明该语义（见 N6 条目）。

## N3 过滤下子表缺 `session_id` 列不再整次失败

- 实现：`export_baseline`/`observe_one_table`/`read_all_rows`/`verify_*` 统一判定「子表且无 session_id 列 → 不做会话过滤、全量采集」，并在报告/覆盖/状态中以 `unfilteredTables` 列名。原失败点（基线导出 SQL 引用不存在的 `session_id`）已消除。
- 测试：`child_table_without_session_id_is_captured_in_full_and_reported`——`todo(id,content)` 无 session_id，`--session-since 4000` 下首次采集 ok、`unfilteredTables=["todo"]`、todo 行全量入证据、coverage 同步列出；后续向 todo 插入仍为常规 delta。

## N4 增量采集内存/IO 与证据总量解耦

- 实现：
  - `ManifestSqliteChunk` 增加 `min_key`/`max_key`（导出序的键范围）；`ManifestSqlite` 增加 `delta_table_ranges`（每个 delta 批次按表的键范围）。
  - `fill_before_from_evidence` 只解压「键范围含目标键」的基线块（`ChunkLineStream` 流式逐行，内存≈单行+解压窗口）与「表键范围含目标键」的 delta 批次；门控为「本次变更含 update/delete」才读证据。
  - delta 批次读取改为 manifest-only 元数据（`sqlite_delta_batch_metas`）+ 按需 `read_delta_lines`；`replay_sqlite` 同样先取元数据，只解压目标代与序号范围内的批次（验收报告 N4「同类」问题一并修复）。
  - 新增进程级度量 `evidence_decompressed_bytes()`（含重置接口），覆盖全部证据解压路径（块、delta 对象）。
- 测试：`p3_sqlite_decompress_isolation.rs`（独立测试二进制——计数器是进程级原子量，独立进程避免并发测试污染）断言：无变化观测解压 0 字节；纯 insert 增量解压 0 字节；含 update/delete 的增量只解压被触及的块与批次（≤ 基线总量）。规模断言见下。

## N5 `--verify` 内存有界

- 实现：可排序表（有主键或 rowid，即全部真实白名单表）走**流式归并**——期望侧为基线块流（`ChunkLineStream` 逐行哈希，按导出序）+ delta 修正覆盖层（`key → None | Some(hash)`，内存∝delta 链不∝行数），源侧为同序有序扫描（`order_suffix` 统一为主键序优先）；两侧按键归并判定 insert/update/delete 修正。重复/NULL 主键在主键序下相邻，`#rowid:`/`#dup:` 后缀以 O(1) 状态复现导出规则。无序表（无主键且无 rowid，`row:hash` 键）保留有界映射回退路径。校验不再回写键集/哈希（见 B1）。`order_suffix` 由 rowid 优先改为主键优先以对齐两侧流序。源连接增加 `PRAGMA cache_size=-16000`（16 MiB 页缓存上界，采集内存可预测）。
- 测试与实测：规模用例在 250,001 行（200k part×2.5KB+50k message）与 2 倍行数（450,181 行）分别跑 `--verify`，峰值内存与增长见下表；0 mismatch（归并与源逐行一致）。

## N6 README 与 CLI 帮助写明固定阈值语义

- README「Session filter」小节新增四条生产语义：阈值必须固定（滚动阈值每次触发 `filter_changed` 整库重基线）；去掉过滤全量回填恰好触发一次重基线；`--verify` 必须带同一过滤；旧会话重入范围补采与无 session_id 子表全量采集（`unfilteredTables`）。盲区声明同步收窄（同值未见键可采）。CLI `--help` 的 `--session-since` 处写入相同警示。

## N7 移除 rusqlite `backup` 特性

- `Cargo.toml` 两个 target 的 rusqlite features 改为 `["bundled"]`；`Cargo.lock` 无需变化（特性不改变依赖图），`cargo build --locked` 通过。代码中已无 backup API 调用（上一轮已删）。

## 返工后测试

- `cargo test --locked --no-fail-fast` → **634 passed / 0 failed / 3 ignored**（前轮 629/0/3；新增 B1/N1/N2/N3/N4 五个用例与独立隔离二进制 1 个用例）。`cargo build --locked` 0 warning；`git diff --check` 干净。
- 新增/修改用例清单：`verify_honors_session_since_filter`（B1）、`same_watermark_value_smaller_keys_are_captured`（N1）、`session_reentering_scope_backfills_child_rows`（N2）、`child_table_without_session_id_is_captured_in_full_and_reported`（N3）、`observations_without_before_images_decompress_no_evidence`（N4，独立二进制 `p3_sqlite_decompress_isolation.rs`）、`verify_finds_watermark_blind_changes_and_corrects_them` 与既有 40 用例适配新路径（N5 流式归并、filter 语义）后全部通过。
- 既有断言改动说明：仅 `verify_finds…` 的内部行为断言随「校验不再回写键集」调整（前文该断言依赖已废弃的校验回写）；其余既有断言未改。

## 返工后规模实测（release，`--ignored`，单次完整运行）

合成库 A：837 MiB，250,001 行（part 200k×≈2.5KB + message 50k×≈1KB + session/project/todo/credential）；库 B = A 复制后 part 键前缀翻倍（450,181 行）。

| 阶段 | 实测 | 预算/断言 |
|---|---|---|
| 基线 A | 3.53 s，19 块，567,194 KiB → 2,230 KiB（比 254:1，合成负载偏高），峰值 161 MiB，每块含键范围 | ✓ |
| 稳态 A（改100+增100+删10） | **1.11 s**（≤10 s ✓），delta 3 KiB，峰值 **62 MiB**（<300 ✓）；**解压证据 56,242 KiB = 基线的 9%**（断言 < 基线 25% ✓；修复前该路径整链整表 ≈ 基线量级、耗时 2.8-3.0 s） | ✓ |
| 无变化观测 A | 解压证据 **0 字节** | ✓ |
| `--verify` A（250k 行） | 5.25 s，0 mismatch，峰值 **59 MiB**（<300 ✓；修复前 259 MiB） | ✓ |
| 基线 B（450,181 行） | 35 块，每块键范围齐全 | ✓ |
| `--verify` B（2× 行） | 9.47 s，0 mismatch，峰值 **109 MiB**（<300 ✓）；较 1× 增长 +51 MiB | ✓（≤100 MiB 断言通过） |

内存测法同前文（`#[global_allocator]` 计数分配器，测量段峰值）；解压字节数测法为采集器内置度量（覆盖基线块与 delta 对象全部解压路径）。

## 返工限制（新增/更新）

- verify 峰值 59→109 MiB 的 2× 增长（+51 MiB）来自源连接的 `ORDER BY <pk>` 扫描/排序机制（SQLite 侧），采集器自身状态与流式结构为 O(delta)+O(1)；按真实库规模（part 227k 行）对应 200k 档 ≈60 MiB，余量充足。[UNCERTAIN] 真实库冷缓存与索引形状下的绝对值待 S3 实测。
- 稳态解压 9% 的上界由「被触及块数」决定：OpenCode 真实键近似随机分布时，100 个变更键可能触及全部 19 块（上界=全部块字节），但仍是「触及的块」而与历史链长无关；持续增长的 delta 链按 50% 规则重基线归零。[UNCERTAIN] 真实键分布下的稳态解压量待 S3 实测。
- N4 的 `delta_table_ranges`/`min_key`/`max_key` 为 @2 manifest 新增字段（serde 默认空）；空值回退为「解压该表全部块/批次」，生产证据库为空、无兼容负担。
- 校验不再回写观测键集：范围外行不进键集由「范围过滤扫描 + 不回写」共同保证；观测键集始终来自常规观测的同范围扫描。
- 未做（同前文）：S2 采集驱动、61/62/64/90、assets/dist、git commit。

---

# §返工 2（对应独立复验报告 STAGE2-S1-REVERIFICATION.md，被验状态 = §返工 1 完成时的 d16290a；本节只追加不改写前文）

工作区 `C:\wt\stage2-s1`（分支 `stage2-s1-sqlite`，未 commit，由 orchestrator 提交）。约束同前文：全程未读取任何真实数据；临时文件在专用目录 `C:\wt\rework2-tmp`（未用 `%TEMP%\opencode`）；本轮未用到 Python。本轮由两次会话完成：第一会话完成 R1/R2/N-R3/N-R4 主体实现与 14 个新增用例后因模型额度用尽（429，code 1308）中断、未写记录；第二会话在其未提交改动基础上继续——修正 4 个中断会话遗留的未跑通用例（夹具缺陷 1、行数计数错误 2、未注入 failpoint 1），补上流式归并的 drain 修复与 README/报告文案，然后全套测试与规模实测通过。

## R1（阻断）补采跳过已读主键

- 实现：`extend_backfill` 对补采行不再做 `#rowid:N` 消歧——键已被本次观测的常规水位读取记过（在当前键集中）或已在上一观测键集中（已归档）时一律跳过，不再产出幻影 insert/delete 对。补采集合从「本次新进入范围的会话」扩大为「新进入 ∪ 本次被更新过的会话行」（同 id 重建、被重新触碰的会话同样补采其未归档子行）；上一观测键集不可信（崩溃 adopt 后为空）时整体跳过补采，该声明盲区由 N-R4 的 adopt 后自动校验愈合。
- 测试：`backfill_reentry_with_new_child_rows_has_no_phantoms`（复验建议的主形态：重入会话同时有高于水位的新子行——insert 计数恰好为会话 + 2 个水位新子行 + 2 个补采子行，replay 与按同一过滤的源库逐行一致、delta/replay 全程无 `#rowid:`/`#dup:` 键、下一周期 `skipped_unchanged`）；`backfill_recreated_same_id_session_has_no_phantoms`（同 id 重建：1 delete + 2 insert + 1 update，无幻影，下一周期干净）；`backfill_child_modified_while_session_out_of_scope_has_no_phantoms`（范围外子行被改后会话入范围：改动的子行走水位读取、未归档兄弟行走补采、无幻影）。

## R2（阻断）与 SQLite BINARY 排序一致的类型化比较器

- 实现：`--verify` 流式归并的键比较由 Rust 字符串序改为解码后的类型化排序元组 `SortValue`（`NULL < INTEGER/REAL < TEXT < BLOB`；数值按值、文本/BLOB 按字节序，即 BINARY collation；`pk:[…]` 逐分量解码、`rowid:N` 为单整数、`#rowid:`/`#dup:` 消歧后缀按基键参与排序）。期望侧与源侧两路流分别做单调性检查；主键含非 BINARY collation 的表经 `PRAGMA index_list`/`index_xinfo` 预先检出，以及运行中检出顺序错位的表，当次即清空归并结果、回退既有 `verify_table_bounded` 有界映射路径（同一 run 内完成，修正语义一致），并在报告以 `verifyBoundedTables` 列名。整数、复合、rowid 键表不再出现「1 行真实差异放大为整表修正」的假 mismatch 风暴。
- 本轮追加修复（归并 drain）：源侧扫尽后，期望流中「大于全部源行」的 pending 条目原先不会被判定——删除最高键行时该校正会被永久漏掉。现 drain 先消费 pending 条目再继续读流。常规观测本身能发现删除（pk 扫描），该窗口只在崩溃 adopt 后（键集不可信、删除不可见）真实出现，由 adopt 用例覆盖。
- 测试：`verify_merge_matches_sqlite_order_for_integer_keys`、`..._for_composite_keys`（OpenCode `todo` 形 `(session_id, position)`）、`..._for_rowid_tables`、`..._for_non_ascii_text_keys`、`..._for_mixed_type_keys`（TEXT×INTEGER 跨类型档位）——单行盲插入各报 `verifyMismatches==1`（混合型两行盲插入报 2），replay 行数正确；`verify_blind_update_is_one_mismatch_for_integer_keys`（盲内容更新 1 mismatch、1 update 修正）；`verify_non_binary_collation_table_takes_bounded_path`（NOCASE 主键表走有界路径并出现在 `verifyBoundedTables`）；`verify_merge_is_idempotent_for_integer_keys`（连续两次 `--verify` 均 0 mismatch）。既有 `verify_finds_watermark_blind_changes_and_corrects_them`、`verify_honors_session_since_filter` 在新路径下保持通过。

## 崩溃 adopt 漏行经 `--verify` 补录（N-R4 实现 + R2 数据丢失路径回归）

- 实现：基线 adopt 重建的观测状态 `last_verify_at_ms=0`，下一次采集（含无变化周期）自动触发全量校验，把 adopt 窗口内的删除与低键同值插入补录为 verifyCorrection delta；`--verify` 立即强制同一检查。delta adopt 的观测是重算的、本身完整，不设该窗口（保持原 `last_verify_at_ms`）。基线 adopt 的 degraded 报错文案改为指向该机制（「下一次采集自动校验，`--verify` 可立即触发」）。
- 测试：`adopt_window_deletes_and_low_key_inserts_are_verified`——`--rebaseline` 提交后、观测状态写入前崩溃（failpoint），重跑得 `recovered_after_crash`（ok=false）；随后删除 prt_05 与最高键 prt_24、插入低键同值 prt_000（常规观测三者均不可见），下一次采集自动校验：`verifyMismatches=3`（2 删 1 插，其中 prt_24 的删除只能由 drain 产出），replay 与源库逐行一致，再下一周期 `skipped_unchanged`。

## N-R3 空 `delta_table_ranges` 回退读取

- 实现：`fill_before_from_evidence` 与 `build_delta_overlays` 对「无范围记录」的 delta 批次视为「可能包含任何键」并始终读取（有范围记录的批次仍按范围裁剪）；基线块缺 `min_key`/`max_key` 视为必读的既有语义不变。README 同步写明该语义。
- 测试：`legacy_delta_batches_without_ranges_still_provide_before_images`——把全部 manifest 的 `deltaTableRanges` 置空后再更新同一行，其 update 行 `before` 取自上一 delta 批次（v1 值）而非基线（v0 值），replay 与源一致。

## N-R4 / N-R5 文档与空白

- README 四处同步（规范态表述）：会话重入条目——重入或同 id 重建的会话补采其未归档子行，且补采永不重复记账（已采集/已归档的键直接跳过）；校验条目——两路流按 SQLite 排序（类型化元组、BINARY 字节序）归并，非 BINARY collation 或运行中错位的表当次回退有界路径并记入 `verifyBoundedTables`；增量读取条目——无范围记录的批次始终读取；崩溃条目——基线 adopt 的窗口由下一次采集自动校验愈合、`--verify` 可立即触发。
- `tests/p3_sqlite_decompress_isolation.rs` 末尾多余空行删除，`git diff --check` 干净（exit 0）。

## 返工后测试

- `cargo test --locked --no-fail-fast`（C:\wt\stage2-s1\agent-memory）→ **648 passed / 0 failed / 3 ignored**（§返工 1 后为 634/0/3；净增 14 用例：R1×3、R2×8、adopt×1、N-R3×1、锁失败语义×1）。`cargo build --locked` 0 warning。
- 中断会话遗留 4 个未跑通用例的修正说明：`backfill_recreated_same_id_session_has_no_phantoms` 原夹具对不存在的 `ses_x` 会话行做 UPDATE（m_x1 从未入库，delete 断言必然失败）——重建为自含夹具（ses_x 在基线内），并按实际语义改断言为 1 delete + 2 insert + 1 update（同 id 重建的会话行是 update）；composite/mixed 两用例的 replay 期望行数按 `0..=24` 实际行数（24 行基线）修正为 25/26；adopt 用例原先根本未注入 failpoint（不会崩溃、断言的 adopt 形态不成立）——改用 `capture_source_with_options(..., Some(Failpoint::AfterManifestBeforeState), rebaseline)`。另将一个名不副实的用例更名为 `verify_merge_is_idempotent_for_integer_keys`（其内容本就是幂等性验证，不触发错位回退）。

## 返工后规模实测（release，`--ignored`，单次完整运行，总计 535.07 s）

合成库：837 MiB，part 200,000 行（≈2.5 KB）+ message 50,000 行（≈1 KB）+ session/project/todo/credential；构建 507.7 s（不计预算）。

| 阶段 | 实测 | 预算/断言 |
|---|---|---|
| 基线 A（250,001 行） | 2.79 s，19 块，567,194 KiB → 2,230 KiB（254:1，合成负载偏高），峰值 161 MiB | — |
| 稳态 A（改 100 + 增 100 + 删 10） | **0.97 s**（≤10 s ✓），delta 3 KiB，解压证据 56,242 KiB = 基线 9%，峰值 **62 MiB**（<300 ✓） | ✓ |
| `--verify` A（@250k） | 4.02 s，0 mismatch，峰值 **59 MiB**（<300 ✓） | ✓ |
| 基线 B（2× 行，450,181 行） | 35 块 | — |
| `--verify` B（@2×） | 7.62 s，0 mismatch，峰值 **109 MiB**，较 1× 增长 +51 MiB | ✓（≤100 MiB 断言通过） |

内存测法同前文（`#[global_allocator]` 计数分配器取测量段峰值）。稳态解压 9%、`--verify` 峰值 59→109 MiB 与 §返工 1 记录一致：R2 只改比较器与回退路由，未改变内存形状；R1/N-R4 只作用于补采与校验触发时机，稳态路径不变。

## 命令与退出码

- `cargo test --locked --no-fail-fast` → exit 0（648 passed / 0 failed / 3 ignored）
- `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture` → exit 0（535.07 s，`opencode_scale_capture_is_incremental_and_bounded` 通过，预算行全部 true）
- `cargo build --locked` → 0 warning
- `git diff --check` → exit 0
- 被验源码未读任何真实数据；未 git commit（由 orchestrator 执行）

## 修改文件

- `agent-memory/src/capture_sqlite.rs`：R1（补采跳过已读/已归档键、补采集合扩展、不可信键集跳过）；R2（`SortValue` 类型化比较器、`sort_values_from_key`、collation 检测与有界回退、`verify_bounded_tables` 报告字段、归并单调性检查与 drain 修复）；N-R3（无范围批次始终读取）；N-R4（adopt 后自动校验、degraded 文案）。
- `agent-memory/tests/p3_sqlite_consistency.rs`：+14 用例（R1/R2/adopt/N-R3/锁语义）与共享夹具；4 个中断会话用例的修正与 1 处更名。
- `agent-memory/tests/p3_sqlite_decompress_isolation.rs`：末尾空行删除（N-R5）。
- `agent-memory/README.md`：R1/R2/N-R3/N-R4 四处语义（规范态）。

## 返工限制（新增/更新）

- WITHOUT ROWID 表无独立 pk 索引，collation 预检不覆盖，依赖归并自身的单调性检查兜底（检出错位当次回退有界路径并列名）；常规 BINARY 主键不受影响。[UNCERTAIN] 非 BINARY collation 的 WITHOUT ROWID 表属构造性场景，真实 OpenCode 白名单表未触发。
- `verifyBoundedTables` 为采集报告新增字段（serde 默认空，@2 兼容）；有界回退路径内存 ∝ 表行数（与 §返工 1 之前的 verify 同量级），仅对 collation 异常/错位表触发，规模用例未触发（报告为空）。
- R2 的整数/实数混合比较按 IEEE-754 位序实现，绝对值超过 2^53 的整数键与实数键存在理论边界情形；真实白名单表主键为文本 id 或小整数位置，不触发。[UNCERTAIN] 真实库冷缓存与键分布下的绝对耗时/内存仍待 S3 实测（同前文）。
- 未做（同前文）：S2 采集驱动、61/62/64/90 套件分发物、assets/dist 重建、git commit。
