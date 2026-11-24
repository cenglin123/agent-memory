# STAGE3-S1 完成与独立验收

## 范围

实现显式 learning root 中的候选 v1 schema、稳定 candidate/ingest 身份、revision/status 历史、SQLite 单写者原子 ingest，以及 `learning init|ingest|show|list` CLI。学习库不是长期记忆、不是 recall 来源，也没有晋升或生产写入通道。

实现提交链：

1. `68de1a7`：候选审稿存储的初版实现与文档。
2. `3433a2d`：verification-001 两项 P1 修复。
3. `ffef2ad`：S1 分发物重建。
4. `f575b22`：分发物谱系刷新。

全部开发与测试使用隔离 worktree、合成 fixture 和临时 learning root；未读取真实会话、Archive 内容、知识库或生产 SQLite，未执行生产安装或真实数据试采。

## 核心交付

- v1 严格 JSON schema：逐层拒绝未知字段、非法枚举、错误时间/hash、空 evidence、非空 promotion/reuse/validation/ingestions；S1 仅接受 `candidate`。
- 稳定 `candidate_id` / `ingest_key` / `payload_sha256`；同 key 同内容幂等重放，同 key 异内容冲突，UNIQUE 并发回退遵循同一语义。
- 新候选必须从 revision 1 开始；revision+1 保留历史；同 revision 只追加去重证据。
- 候选保留首次接收的来源，每批次实际 `extractor/source_checkpoint` 均可在 ingestions 回读。
- `BEGIN IMMEDIATE` 单写者事务、失败整体回滚、WAL 与 schema/user_version fail-closed。
- 并发 init 幂等返回合法 DB，失败路径不删除其他初始化者已提交的 schema。
- 无默认真实 root、Archive、recall、FTS/向量索引或长期记忆旁路。

## 独立验收轨迹

### verification-001：REVISE

fresh GLM 发现两项 P1：

1. 新候选可非法诞生于 revision 2，缺少 revision 1 守卫。
2. 并发 init 的 exists/read_dir TOCTOU 在负载下导致合法初始化者间歇性硬失败。

主会话修复后，learning_store 连续 6 轮 29/29 通过，全量回归 748 passed / 3 ignored / 0 failed。

### verification-002：ACCEPT

新的 fresh GLM 独立复测：

- revision=2 新候选被 `CANDIDATE_REVISION_CONFLICT` 拒绝，四表无半写状态；随后 revision=1 成功。
- learning_store 首跑与测试二进制连续 6 轮均 29/29。
- 跨进程 5 轮 × 8 个并发 init 全部成功，合法 schema 保留。
- 全量 `cargo test --locked` 748 passed / 3 ignored / 0 failed。
- 严格 schema、幂等/冲突、来源回读、revision 历史、回滚、显式 root 与无晋升旁路保持通过。
- OCSR 读路径审计 clean。

唯一非阻断项为 `CURRENT.md` 的待提交状态需要在收尾时重置。

### verification-003：ACCEPT

另一位 fresh distribution verifier 独立核验最终 `f575b22`：

- 二进制 SHA-256：`96d0d91285b8e4bbf3362097c34db9c97c6eb0355778187e9dc3b25b7ac781ea`；11,325,440 bytes，与 manifest 一致。
- 源码树 SHA-256：`83cb04554a98b3dfe5011f95820d4215800319531622a3d2c6bbffc2e911e994`；83 个构建输入文件，独立重算一致。
- manifest `source.commit` 是 HEAD 祖先，`treeDirty=false`；`3433a2d` 与最终分发的构建输入逐字节一致。
- `build -Verify` 双构建哈希一致，静态 CRT、无 GUI 依赖，learning help 在位。
- 62 离线验收在静默窗口 43/43，真实根前后指纹一致。
- 62 首轮曾因生产索引计划任务在验收窗口内并发写入真实根而为 42/43；复跑无并发时通过。该环境事件不构成分发物缺陷，保留在验收记录中。
- OCSR 读路径审计 clean；未运行默认生产 90，未安装或读取真实内容。

## 验证汇总

| 检查 | 结果 |
|---|---|
| learning_store | 29/29；主会话连续 6 轮，fresh verifier 另连续 6 轮通过 |
| 全量 Rust | 748 passed / 3 ignored / 0 failed |
| 并发 init | 线程与跨进程重复测试通过；有效 schema 不被删除 |
| `build -Verify` | 双构建 SHA-256 一致 |
| 62 离线验收 | 静默窗口 43/43 |
| 63 谱系验收 | 19/19 |
| 文档快速检查 | 6/6 |
| `git diff --check` | 通过 |
| OCSR 读取边界 | 各 fresh verifier 均 clean |

## 边界与遗留

- 默认 `90 -AgentMemoryOnly` 的分发物与源码谱系为 PASS，但生产检查两项 FAIL：本机已安装二进制仍是旧哈希，真实 Archive 已有数据。S1 不做生产安装，不用修改或清空生产状态消除失败。
- 62 的真实根指纹守卫对并发采集天然敏感；应在静默窗口运行。首轮 42/43 与静默窗口 43/43 均如实保留。
- pinned Rust 1.99.0 未安装 rustfmt 组件；新增 learning 模块与测试已用本机可用 rustfmt 校验，不对基线未格式化文件做无关改写。
- 未做生产安装、真实数据试采、S2 来源裁决或任何长期记忆写入。
