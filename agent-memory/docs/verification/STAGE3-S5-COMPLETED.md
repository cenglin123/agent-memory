# STAGE3-S5 完成与独立验收

## 范围

在 S1–S3 之上交付目标路由层：schema v3（`reuse_history` + `validation_history`，v2→v3 幂等迁移）、`learning reuse` / `validate` 事件记录、`learning gate-target` 六目标需求矩阵评估、`learning export` 审稿包导出。零晋升：store 无晋升命令，status 恒为 candidate；`ready_for_approval` 只意味着「可提交人工批准」。

实现提交链：

1. `1259738`：S5 全部实现（routing.rs 六目标矩阵 + events.rs 事件记录 + store v3 + 4 个 CLI 子命令 + 12 条集成测试 + 3 条模块单测）。
2. `71c5db0` / `07d549a`：分发物重建与谱系刷新。

全部开发与测试使用隔离 worktree、合成 fixture 和临时根；未读取真实会话、Archive、知识库、`~/.memory`、cc-switch 或生产 SQLite，未执行生产安装。

## 核心交付

- schema v3：v2→v3 探针前置 + 写锁重读 + 幂等迁移；v1 走链式 v1→v2→v3；未来版本/外来/残缺 fail-closed。`reuse_history` 带 UNIQUE(candidate_id, task_id)。
- `learning reuse`：同任务去重（`REUSE_TASK_CONFLICT` exit 2）、单事务原子、历史只追加、record.reuse[] 同步刷新、payload 哈希随事件刷新。
- `learning validate`：多轮保留、非法 verdict 拒绝、record.validation[] 同步。
- `learning gate-target` 六目标矩阵：
  - **feedback**：owner_backed（S2 判据复用）+ dedup 已记录 + 反证列出
  - **user_profile**：全部证据 owner+interactive + dedup + ≥1 条 user_statement/correction
  - **workflow**：reuse ≥2 互异 task、全 pass、applicability 非空、dedup
  - **skill**：workflow 全部 + validation ≥1 全 pass + 两个 manual 项（cc-switch 单源、宿主副本不写）
  - **note**：证据非空 + dedup
  - **rule**：恒 `proposal_only`（`rule_requires_governance`），明示「常驻规则必须走治理流程与用户确认，绝不直接部署」
- `skipped` 阻断（未提供源门控时 gate 依赖需求不可评估）而 `manual` 不阻断——与「Skill 的 cc-switch 来源检查是流程门」的合同语义一致。
- `learning export`：审稿包 JSON 固定 `decision_required: "human approval"`；rule 额外固定 `promotion_path: "none — proposal only"`；`--out` 拒绝相对/`~`。
- ingest 输入端对 reuse/validation 必须为空的 S1 校验保持不变（事件只经新 CLI 记录）。

## 独立验收轨迹

### verification（源码，`1259738`）：ACCEPT

fresh GLM 独立全量复跑 3 次（802 passed / 0 failed / 3 ignored），20+ 次 CLI 合成探针（init/ingest/reuse/validate/dedup/gate-target/export 正反矩阵），零晋升 grep 全树无 status 翻转 SQL，读路径审计 clean。零阻断 findings；3 条 info 级观察（畸形事件条目统计盲区不可达、validate --notes 不落库与合同一致、并发迁移竞态仅顺序覆盖）。

### verification（分发物，`07d549a`）：ACCEPT

fresh GLM 独立核验：

- `1259738..07d549a` tracked 变更仅 build-manifest.json，源码树零漂移。
- manifest 二进制 `7b2f901d56d0484ee79ff5fc584b9e5156fda39ac80486f04e713310d7d449c8`（11,537,408 bytes）逐字核验；树哈希 `c7737348…`（87 个构建输入）；`source.commit` 为 HEAD 祖先且 `treeDirty=false`。
- `build -Verify` 现场重建双构建同哈希；dist `--help` 含 gate-target / export / reuse / validate。
- 62 离线验收 **43/43**（真实根指纹项 PASS，9770 文件指纹前后一致）；63 谱系 19/19。
- 全量 `cargo test --locked` 802 passed / 0 failed / 3 ignored。

## 验证汇总

| 检查 | 结果 |
|---|---|
| learning_routing | 12/12 |
| 全量 Rust | 802 passed / 0 failed / 3 ignored（源码轮 3 次 + 终验 1 次） |
| `build -Verify` | 双构建 SHA-256 一致（`7b2f901d…49c8`） |
| 62 离线验收 | 43/43（真实根指纹 PASS） |
| 63 谱系验收 | 19/19 |
| `git diff --check` | 通过 |
| OCSR 读取边界 | 两轮 fresh verifier 均 clean |

## 边界与遗留

- `ready_for_approval` 不是晋升：审批动作在库外（KB plan 记录），与 S4 实践一致。
- Skill 的 cc-switch 单源检查与宿主副本不写入是流程门（manual），机械部分只做 validation 计数与明示。
- 并发 v2→v3 迁移竞态仅顺序覆盖（沿 S3 既有模式）；validate `--notes` 不落库（合同 DDL 无该列，README 已文档化）。
- 未做生产安装、真实目标路由试采（S6 范围）或长期记忆写入；默认生产 90 的旧安装哈希与真实 Archive 非空两项失败保留。
