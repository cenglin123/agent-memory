# STAGE3-S2 完成与独立验收

## 范围

在 S1 候选存储之上交付只读来源与证据门控（gate v1）：候选的每条证据必须能回读到 committed Archive 记录，区间哈希与 committed 代际必须匹配，来源声明必须与记录中的结构化事实一致。gate 只产出报告；S1 `store.rs`/`model.rs` 语义零改动；把 gate 设为 ingest 前置属于 S3。

实现提交链：

1. `ca536e6`：gate v1 与结构化 provenance 裁决、`learning gate` CLI、16 条集成测试 + 9 条模块单测。
2. `5a4ef4c`：verification-001 盲区收口——3 条集成测试锁定 `GATE_ARCHIVE_UNREADABLE`、访问域拒绝、组合违规首失败码顺序。
3. `2c93ad2` / `0b4a128`：分发物重建与谱系刷新。
4. `bca1923`：verification-002 指出的文档计数与状态修正。

全部开发与测试使用隔离 worktree、合成 fixture 和临时根；未读取真实会话、Archive 内容、知识库或生产 SQLite，未执行生产安装。

## 核心交付

- ref 合同：唯一接受的证据 ref 为 `archive-record:<64 位小写 hex>`（committed `ManifestRecord.sha256`）；span 为整条 committed 记录，`span_sha256` 必须等于其 SHA-256。
- 检查顺序（首失败即记码，逐项独立）：`E_REF_FORMAT` → `E_REF_UNRESOLVED`（读回 + committed manifest/持有对象双证明，覆盖伪造 ref 与活源尾部）→ `E_SPAN_MISMATCH` → `E_GENERATION_MISMATCH` → `E_PROVENANCE_MISMATCH` → `E_TAINT_POLICY`。
- 结构化 provenance：事实只来自 committed manifest 的 `provider` + 记录字节。仅 `claude` 做结构化裁决（sidechain→subagent、isMeta→runtime、tool_result→受限执行证据、user 纯文本→interactive、assistant→agent_derived、其他 type→runtime/unknown）；非 claude provider 与解析失败一律 fail-closed 只接受全 `unknown` 声明。
- 召回回流阻断：`recall_taint=true` 必须 `evidence_kind=recalled_material` 且不得声明 `owner`。
- CLI：`learning gate --root --archive --domain (--candidate-id | --candidate)`；报告 `{ok, candidate_id, owner_backed, counts, items[facts]}`；`ok` 是门控裁决而非进程状态；两条输入路径共享同一校验且绝不写库。
- 只读边界：不写 learning.db、不写 archive、不取 capture 锁（集成测试以四表行数、mtime、文件树真实断言）。

## 独立验收轨迹

### verification-001：ACCEPT（源码）

fresh GLM 独立复跑全量 4 次（773 passed / 3 ignored / 0 failed），10 次合成 CLI 探针（正例、错代际、组合违规顺序、活源尾部、伪造 sha、ref 形态、usage、help），读路径审计 clean；零阻断 findings。列出 4 项非阻断测试盲区（其中 3 项由 `5a4ef4c` 收口，第 4 项「同 sha 多 manifest 持有」经分析在对象内容寻址下不可区分，未单测）。

### verification-002：ACCEPT（盲区收口 + 分发物）

fresh GLM 独立核验：

- 三条新测试为真实断言（非 mock），learning_gate 19/19；全量 776 passed / 3 ignored / 0 failed。
- `ca536e6..0b4a128` 无 `src/` 漂移；分发物二进制 SHA-256 `6b86d535e831157423a0ab7553748275fe40a649b83715e0e0aab2a377e983ae`、11,245,056 bytes 与 manifest 一致；树哈希 `a57c6ede…`（85 个构建输入 = S1 的 83 + gate.rs + provenance.rs）；`source.commit` 为 HEAD 祖先且 `treeDirty=false`；`build -Verify` 双构建同哈希。
- 62 离线验收第一轮 42/43（FAIL 为真实运行时根被外部并发写入的指纹项 9698→9702），静默 90 秒后复跑 43/43；两轮均如实记录。
- 2 项非阻断文档发现（CHANGELOG 条目嵌行与计数、CURRENT 过期表述），已由 `bca1923` 修正。

## 验证汇总

| 检查 | 结果 |
|---|---|
| learning_gate | 19/19（含 3 条盲区收口测试） |
| 全量 Rust | 776 passed / 3 ignored / 0 failed（源码验收 4 次 + 终验 1 次独立复跑） |
| `build -Verify` | 双构建 SHA-256 一致（`6b86d535…983ae`） |
| 62 离线验收 | 静默窗口 43/43（首轮 42/43 为生产并发写入，如实保留） |
| `git diff --check` | 通过 |
| OCSR 读取边界 | 两轮 fresh verifier 均 clean |

## 边界与遗留

- gate 是报告器：un gate 的候选仍可被 S1 store 收下；把 gate 设为 ingest 前置属 S3。
- 更细粒度 span（记录内子区间）与 OpenCode/Codex 结构化裁决不在 S2；当前对它们 fail-closed。
- 同一记录 sha 被多个 manifest 持有时取最旧持有者为 provider/代际权威（内容寻址下持有对象相同，结果一致；未单测）。
- 62 的真实根指纹守卫须在静默窗口运行；本轮首轮 42/43 与静默复跑 43/43 均保留。
- 未做生产安装、真实数据试采或长期记忆写入；默认生产 90 的旧安装哈希与真实 Archive 非空两项失败保留。
