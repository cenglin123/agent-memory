# Archive P5 收尾修复记录

范围：P5 覆盖报告/派生重放的收尾小修，**不推进 P6**，不使用真实会话数据。

真实数据：**否**。全部测试只使用 `tempfile` 临时根与合成源。

Git：**验收时已提交**（本次 orchestrator 提交；executor 不自行执行 `git commit`）。
P5 实现基线为 `de2021d`，本收尾修复随本轮验收提交。

## 1. 修改内容

| 文件 | 动作 | 修改 |
|---|---|---|
| `agent-memory/src/report.rs` | 修改 | `derived.status=rebuilding` 时置 `coverage.ok=false`，不再作为正常状态；异常文案保持“derived reads fail closed until replay completes” |
| `agent-memory/tests/p5_report_replay_scrub.rs` | 修改 | `derived_read_is_fail_closed_on_missing_access_and_rebuilding` 改用当前 parser 身份，并新增断言：rebuilding 时 `coverage.ok=false`、`derived.status=rebuilding`、存在 mid-rebuild 异常、CLI `coverage` 退出 8；新增 `cli_replay_partial_failure_exits_nine` |
| `agent-memory/docs/verification/P5-ARCHIVE-COMPLETED.md` | 修改 | Git 状态改为验收时已提交（本次 orchestrator）；测试计数更新为 364，P5 用例 16 项，补记新增 CLI 退出码 9 用例与 rebuilding 覆盖断言 |
| `agent-memory/README.md` | 修改 | Build & test 计数同步为 364 |
| `CURRENT.md` | 修改 | 最近完工计数更新为 364 passed / 1 ignored；Git 状态改为验收时已提交（本轮 orchestrator，实现基线 `de2021d`）；P4 历史条目的提交状态同步为 `c6fc7c1` |

未修改：`src/coverage.rs`、`src/replay.rs`、`src/scrub.rs`、`src/archive/**`、
`src/capture*`、`src/providers/**`、`src/attachment.rs`、`src/recall/**`、
安装脚本与分发物、Obsidian 库。

## 2. 行为修正

### 2.1 rebuilding 是异常态

`report::evaluate` 读取当前 parser/model 的检查点快照；当
`snapshot.status == "rebuilding"` 时：

- 追加显式异常
  `derived layer is mid-rebuild; derived reads fail closed until replay completes`；
- 置 `CoverageReport.ok = false`；
- `DerivedReport.status = "rebuilding"` 继续如实上报；
- CLI `coverage` 因 `ok=false` 使用既有异常/对账退出码 **8**，不再返回 0。

派生回读本身仍由 `read_derived` 在 generation `rebuilding` 时 fail-closed
（`AccessDenied`），两层语义一致。

### 2.2 replay partial/failed 的 CLI 契约

CLI `replay` 在存在解析失败源时：

- stdout 仍输出完整 `ReplayReport`；
- `ok=false`、`failedSources>=1`、失败源 `status=parse_failed`；
- 退出码稳定为 **9**。

新增反向用例使用可采集但不可解析的整文件 OpenCode 源，验证上述字段与退出码；
同一状态经 CLI `coverage` 验证为退出 **8**、`categories.parseFailed >= 1`。

## 3. 测试

```sh
cd agent-memory
cargo test
```

实测（2026-10-26，worktree `C:\wt\archive-p5-20261026`）：**exit 0，
364 passed / 0 failed / 1 ignored**。

| 目标 | 数量 |
|---|---|
| lib（含 P5 单元 2 项） | 165 |
| `boundary_no_default_roots` | 5 |
| `p0_formats` | 4 |
| `p1_archive` | 8 |
| `p2_jsonl_incremental` | 17 |
| `p3_sqlite_consistency` | 33 |
| `p4_attachments` | 24 |
| **`p5_report_replay_scrub`** | **16** |
| `recall_boundary` / `recall_cli_mcp` / `recall_contract` | 6 / 7 / 9 |
| `recall_kb_device` / `recall_opencode` / `recall_read` | 25 / 22(+1 ignored) / 18 |
| doctests | 5 |

新增/强化的 P5 断言：

1. `derived_read_is_fail_closed_on_missing_access_and_rebuilding`
   - rebuilding 时 `coverage.ok == false`；
   - `coverage.derived.status == "rebuilding"`；
   - 报告含 `mid-rebuild` 异常；
   - CLI `coverage` 退出 8，stdout 中 `ok=false`、`derived.status=rebuilding`。
2. `cli_replay_partial_failure_exits_nine`
   - CLI `replay` 退出 9；
   - stdout `ok=false`、`failedSources=1`、`sources[0].status=parse_failed`；
   - 同状态 CLI `coverage` 退出 8、`categories.parseFailed >= 1`。

原有 363 项测试全部保留并通过，未删除或弱化任何断言。

## 4. 边界与限制

- 未推进 P6：无离线安装、预编译分发物、审计接入或真实数据试采。
- 真实数据：**否**。
- 未执行 `git commit`；提交由本轮 orchestrator 完成。
