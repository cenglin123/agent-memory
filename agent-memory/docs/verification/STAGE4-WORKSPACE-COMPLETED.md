# STAGE4-WORKSPACE-COMPLETED — 第四阶段 workspace 任务层交付与验证

- 日期：2026-11-04
- canonical 谱系对照（worktree 提交经 rebase 后入 master，内容逐 commit 等值）：73f3eadc→efdeffb、55d985f→32b763f、1cff776→caba7f4、39b4b51→4e2ff5f、ee24ac4（本报告）→f1f3097；KB 计划 checkpoint 记录中的 implementation_commit 为 worktree 原哈希，canonical 对应以上映射
- 计划：KB `docs/plans/active/20261104-第四阶段workspace任务层.md`（rev2，canonical commit `e55b75aa`，含 implementation-001..004 与 verification-001/002 全部 checkpoint 记录）
- 实施提交链（隔离套件 worktree `C:\wt\20261104-203320-1a59ea8b`，基线 `aebb53e`）：
  - implementation-001 `73f3eadc`：schema v4——`event_clock` 全局事件序（三事件表 `event_seq` 同事务取号、v3 数据按 `(recorded_at, 表名秩, seq)` 稳定回填）、`task_events` 新表（event_kind CHECK 封闭）、`reuse_history` 幂等键重建（`UNIQUE(task_id, actor_id, client_seq)`，删表级 UNIQUE，旧行 `migrated:v3` 归因）、`validation_history` 增 `event_seq`、open 链 1→2→3→4 逐级 probe fail-closed、`learning migrate --to 3` 降级工具（事务外 VACUUM INTO 固定名备份 + sidecar、知识行重复预检、级联删除含 `orphan_task_refs`、显式 seq 回插、SAVEPOINT 探针断言衔接）
  - implementation-002 `55d985f`：`learning task create|claim|abandon`（create load-first 零副作用重复语义、claim 事件即首任负责者、`--reason` 必填）、`learning reuse` 的 `--actor` 必填 + `--client-seq`/`--idempotent` 幂等三态、ingest task 守卫（`TASK_REQUIRES_TASK_CREATE`）、`NOT_A_TASK`/`TASK_MISMATCH` 守卫、payload task 分支（evidence 空、`proposed_target` 恒 `task`、同 claim 内容寻址收敛同 candidate_id）、投影核心 `project_task_state`（五步规则、跨表 `event_seq` 定序、TTL 仅 active、done 只由 fail 退回）
  - implementation-003 `1cff776`：`learning workspace` 只读投影列表（`--state` 五值映射四态、`last_event.event_seq` 倒序、零写路径）、`learning task show` 三表统一视图全事件、create 双路径 `state` 收口、`learning list` 默认隐藏 task（`--include-tasks`）、`gate`/`dedup`/`gate-target`/`export` 对 task 一律 `TASK_NOT_ROUTEABLE`、A6 只读断言四件套
  - implementation-004 `39b4b51`：README stage 4 全节（错误码总表、R14「约定非认证」声明）、INSTALL/UPGRADE v4 迁移与降级说明；18 项实现-文档一致性核对
- 执行者：deepseek/deepseek-flash（ocsr 派发，合成 fixture，真实 root 零接触）；主会话每棒独立复跑全量测试并验收后提交；checkpoint 记录逐条入 KB 计划

## 测试与验证

- 全量 `cargo test --no-fail-fast`：**842 passed / 0 failed / 3 ignored**（39 个测试二进制；ignored 为既有 `p3_sqlite_scale`、`recall_fulltext`、`recall_opencode`）。每棒实施后由主会话独立复跑确认，verification-001 由 GLM 以独立临时 target 第三次复跑并 awk 逐行累加复核。
- 新增测试：`learning_migration.rs` 11（迁移等值/回填/clock 衔接/fail-closed/降级全链路/往返）、`learning_task.rs` 20（A4/A5/A7/A8/A10/A11 + 投影 16 例 + 跨表交错四例 + TTL 边界）、`learning_workspace.rs` 8（A6 四件套/A12/A13/隔离/create state）。
- 既有 learning 套件语义逐字保持：learning_store 29、learning_dedup 9、learning_routing 12、learning_gate 19（仅版本断言机械调整与 `--actor` 实参适配）。
- rustfmt（1.95）`--check` 通过；`git diff --check` 干净；check-docs 6 项 PASS；95-test-docs-governance 67/67。

## Fresh verification（跨模型族，GLM / zhipuai）

- **verification-001：pass（ACCEPT）**——A1–A13 逐项验收（实现行号 + 测试断言 + 三批端到端 CLI 实测）、文档一致性 12 项抽查、越界 diff 为空、既有测试适配无语义转向。P3 备注 2 条（dedup 守卫在 report 形状校验之后；`--state` 空白值宽容）均不阻断，裁定维持现状。报告：`C:\wt\stage4-s1-dispatch\verify-001.md`
- **verification-002：pass（ACCEPT）**——真 v3 二进制回滚验证：v3 旧版（父提交 `aebb53e` 构建）对 v4 降级后的库完成 list/show/reuse/validate 全操作（五操作全 exit 0、sqlite 断言全过：`user_version=3`、无 v4 表、新写行在位）；往返复核知识数据无损、actor 归因按 R8 声明转 `migrated:v3`；v3 原生根对照健康。A9 完全闭合。报告：`C:\wt\stage4-s1-dispatch\verify-002.md`
- 隔离声明：全部验证使用合成 fixture 与临时目录；真实 learning root、真实会话数据、OneDrive 路径零接触。

## 已声明偏差与设计裁定（实施期收敛）

- 幂等判定先于 S5 知识候选唯一性检查（同键同内容重试安全优先；未钉 seq 的重复仍 `REUSE_TASK_CONFLICT`）
- task 事件不重写 payload（`record_json`/`payload_sha256` 恒定——「投影读表」的写侧对齐）
- `PRAGMA total_changes` 在本构建被 OMIT_DEPRECATED 省略（主会话独立复核属实），A6 同连接写入计数改用 `sqlite3_total_changes64` FFI 只读诊断
- create 输出字段按实现落文档（含 `claim`/`actor`）；`--now` 输出规范化为毫秒 `Z`；`TASK_NOT_ROUTEABLE` 双形态（gate/dedup exit 2，gate-target/export 报告 exit 0）
- 降级同时重建 `validation_history`（v3 精确形状要求）；迁移错误码补 `MIGRATION_BACKUP_EXISTS`/`MIGRATION_SEQUENCE_UNCONTINUOUS`

## 边界与未做

- 未做：分发物（`assets/dist`）重建与谱系刷新、62/63 复验、生产安装（`~/.agent-memory/bin`）、真实数据试采、计划转 done。
- P3-1/P3-2 维持现状（见 verification-001）。
- 学习根纯度约定自本阶段修订：root 含 `learning.db` 与显式 `migrate --to 3` 的备份对（`learning.db.bak` + `.meta.json`）。
