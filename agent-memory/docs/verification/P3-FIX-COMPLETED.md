# Archive P3 复核修复记录（F1 / F2 + 测试与文档）

范围：针对 fresh verification（`b52499d`，报告
`<user-home>/AppData/Local/Temp/opencode/archive-p3-verification.md`）的 F1、F2 及
F3/F4/F6 相关项做修复。仅修改 `agent-memory/**` 与 `CURRENT.md`；未推进 P4。
真实数据：**否**（全部用例仍为合成 fixture + 临时根）。Git：本修复由 orchestrator 在验收后提交。

## 1. 修复项

### F1（Medium）`observation_state_missing` 未降级，`report.ok` 与 `coverage.ok` 不一致

原状：观测状态损坏 / `captureSeq` 越界时，计划落到 `observation_state_missing`；该触发
既不算 degraded，也允许去重，于是 `report.ok=true`、CLI 退出 0，而 `coverage.ok=false`。

修复（`src/capture_sqlite.rs`）：

- 把触发拆成两个独立判定：
  - `degraded_trigger`（健康）：`observation_state_missing` |
    `baseline_unavailable` | `unclassifiable_state`；
  - `dedupe_blocked`（证据）：`baseline_unavailable` | `unclassifiable_state`（必须重新入档，
    不能被"内容相同"掩盖）。
- 三个 `degraded_trigger` 统一走：提交/复用可用基线 → `status=degraded`、`ok=false`、
  `errors += 1`、写 `sqlite_snapshot:<trigger>` anomaly；去重路径同样标记 degraded
  （`finish_unchanged` 的 `degraded_reason`）。
- 因此 `report.ok == coverage.ok` 在"无法安全增量"的全部 P3 场景中成立，CLI `capture`
  以 `EX_CAPTURE=7` 退出并带 `report.error`。

### F2（Low）`state.seq+1` 被无条件信任

原状：只要观测状态写成 `prev.seq+1` 且重算 `rowDigest`，就会被当作差分基线，可能写出
`before` 为从未观测内容的差分行。

修复：`capture_sqlite_source` 的观测状态校验改为：
`seq == prev.seq + 1` 时**必须**存在 `committed_unit_at(archive, source, seq)`（真实崩溃窗口），
否则一律按 `observation_state_unreadable` 拒绝并重建状态；`seq` 不在 `[prev.seq, prev.seq+1]`
同样拒绝。

### F3 两条降级路径缺测试

- 新增 `schema_drift_during_observation_is_degraded`：用新的测试缝
  `capture_sqlite::set_pre_observation_hook` 在 preflight 之后、全量观测之前执行 DDL，
  断言不写差分、`degraded`、`schema_changed_during_observation`、`report.ok == coverage.ok`。
- 新增 `unclassifiable_state_is_degraded_and_repaired`：清空 `state/sources.json` 的
  `known_columns`，断言新基线、`degraded`、状态修复、下一轮健康。
- 新增 `orphaned_corrupt_observation_state_is_degraded_without_state`：观测状态损坏且
  `sources.json` 丢失（无可用先验状态）时，不得把历史清单误判为崩溃接管，必须以
  `unclassifiable_state` 重建基线并报 degraded。
- 修正完成记录中"用例 6、25 覆盖 schema 漂移"的错误引用。

### F4 文档与提交状态

- `P3-ARCHIVE-COMPLETED.md` / `CURRENT.md` 的 Git 状态改为“已提交”：P3 基础实现记为
  `b52499d`，F1/F2 修复由 orchestrator 提交；P2 记录同步为“验收时已提交为 `b2717d7`”。

### F6 补充反向测试

- 新增 `forged_next_sequence_state_is_rejected_without_a_committed_unit`（F2 的反向用例），
  同时验证真实崩溃窗口（`+1` 且清单存在）仍被接管为 `recovered_after_crash`。
- F6 建议的 `has_object`/`read_object_bytes` 非法 sha 单元测试仍待后续小修（见 §4）。

### 整改复审追加（N1）：接管被拒时不得残留恢复异常

第一轮整改后复审发现：`adopt_committed_unit` 在判定角色**之前**就写入
`sqlite:recovered_after_crash`；若差分接管因"崩溃后源库又有写入"而无法校验（返回
`Ok(false)`），控制流回到正常差分路径，异常已留在 `report.anomalies` 里，而
`report.ok=true` —— 再次出现 `ok` 与 `coverage.ok` 不一致。

修复：`sqlite:recovered_after_crash` 只在实际采纳成功的分支内写入（基线接管在分支入口、
差分接管在 `live == derived` 校验通过之后）。接管被拒时按"新序号 + 正常差分"处理，
`report.ok=true` 且 `coverage.ok=true`。用例 33 覆盖该窗口（崩溃 → 再写 → 正常 delta、
seq 2 与 seq 3 均可重放、两侧 `ok` 一致）。

## 2. 行为变化（相对 `b52499d`）

| 场景 | 旧行为 | 新行为 |
|---|---|---|
| 观测状态缺失/损坏，快照内容与基线相同 | `skipped_unchanged`、`ok=true`、coverage false | `degraded`、`ok=false`、coverage false；内容相同仍去重（不产生第二基线），状态被重写 |
| 观测状态缺失/损坏，内容已变化 | 新基线、`ok=true`、coverage false | 新基线、`degraded`、`ok=false`、coverage false |
| 观测状态损坏且状态文件丢失（无先验） | 被当作 `initial` 干净成功（anomaly 但 `ok=true`） | `unclassifiable_state` 重建基线、`degraded`、`ok=false`，不再误判为崩溃接管 |
| 观测状态 `seq+1` 且无对应清单 | 被接受为差分基线（可写出伪造 `before`） | 拒绝（`observation_state_unreadable`），按缺失状态处理，不写差分 |
| 观测状态 `seq+1` 且有对应清单（真实崩溃） | 接管，`ok=true`、coverage false | 仍接管（不写第二份），但该轮报 `degraded` / `ok=false`，下一轮恢复 |
| 崩溃后源库又发生写入（接管无法校验） | 接管被拒，但 `recovered_after_crash` 异常已写入 → `ok=true`/coverage false | 不写恢复异常，按新序号写正常差分（`ok=true` 且两侧一致）；用例 33 |
| 首次采集在清单提交后崩溃（无状态） | 重跑按 `initial` 再写一个基线 | 行为不变（不接管）；代价是多一份全量基线，已记入限制 |
| schema 漂移于 preflight 与观测之间 | `degraded`、`ok=false`（无测试） | 行为不变，新用例覆盖 |
| `unclassifiable_state` | `degraded`、`ok=false`（无测试） | 行为不变，新用例覆盖 |

由此对 SQLite 来源形成统一口径：**`report.ok == coverage.ok`**（成功采集/未变化/源删除为
true；锁/不可用为 failed，无法安全增量为 degraded，两者均为 false）。为闭合最后一处
“源删除 + 旧失败标记”的分歧，源删除时把待处理的 `lastUnavailable` 按历史处理（仍计入
`errorsTotal`，见用例 22 第二阶段）。CLI 契约不变：`capture` 在 `!report.ok` 时退出 7。

## 3. 测试

```sh
cd agent-memory
cargo test
```

实测（2026-10-25，worktree `C:\wt\archive-p3-20261025`）：**exit 0，
320 passed / 0 failed / 1 ignored**。

| 目标 | 数量 |
|---|---|
| lib（单元） | 161 |
| `boundary_no_default_roots` | 5 |
| `p0_formats` | 4 |
| `p1_archive` | 8（未改） |
| `p2_jsonl_incremental` | 17（未改） |
| **`p3_sqlite_consistency`** | **33（28 → +5，2/24/26 断言按新语义更新）** |
| `recall_boundary` / `recall_cli_mcp` / `recall_contract` | 6 / 7 / 9 |
| `recall_kb_device` / `recall_opencode` / `recall_read` | 25 / 22(+1 ignored) / 18 |
| doctests（compile-fail 反向） | 5 |

新增/更新的 P3 用例：

- 29 `forged_next_sequence_state_is_rejected_without_a_committed_unit`（F2 反向；含真实崩溃窗口正例）
- 30 `schema_drift_during_observation_is_degraded`（F3）
- 31 `unclassifiable_state_is_degraded_and_repaired`（F3）
- 32 `orphaned_corrupt_observation_state_is_degraded_without_state`（无先验状态时不得误判崩溃接管）
- 33 `crash_then_new_writes_falls_back_to_a_clean_delta`（接管被拒时不写恢复异常、两侧 `ok` 一致、seq 2/3 均可重放）
- 2 `repeated_capture_and_repeated_snapshot_are_deduped`：状态丢失后的去重现在是 degraded，
  断言 `report.ok == coverage.ok`，之后恢复 delta
- 13 `source_deletion_keeps_the_archived_evidence_readable`：新增 `report.ok == coverage.ok`
- 22 `health_flag_is_cleared_by_a_successful_unchanged_observation`：新增“失败后源被删除”
  第二阶段，断言 `source_missing` 两侧均为 `ok=true` 且 `lastUnavailable` 清空
- 24 `corrupted_observation_state_is_reported_and_healed`：新增 degraded 断言与 CLI 退出 7 断言
- 26 `observation_state_with_a_foreign_capture_seq_is_rejected`：新增 degraded /
  `report.ok == coverage.ok` 断言

原有断言未删除、未弱化；P1/P2/recall 测试文件零改动。

## 4. 剩余限制（本次未处理）

1. **F5（可见性窗口）**：基线对象丢失在**下一次采集前**不会反映到 coverage（只有
   `reconcile().missing_objects` 可见）。对象级 scrub/对账属后续阶段（P5），本次未改。
2. **F6（部分）**：`Archive::has_object` / `read_object_bytes` 的非法 sha fail-closed 逻辑
   仍无仓库内单元测试（实现已在 `b52499d`，本次未加用例）。
3. **F7（极窄崩溃窗口）**：manifest 提交后、`state/records.json` 写入前崩溃时，
   `adopt_committed_unit` 只恢复状态与观测文件，不重建差分行索引；`replay_sqlite` 仍可从
   对象解压重放（证据不丢），但 `read_record(sha)` 对这批差分行不可用。
4. **同域信任边界**：运行时根内可写者仍可伪造 manifest/对象；本次的 `+1` 清单门槛只挡
   "仅伪造观测状态"的路径，不等同于防篡改（无签名/密钥，计划内如此）。
5. **观测盲区**不可消除：两次观测间完全改回的行与未变化不可区分；报告以
   `observationIntervalMs` + `blindWindow` 披露。
6. **首次采集崩溃的代价**：没有任何状态时（首次采集在清单提交后崩溃），重跑不接管，会
   再写一个基线代际（多一份全量快照，证据一致、重放正确）。这是"无状态不能证明崩溃窗口"
   的必然取舍，已记入 `P3-ARCHIVE-COMPLETED.md` §5.5。
7. **P4–P6 未做**：附件/ContentRef、provider 级覆盖对账与派生重放、完整 CLI 回读/覆盖契约、
   离线分发与真实数据试采均未实现。

## 5. 结论

F1、F2 已修复并有直接反向用例；F3 的两条降级路径、同源边角（无先验状态的损坏观测状态、
崩溃后继续写入）与 F4 的提交状态已补齐；SQLite 来源在成功/未变化/源删除时 `ok=true`，在
锁/不可用（failed）与无法安全增量（degraded）时 `ok=false`，即
**`report.ok == coverage.ok`**，CLI 相应退出 7。
`cargo test` 320 passed / 1 ignored / exit 0，P1/P2 既有断言未改动。P4 未推进。
