# STAGE3-S3 完成与独立验收

## 范围

在 S1 存储与 S2 gate 之上补齐第三阶段操作链的中间环节：查重处置记录（`learning dedup` + `dedup_history`）、learning.db schema v1→v2 幂等迁移、supersession 顺序规则（revision+1 的 `updated_at` 不得早于当前值）、gate 作为 `learning ingest` 的可选前置（`--archive/--domain`，`GATE_BLOCKED` 零写入拒绝）。既有候选连接以手册 + 字节不变测试交付机制；真实 `~/.memory` 候选连接与真实 KB ask.py 查重属 S4 授权范围。

实现提交链：

1. `2689efb`：S3 全部实现、9 条集成测试 + 2 条模块单测、文档（含 orchestrator 补写的 CHANGELOG——executor 在收尾文档编辑中被看门狗终止，代码与测试已完整落盘）。
2. `fb5cee6` / `c9c6881`：分发物重建与谱系刷新。

全部开发与测试使用隔离 worktree、合成 fixture 和临时根；未读取真实会话、Archive、知识库、`~/.memory` 或生产 SQLite，未执行生产安装。

## 核心交付

- `learning dedup`：report 浅层校验（顶层对象 + 非空 `query` + `results` 数组，元素原样存入 matches）、四 disposition、`--reason` 必填、单事务原子、历史只追加、`contradicts` 只记录不写反证不改状态、payload 哈希随 dedup 刷新。
- schema v2：v1 先探针后迁移（外来/残缺库拒绝且零写入）、写锁下重读版本号保证重入、未来版本/损坏仍 fail-closed。
- same-revision 比较集排除 dedup（合并保留 stored dedup，追加不重置 pending）；ingest 输入仍要求 pending；同 key 原始 payload 重放语义不变。
- supersession：revision+1 且 `updated_at` 更早 → `CANDIDATE_REVISION_CONFLICT`（out-of-order）；相等/更晚接受；stored 值解析失败为 integrity。
- gate 前置 ingest：成对参数、`GATE_BLOCKED`（exit 2）零写入、无参数时 S1 行为不变；gate 与 ingest 非原子已如实记录。
- 既有候选连接：零新增外部清单读取路径；手册固化 + 合成清单文件全流程字节不变测试。

## 独立验收轨迹

### verification-001：ACCEPT（源码，`2689efb`）

fresh GLM 独立全量复跑 3 次（787 passed / 0 failed / 3 ignored），18/18 合成探针（含 suite 盲区：supersession integrity 路径、v1 库上 gated blocked ingest 零写入且不触发迁移、dedup CLI 合法/非法矩阵），读路径审计 clean，零阻断 findings。4 项非阻断观察：`same_revision_fields` 的 dedup 排除在调用点等化实现（README 已披露，建议 S4 前移入方法）、bin 文件 rustfmt 偏差为基线既有、3 个探针证实的盲区建议 S4 收编为常规用例、init 对真 v3 库重试约 15 秒后才拒绝（fail-closed 不受影响）。

### verification-002：ACCEPT（分发物，`c9c6881`）

fresh GLM（接续同一实例完成收尾轮）独立核验：

- `2689efb..c9c6881` tracked 变更仅 build-manifest.json，agent-memory 源码树零漂移。
- manifest 二进制 `d3c91e71c4d8a9ca5bcfc3f25baee828995a444180f1552c97ba6102d9142287`（11,298,304 bytes）逐字核验；树哈希 `2d523457…adde`（86 个构建输入 = S2 的 85 + dedup.rs）；`source.commit` 为 HEAD 祖先且 `treeDirty=false`。
- `build -Verify` 现场重建双构建同哈希；dist `--help` 含 learning dedup 与 learning gate。
- 62 离线验收前台单轮 **43/43**（真实根指纹项 PASS，9736 文件指纹前后一致；首轮后台运行因会话中断 0 字节输出，不构成 FAIL 判定，两轮如实记录）。
- 全量 `cargo test --locked` 787 passed / 0 failed / 3 ignored。
- 读路径审计 clean；过程说明（首轮后台中断 + 收尾轮前台完整运行）随报告如实保留。

## 验证汇总

| 检查 | 结果 |
|---|---|
| learning_dedup / gate / store | 9 / 19 / 29 全过 |
| 全量 Rust | 787 passed / 0 failed / 3 ignored（源码轮 3 次 + 终验 1 次独立复现） |
| `build -Verify` | 双构建 SHA-256 一致（`d3c91e71…2287`） |
| 62 离线验收 | 43/43（前台单轮，指纹项 PASS） |
| 63 谱系验收 | 19/19（orchestrator 运行） |
| `git diff --check` | 通过 |
| OCSR 读取边界 | 两轮 fresh verifier 均 clean |

## 边界与遗留

- gate 与 ingest 非原子（两次独立读取）；ingestion 不持久化 gate 报告。
- same-revision 证据追加会把 `updated_at` 刷新为提交值（理论上可早于原值）；supersession 下限由 revision+1 检查承担——已知观察项。
- init 对真 v3 库会重试约 15 秒后才拒绝（fail-closed 语义不变，仅延迟）；可后续仅对 Lock 重试。
- v1 库 + gated blocked ingest 不触发迁移、stored `updated_at` 损坏 → integrity 等 3 条探针证实路径建议 S4 收编为常规测试。
- 未做生产安装、真实 `~/.memory` 候选连接、真实 KB 查重、真实数据试采或长期记忆写入；默认生产 90 的旧安装哈希与真实 Archive 非空两项失败保留。
