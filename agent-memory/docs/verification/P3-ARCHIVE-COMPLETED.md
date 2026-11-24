# Archive P3 完成记录 · OpenCode 与 SQLite 一致性

范围：计划 `20261024-agent会话原始证据归档.md` 的 **P3** 阶段（OpenCode 旧 JSON
storage、SQLite V1/V2，SQLite 原始证据 = 基线快照 + 原始行级差分）。P4–P6 未做。
真实数据：**否**。全部用例只使用 `tempfile` 临时根与合成 fixture，未指向、未读取任何
真实会话目录或真实数据库。Git：已提交（P3 基础实现 `b52499d`；验收后的 F1/F2 修复由
orchestrator 提交，见 `P3-FIX-COMPLETED.md`）。

## 1. 实施文件

| 文件 | 动作 | 职责 |
|---|---|---|
| `agent-memory/src/capture_sqlite.rs` | 新增 | P3 证据引擎：rusqlite backup 一致性快照（zstd）、周期一致性读取、行级差分、基线触发策略、重放/回查、崩溃后接管、健康错误 |
| `agent-memory/src/archive/mod.rs` | 修改 | `ManifestSource.sqlite` / `ManifestSqlite`（采集事实）、`SourceState.sqlite` / `SqliteState`（采集状态）；全部 `serde(default)` 向后兼容；目录布局注释 |
| `agent-memory/src/capture.rs` | 修改 | `SourceKind::Sqlite` 路由（provider `opencode` + SQLite 文件魔数，先于整文件指纹短路）；`CaptureReport.sqlite` |
| `agent-memory/src/coverage.rs` | 修改 | 每源 `sqlite` 覆盖块（观测水位、观测间隔盲区、快照代际、差分链、行计数、异常）；总量 `sqliteBaselines` / `sqliteDeltas` / `sqliteDuplicatePayloadRows` |
| `agent-memory/src/lib.rs` | 修改 | 模块注册 |
| `agent-memory/tests/p3_sqlite_consistency.rs` | 新增 | P3 验收用例 33 项（合成 fixture + 临时根） |
| `agent-memory/Cargo.toml` | 修改 | `[dev-dependencies]` 增加同版本 `rusqlite`（测试构造合成数据库；生产依赖不变） |
| `agent-memory/README.md` | 修改 | P3 证据模型、触发条件、重放、健康语义、覆盖字段、布局、原子性、已知限制 |
| `CURRENT.md` | 修改 | 任务状态重置为"无" |

未触碰：`src/providers/**`、`src/recall/**`、`src/mcp.rs`、`src/codex_*`、
`tests/p0_formats.rs`、`tests/p1_archive.rs`、`tests/p2_jsonl_incremental.rs`、
`tests/recall_*`、`tests/fixtures|support/**`、安装脚本与分发物、Obsidian 库。

## 2. 证据模型（实现）

- **基线快照**：`rusqlite::backup` 从只读源连接备份到临时库（主库 + WAL 的一致性合成），
  读取临时库字节 → sha256 → zstd(level 3) → 内容寻址对象（role `sqlite_baseline`）。
  绝不在活跃库上直接复制文件字节。临时库与快照观测文件落在运行时根的
  `archive/staging/` 内，用后即删。
- **周期一致性读取 + 行级差分**：每次运行在一个读事务内读取全部表（`BEGIN`…`COMMIT`），
  与"上次观测"逐行比较，按主键/rowid/内容哈希定位行。每条差分记录（JSONL 一行）字段
  固定为：`capture_seq`、`observed_at_ms`、`table`、`key`、`op`
  （`insert|update|delete`，按观测差分判定）、`before`、`after`。整批作为 zstd 对象
  （role `sqlite_delta`），并为每行写一条可寻址、可哈希校验的 manifest record。
- **不实现的机制**（按计划）：session extension、源库触发器/shadow table、WAL 帧或
  btree 页解码。源连接始终 `SQLITE_OPEN_READ_ONLY`。
- **观测状态**：`state/sqlite/<hash32>.obs.zst` 保存上一次观测的行映射，是采集状态而非
  证据；文件绑定来源键、所属 `captureSeq` 与自身行摘要（`rowDigest`），损坏/非蓄意篡改
  会被识别为 `observation_state_unreadable`，并以去重基线自愈，而不是拿被改过的行做差分。
  `captureSeq` 必须落在 `[state.seq, state.seq+1]`：**`+1` 只有在 `committed_unit_at(seq)`
  确实存在（真实崩溃窗口）时才被接受**，否则一律拒绝——避免伪造的 `+1` 状态把从未观测过的
  行当作 `before` 写进差分。丢失时同样触发新基线（若快照内容相同则去重，不产生第二基线）。
- **重放**：`replay_sqlite(archive, source, upto_capture_seq, domain)` 取"≤ 目标采集序号
  的最近基线"，按序应用该代际的差分，输出 `table → key → 行内容`，并保留列信息。
  未知表/列原样出现；`read_baseline_snapshot` 返回精确快照库字节，可直接用 SQL 回查。
  全部读取经调用方访问域（fail-closed，跨域 `AccessDenied`）。
- **行内容不是统一模型投影**：值为原始形态（整数/实数/文本；非法 UTF-8 与 blob 以
  `{"invalid_utf8_hex":…}` / `{"blob_hex":…}` 原样保留），不以 SessionMessage 结构替代。

### 2.1 快照触发策略

| 触发 | 原因标签 | 该轮健康 |
|---|---|---|
| 首次采集 | `initial` | ok |
| 观测 schema 与基线代际不同 | `schema_change` | ok（采集事实） |
| 出现基线代际未知的表/列 | `unknown_table_or_column` | ok（采集事实） |
| 差分链长度 ≥ 8 / 原始差分字节 ≥ 512 KiB / 链龄 ≥ 15 min | `delta_chain_threshold` | ok（采集事实） |
| 观测状态文件缺失/损坏、基线对象缺失、状态不完整 | `observation_state_missing` / `baseline_unavailable` / `unclassifiable_state` | **degraded / ok=false** |
| 快照内容与现有基线逐字节相同 | 不新建基线；`snapshotDeduplicated:*` 作为采集事实报告（基线对象必须仍在档；链阈值去重时清零差分链） | 随触发原因（`observation_state_missing` 仍为 degraded） |

WAL 增长、mtime 变化、小写入一律**不**触发整库快照：只报告 `walBytes` 并在需要时写行差分。
高频小写入下基线代际由差分链阈值约束（实测 40 次写入 → 5 个基线代际，见用例 12）。

### 2.2 健康与失败语义

- `sqlite_locked`（busy 超时，含"preflight 读之后才被加锁"的窗口：备份步进按有界期限重试，
  不依赖 backup API 的 busy handler）与 `sqlite_unavailable`（不可读/损坏）：`ok=false`、
  `status=failed`、计数错误、写 `anomalies` 与 `sqlite.lastUnavailable`，不写对象/清单、
  不推进水位；覆盖报告不返回空成功（用例 10、11、23）。
- 健康标记只在下一次**成功观测**时清除（无论是否有行变化），因此瞬时锁冲突不会让
  `ok=false` 永久滞留（用例 22）。
- 正常快照触发（`schema_change` / `unknown_table_or_column` / `delta_chain_threshold`）
  是采集事实，只出现在 `snapshotTrigger` / `lastSnapshotReason`，不当作异常、不翻转 `ok`。
- **无法安全增量一律 degraded**：`observation_state_missing`、`baseline_unavailable`、
  `unclassifiable_state` 三个触发都会提交/复用可用的基线，但该轮报 `status=degraded` /
  `ok=false`（CLI 退出 7）；观测中途 schema 漂移同样 `degraded`。崩溃后接管的
  `recovered_after_crash` 也报 degraded（本轮之前发生过崩溃），下一轮恢复健康。由此
  **SQLite 来源的 `report.ok` 与 `coverage.ok` 始终一致**（用例 2、24、26、29、30、31、32）。
- **无先验状态时不得误判崩溃接管**：只有存在状态且清单确实占据 `state.seq+1` 才走接管；
  状态整体丢失时历史清单不算崩溃窗口，按 `unclassifiable_state` 重建基线并报 degraded
  （用例 32）。
- **去重不得掩盖证据缺失**：只有基线对象仍在归档中、且状态可自愈的快照才允许去重；
  基线对象丢失时相同内容会重新入档（`baseline_unavailable`，用例 25），不会报"无变化"。
  去重本身不改变"本轮 degraded"的判定。
- 损坏的 OpenCode 库头（`.db`/`.sqlite`/`.sqlite3` 名称或魔数不符）路由到 SQLite 路径并
  以 `sqlite_unavailable` 失败，不静默降级为整文件采集（用例 27）。
- 源文件删除只改来源状态（`source_missing`），归档仍可重放与读取基线（用例 13）。
  删除时待处理的 `lastUnavailable` 按历史处理（仍计入 `errorsTotal`），否则一次旧锁超时会让
  `coverage.ok=false` 永久滞留、与 `source_missing` 的 `ok=true` 冲突（用例 22 第二阶段）。
- 单写者锁沿用 Archive 根锁，第二实例 `LockHeld`（用例 14）。

## 3. 测试

```sh
cd agent-memory
cargo test
```

实测（2026-10-25，worktree `C:\wt\archive-p3-20261025`）：**exit 0，
320 passed / 0 failed / 1 ignored**。

| 目标 | 数量 |
|---|---|
| lib（单元，含 4 项 P3 差分/去重/未知 schema 单元） | 161 |
| `boundary_no_default_roots` | 5 |
| `p0_formats` | 4 |
| `p1_archive` | 8（未改） |
| `p2_jsonl_incremental` | 17（未改） |
| **`p3_sqlite_consistency`** | **33（新增）** |
| `recall_boundary` / `recall_cli_mcp` / `recall_contract` | 6 / 7 / 9 |
| `recall_kb_device` / `recall_opencode` / `recall_read` | 25 / 22(+1 ignored) / 18 |
| doctests（5 个 compile-fail 反向用例） | 5 |

P3 用例（全部合成临时根）：

1. 首次采集 = 一个一致性基线快照（读回为合法 SQLite 库、行内容一致、源字节未变）
   `initial_capture_is_a_consistent_baseline_snapshot`
2. 重复采集跳过；观测状态丢失时快照去重（仍 1 个对象/1 个基线）并以 degraded 报告
   （`report.ok == coverage.ok`），随后恢复行差分
   `repeated_capture_and_repeated_snapshot_are_deduped`
3. 周期行差分 + 逐观测点重放（seq 2/3/4 各自恢复；差分行恰为 7 个可观测字段）
   `periodic_deltas_replay_to_each_observation_point`
4. 两次观测间"改了又改回"：报告为未变更 + 观测间隔盲区，不虚构观测点；CLI JSON 同样呈现
   `reverted_changes_are_reported_as_a_blind_window`
5. 未知表/未知列从基线快照与差分记录回查
   `unknown_table_and_columns_are_recoverable`
6. schema 变化（加列/删列）各自触发新基线，旧代快照仍可读
   `schema_change_triggers_a_new_baseline`
7. WAL 活跃（写连接保持打开、autocheckpoint 关闭）：行差分可采，基线仍为 1 个
   `wal_active_writes_are_observed_without_extra_baselines`
8. part 增长（新增 part + 原 part 变长）= 行差分，不触发快照
   `part_growth_is_a_row_delta`
9. V1/V2 并存与迁移：两代原始行都保留，跨表相同 payload 计入 `duplicatePayloadRows`
   `v1_v2_coexistence_keeps_both_generations_and_counts_duplicates`
10. 锁超时 = 显式健康错误，覆盖报告 `ok=false` 且非空；解锁后恢复
    `lock_timeout_is_an_explicit_health_error`
11. 数据库不可用（损坏文件）= 显式失败，不返回空成功
    `unavailable_database_is_not_an_empty_success`
12. 高频小写入（40 次）下基线代际有界（5 个，≤ 40/8+1），末态重放与源库逐行一致
    `high_frequency_writes_keep_baseline_generations_bounded`
13. 源删除：状态变 `source_missing`，重放与基线读取仍完整
    `source_deletion_keeps_the_archived_evidence_readable`
14. 第二实例被单写者锁拒绝
    `second_capture_instance_is_refused`
15. 清单提交前崩溃：无水位、孤儿对象上报、重跑复用同内容地址
    `crash_before_manifest_commit_leaves_no_watermark`
16. 清单提交后、状态落盘前崩溃：下次运行接管已提交单元，不写第二份
    `crash_after_manifest_adopts_the_committed_unit`
17. 旧 JSON storage：整文件入档，相同内容只存一个对象，重复采集无操作
    `legacy_json_storage_is_archived_and_deduped_by_content`
18. V2-only 数据库（当前默认写格式）可采、可增量、可重放
    `v2_only_database_is_captured_and_replays`
19. 回读 fail-closed：跨访问域拒绝（重放/差分/基线读取）
    `sqlite_readback_is_fail_closed_across_access_domains`
20. 差分链字节阈值（单次 600 KiB 变更）触发新基线
    `delta_chain_byte_threshold_triggers_a_baseline`
21. 可重复 NULL 主键不互相覆盖（键冲突以 rowid 消歧）
    `nullable_primary_keys_do_not_overwrite_rows`
22. 瞬时锁故障后，下一次"成功但无变化"的观测清除健康标记（覆盖恢复 `ok=true`）
    `health_flag_is_cleared_by_a_successful_unchanged_observation`
23. preflight 之后才取得的锁：快照重试有界，报 `sqlite_locked` 而非挂住
    `lock_taken_after_preflight_fails_as_a_health_error`
24. 观测状态损坏：报 `observation_state_unreadable` + `observation_state_missing`、
    以去重自愈并报 degraded（`report.ok == coverage.ok`），CLI 退出 7，随后继续行差分
    `corrupted_observation_state_is_reported_and_healed`
25. 基线对象丢失且库未变化：禁止去重，重建基线并报 `degraded`；新基线可读、下一轮恢复健康
    `lost_baseline_object_is_rebuilt_and_reported`
26. 观测状态 `captureSeq` 与证据水位不符：拒绝该状态、报异常、重做基线并报 degraded，
    随后恢复差分
    `observation_state_with_a_foreign_capture_seq_is_rejected`
27. 损坏的 `.db` 头：路由为 SQLite 并显式 `sqlite_unavailable`，不静默整文件成功
    `damaged_database_header_is_a_health_error`
28. 链达阈值但内容已回到基线（差分相互抵消）：去重并清零差分链，之后不再反复整库快照
    `deduplicated_threshold_baseline_resets_the_delta_chain`
29. 伪造 `state.seq+1` 且无对应已提交单元：拒绝该状态（即使 `rowDigest` 被重算），
    不写出 `before` 为伪造内容的差分；真实崩溃窗口（`+1` 有清单）仍被接管
    `forged_next_sequence_state_is_rejected_without_a_committed_unit`
30. preflight 与观测之间的 schema 漂移：不写差分，报 `degraded` /
    `schema_changed_during_observation`（`report.ok == coverage.ok`），下一轮取基线
    `schema_drift_during_observation_is_degraded`
31. 不可分类状态（`known_columns` 被清空）：提交新基线、报 `degraded`、修复状态并恢复健康
    `unclassifiable_state_is_degraded_and_repaired`
32. 观测状态损坏且 `sources.json` 丢失（无可用先验状态）：不得误判为崩溃接管，
    以 `unclassifiable_state` 重建基线并报 degraded（`report.ok == coverage.ok`）
    `orphaned_corrupt_observation_state_is_degraded_without_state`
33. 崩溃后源库继续写入（接管无法校验）：不写恢复异常、按新序号写正常差分，
    `report.ok == coverage.ok`，seq 2/3 均可重放
    `crash_then_new_writes_falls_back_to_a_clean_delta`

## 4. 覆盖报告口径

每源 `sqlite` 块：`observationSeq`（观测/证据水位）、`baselineGeneration` /
`baselineCaptureSeq` / `baselinesTotal`、`deltaChainLength` / `deltaChainBytes` /
`deltasTotal`、`rowsDiscoveredLatest`、`rowsInserted/Updated/DeletedTotal`、
`duplicatePayloadRows`、`lastObservedAtMs` / `previousObservedAtMs` /
`observationIntervalMs`、`walBytes`、`lastUnavailable`、`schemaFingerprint`、
`knownTables`、`lastSnapshotReason`，以及固定文本 `blindWindow`。
`ok` 只看当前健康（failed/degraded、最近一轮异常、未清除的 `lastUnavailable`），
不看生命周期计数。发现/已采/跳过沿用 P2 量纲显式命名（`discoveredLatest` /
`capturedTotal` / `skippedTotal`），SQLite 下分别对应观测行数、本轮提交行数、
未变化行数。

## 5. 已知限制

1. **P4–P6 未做**：附件/ContentRef、provider 级覆盖对账与派生重放、CLI 回读/覆盖入口的
   完整契约、套装离线接入与预编译分发物、真实数据试采，均未实现。`coverage` 仍是每源
   投影，不是 P5 覆盖报告；`replay_sqlite` 是行级重放，不是统一 Session/Message 派生层。
2. **观测盲区不可消除**：两次观测之间完全改回的行与"从未变化"不可区分。报告给出观测
   水位、观测间隔与 `blindWindow` 文本，不宣称任意时刻保真、不宣称事务边界。
3. **观测状态丢失/损坏即新基线**：`state/sqlite/*.obs.zst` 缺失、损坏或被改动（来源键、
   `captureSeq` 不在 `[state.seq, state.seq+1]`、`state.seq+1` 无对应已提交单元、
   `rowDigest` 不符）时无法安全差分，只能重做基线（若内容与现有基线相同且基线对象仍在则
   去重并记录 `snapshotDeduplicated:*`，否则提交新基线），并如实报 `degraded`；损坏情形
   额外报 `observation_state_unreadable`。这是有意的安全降级。
4. **差分链上限是常量**：长度 8、字节 512 KiB、时长 15 min 写死为 `pub const`，未做
   可配置；达到上限即新基线（若内容已回到基线则去重并清零链，见用例 28）。首版不自动
   删除任何基线代际，存储随写入量增长（压缩后快照 + 压缩差分），代际保留策略留待 §9
   决策落地。
5. **崩溃窗口后的接管有前提**：清单已提交而状态未写入时，重跑用"上次观测 + 已提交差分"
   与实时观测比对；若期间又有新写入则无法接管，改以新序号写下一批（重放仍收敛，但会
   出现一次同一逻辑变更的两批证据；该轮按正常差分报告，不写 `recovered_after_crash`，
   见用例 33）。若源在崩溃后立即被删除，接管不会发生，状态水位会滞后于已提交证据
   （证据本身仍可重放，覆盖报告如实显示）。**首次采集**（尚无状态）在清单提交后崩溃时
   也不接管：重跑会再写一个基线代际（多一份全量快照，证据一致），因为没有任何状态能
   证明这是崩溃窗口而不是一次全新采集。
6. **重复项去重是事实而非合并**：相同对象只存一份、相同快照不产生第二基线；
   V1/V2 同名内容以 `duplicatePayloadRows` 计数，两代原始行都保留（合并属派生层，P5）。
7. **观测成本与库大小线性**：每次运行读取全部表行（这是"周期一致性读取"的语义），未做
   表级廉价信号筛选；超大库需按计划评估观测周期。快照仅在触发时执行。
8. **无统一模型探测**：SQLite 源不做 `load_messages` 解析探测（`parse.attempted=false`），
   派生解析属 P5；行内容本身按原始形态保存。
9. **健康标记的清除**：`sqlite.lastUnavailable` 只在下一次成功观测时清除；源被删除后该
   标记保留，覆盖报告持续 `ok=false`（不掩盖曾经的锁超时/不可用）。
10. **`coverage` 成本**：按源扫描全部清单（O(源 × 清单)），与 P2 相同，索引化留 P5。
11. **快照在内存中物化**：备份库字节、zstd 结果与差分原始字节均一次性读入内存后再
    落盘；超大库需要按 §9 决策评估分块/流式压缩，当前实现按"单库可容纳于内存"设计。
12. **索引/虚拟表**：只归档表与行，不单独归档索引定义；`CREATE VIRTUAL TABLE` 的
    shadow 表按普通表归档（其内容随更新进入差分），虚拟表自身的检索语义不在证据层。
13. **测试缝**：`capture_sqlite::set_pre_snapshot_hook`（preflight 之后、快照之前插入测试
    动作，复现"采集期间才被加锁"）与 `set_pre_observation_hook`（preflight 之后、全量观测
    之前插入测试动作，复现"观测中途 schema 漂移"）以及 `set_lock_timeout_for_tests` 是公开
    测试缝，生产路径不安装、不调用；超时默认 250 ms。
14. **同域信任边界**：运行时根内任何有写权限的本地写者都能伪造 manifest/对象；归档不含
    签名或密钥，`rowDigest` 只能发现非蓄意的损坏。`state.seq+1` 的清单门槛挡住"仅伪造观测
    状态"的路径（用例 29），但在同域写者同时伪造清单时不再是安全边界；这一点与计划一致
    （本阶段不做防篡改）。

## 6. 结论

P3 的 5 项目标均有实现与用例证据：基线快照 + 原始行级差分、快照触发策略、
观察水位与盲区披露、任一采集观测点重放、未知表/列回查、OpenCode 旧 JSON 与
SQLite V1/V2（含并存迁移与重复项事实）、活跃写入/part 增长/WAL、锁超时与
不可用的显式健康错误、只读源、单写者锁、fail-closed 回读、CLI JSON。
`cargo test` 全绿（320 passed / 1 ignored / exit 0），P1/P2 既有断言未改动且保持通过。
真实数据：**否**。
