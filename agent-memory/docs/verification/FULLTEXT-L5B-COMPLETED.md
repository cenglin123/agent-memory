# 全文检索优先 L5b：活源对话语义与混合检索 — 完成记录

- **状态**：完成（代码、测试、文档均落在此工作区；未提交、未分发）
- **真实数据 = 否**：全部用例使用系统临时目录中的合成 fixture 与 `backend=fake` 嵌入后端；未读取任何真实会话、真实知识库内容或生产 Archive，未重建 `assets/dist`，未运行 61/62/90，未执行 `git commit`。
- **工作区**：`C:\wt\hybrid-retrieval-20261028`（HEAD `543dbcf`）
- **执行日期**：2026-10-30
- **承担范围**：L5b（活源对话向量缓存 + 混合检索）与 L2/L3 复核非阻断项 N1/N3/N4/N5/N6 + `543dbcf` 的 V2 工具块集成测试。

## 1. 实现

### 1.1 新增/修改的源文件

| 文件 | 内容 |
|---|---|
| `agent-memory/src/recall/fulltext/vectors.rs`（新增） | 活源对话向量缓存：`<cache_root>/live/<source_id>/vectors.bin`（`AMLV` 魔数 + 版本 + 生成键（含 `;ctx=ctx-v1`）+ 量化名 + 投影器版本 + 维度 + 行数 + 按 `doc_key` 排序的行：`doc_key`/文本 SHA-256/嵌入输入 SHA-256/int4 行）；**嵌入输入带结构上下文**（标题 + 助手单元的最近前序用户正文 ≤300 字符 + 正文）；按嵌入输入哈希复用、整源重嵌入判定、`cached_units` 只读文件头、原子写、降级报告；查询侧 `VectorCache::load` + 惰性解码 + `Similarity::{Missing,Stale,Value}` |
| `agent-memory/src/recall/fulltext/hybrid.rs`（新增） | 混合检索：词面支路（FTS5 `bm25`，≥3 字词 OR）、语义支路（`conversation_units` + 余弦，过滤先行）、`archive-relevance-v4` 接受门控、RRF（k=60）融合、按会话去重、`channels` 标注、`detail.hybrid` |
| `agent-memory/src/recall/fulltext/row.rs`（新增） | exact 与 hybrid 共用的行投影与 ref 还原（`ROW_COLUMNS`、`Row::from_query_row`/`unit_text`/`locator_for`/`into_hit`/`load_by_rowids`），去掉两条路径各写一份的重复 |
| `agent-memory/src/cache_store.rs`（新增） | 缓存原子写：`join_checked`（拒绝空/绝对/`..`/分隔符/盘符组件）+ `write_atomic`（同卷临时文件 + rename）。放在 `src/recall` 之外，recall 边界测试的「无文件写 API」约束保持成立 |
| `agent-memory/src/recall/fulltext/mod.rs` | `INDEX_SCHEMA_VERSION` 1→2；`PROJECTOR_VERSION`（`projector-2`）；新增 `conversation_units` 表（rowid/doc_key/text_sha256/session/message/ts/role/session_kind + 两个索引）；`vectors_path`/`source_dir`；`IndexMeta` 增加 `skipped_oversize`/`projector_version`/`newest_unit_ts`；`open_for_query` 对旧投影器代际返回 `INDEX_STALE` + `projector_version_changed`；`rfc3339`；`index_ready`（auto 模式判定） |
| `agent-memory/src/recall/fulltext/coverage.rs`（新增） | exact/hybrid 每个活源的 `detail.coverage`：`indexed_at`/`newest_unit_ts`/`rows`/`sessions`/`vectors`/`stale_since?`；新鲜度只用单值查询（OpenCode `COUNT(*)`+`MAX(ts)`）或 stat 对比水位，任何失败只省略该块，不改变状态与退出码 |
| `agent-memory/src/recall/fulltext/build.rs` | `index_source` 增加 `config` 参数并在全文构建成功后构建向量缓存（结果写入 `detail.vectors`）；Writer 维护 `conversation_units`（插入/删除/reset/会话改名同步 `session_kind`）；`rebuild_reason` 区分 schema/身份/投影器并把 `projector_version_changed` 写进报告 `reason`；三个构建器记录 `meta.newest_unit_ts`；N1：增长但 mtime 早于水位 → 整文件重扫；N3：OpenCode 超大行与 JSONL 超长行计数（`detail.skipped_oversize`、`meta.skipped_oversize`） |
| `agent-memory/src/recall/fulltext/query.rs` | 改用共享 `row::Row`；exact detail 增加 `skipped_oversize`（来自索引 meta） |
| `agent-memory/src/recall/search.rs` | `SearchMode::{Auto,Exact,Hybrid,Legacy}`；`auto` 按源解析（有可读索引 → hybrid，否则 legacy）；过滤键由 exact/hybrid 共用、`order`/`group_by_session` 在 hybrid 报 `hybrid_fixed_order`；hybrid 回退/直连注释（`detail.hybrid`）；`SearchResult.channels` |
| `agent-memory/src/recall/contract.rs` | `SEARCH_MODE_HYBRID`/`SEARCH_MODE_LEGACY`/`ALL_SEARCH_MODES`、`HYBRID_CHANNEL_{LEXICAL,SEMANTIC}`、`DETAIL_KEYS` 12→13（`hybrid`） |
| `agent-memory/src/recall/providers/mod.rs` | `ExactMeta.channels: Option<Vec<&'static str>>`（exact 恒 `None`，hybrid 写 `lexical`/`semantic`） |
| `agent-memory/src/recall/semantic/index.rs` | `VectorRow::from_bytes` 改为 `pub`（与活源缓存共用 int4 行布局） |
| `agent-memory/src/recall/read.rs` | N5：修正 strict-snapshot 拒绝文案的多余空格串 |
| `agent-memory/src/mcp.rs` | `mode` 枚举增加 `hybrid`/`legacy`；`memory_search` description 写明用法指引（标识符 → exact；概念/模糊/换说法 → 缺省 hybrid；`memory_read` 取上下文、`context=turn` 取整轮；结果为空先看 `detail.coverage`） |
| `agent-memory/tests/recall_hybrid.rs`（新增） | 11 个用例（见 §3.1） |
| `agent-memory/tests/recall_fulltext.rs` | N1/N3/N6 与 V2 工具块共 5 个新用例；1 条 auto 断言按新规格改写；清理 2 条 unused warning |
| `agent-memory/tests/recall_contract.rs`、`tests/recall_cli_mcp.rs` | 2 处冻结表按新增合同更新（见 §6） |
| `agent-memory/README.md` | 章节扩为「Live full-text index, exact mode and hybrid recall」：模式表与用法指引、向量缓存格式与降级、混合检索（支路/门控/RRF/channels/会话去重/detail）、水位盲区与恢复、`skipped_oversize` |
| `assets/references/agent-memory-archive.md` | §4.4 扩为 L2/L3/L5b：缺省模式判定、用法指引、向量缓存、hybrid 合同、水位盲区、超大行计数 |
| `CHANGELOG.md` | 顶部新增「全文检索优先 L5b：活源对话语义与混合检索」 |

### 1.2 关键行为

- **向量范围**：只嵌入 `role ∈ {user, assistant}` 且 `kind = text` 的单元；`tool_input`/`tool_output` 只进全文索引。设备记忆是 `role=user, kind=text`，因此同样被嵌入（按机械规则，不是特例）。
- **缓存身份**：文件头写 `generation_key`（模型/维度/归一化/后端 + `ctx=ctx-v1` 输入规则）、量化名（`int4`）与投影器版本。任一不同即整源重嵌入；同一身份下按**嵌入输入哈希**复用，追加只嵌入新增内容，会话改名精确重嵌入该会话。
- **降级可分**：模型不可用 → `detail.vectors.status = "degraded"` + 原因，**不写半成品文件**，`index` 退出 0；全文失败仍按原状态非零。查询侧无向量缓存 → `detail.hybrid.semantic = "degraded"` + 原因，只用词面支路，源状态仍 `ok`/`empty`。
- **过滤先于候选**：`session_id`/`since`/`until`/`roles`/`session_kinds`（以及 `kinds` 与嵌入范围的交集）在 `conversation_units` 的 SQL 里生效，语义支路不会先取全局最相似再过滤。
- **会话去重**：hybrid 恒按会话去重（每会话最佳一条），`session_hits` = 该会话在融合候选中的条数；`order`/`group_by_session` 在 hybrid 下被拒，避免一个请求同时表达两种排序。
- **auto 判定**：按源判定；无缓存/索引过期/不可读 → legacy，因此无缓存环境与既有测试不受影响。
- **`--strict-snapshot`**：与 exact 一致——索引按配置源身份回答，快照只影响回退扫描（本轮先实现为拒绝，复核后改为与 exact 对齐，避免两个索引模式行为分叉）。
- **缓存可删除**：`vectors.bin` 删除后重建字节一致（行序固定 + fake 后端确定性）；`fts.sqlite` 与它都不承载证据身份。

## 2. 设计决定（规格未逐字规定处）

1. **`conversation_units` 表**：语义支路若直接对 `units`（FTS5 虚表）全表扫描，会读到工具正文主导的 1.6 GB 载荷。新增一张普通表只保留对话单元的定位与文本哈希（约 3 万行），过滤与候选生成都在这张表上完成；它由 Writer 与 `units` 同步维护，`schema_version` 因此 1→2（旧缓存判 `INDEX_STALE` → 重建）。
2. **文本哈希双写**：`conversation_units.text_sha256` 与 `vectors.bin` 行内哈希相对照，天然得到「向量是否由当前文本生成」的判定（`stale_vectors`），查询期不需要重新哈希正文。
3. **向量写入放 `src/cache_store.rs`**：`tests/recall_boundary.rs` 禁止 `src/recall` 出现 `fs::write`/`File::create`/`OpenOptions`/`atomic_write`。把唯一写入口放到 recall 之外、并且只接受「根 + 受校验的路径组件」，既保留该边界断言，又让缓存写入无法指向源树或 Archive 代际。
4. **`embedded` 计数口径**：单位是单元数，不是嵌入调用数；同一文本在多个单元出现时只嵌入一次（内容寻址），但每个单元都计入 `embedded`。
5. **降级不写文件**：部分缓存会静默少覆盖语料，因此失败时保留既有有效缓存（查询期哈希会剔除变化行），仅报告 `degraded`。
6. **RRF 候选窗口**：两支路各取前 200（`HYBRID_BRANCH_WINDOW`、`SEMANTIC_MAX_CANDIDATES`）。RRF 只用头部排名，200 足以覆盖 10–100 条的常规 `limit`，同时把会话去重与门控成本锁住。
7. **门控口径**：词面覆盖的「词数」用 ≥3 字符的 FTS 词（与 exact 的 `MATCH` 集合一致），而不是通用分词结果；单词语查询豁免「≥2 个不同词」但仍要求覆盖。
8. **detail 块**：hybrid 写 `detail.hybrid`（`mode`/`semantic`/`reason?`/`thresholdsVersion`/`fusion`/`rrf_k`/`lexical_candidates`/`semantic_candidates`/`gated_out`/`sessions`/`stale_vectors?`/`skipped_oversize?`），不复用 `detail.exact`（那个块的语义是「索引态与回退原因」）。
9. **hybrid 不回退到 exact**：语义支路缺失只减一条支路，不是换检索语义；只有索引整体不可用才回退扫描并标 `degraded`。
10. **`channels` 承载在 `ExactMeta`**：hybrid 命中同样需要 `timestamp`/`role`/`kind`/`session_kind`/`session_hits`，复用同一内部结构、只多一个 `channels` 字段，避免给 10 处 `SourceHit` 构造点各加字段。
11. **`skipped_oversize` 语义**：记录**本次构建扫描窗口内**观察到的超大行数（重建=全量；增量=窗口内），不是历史累计。它是可观测信号，不参与任何判定；查询 detail 展示索引里记下的值。
12. **rustfmt 不可用**：钉定工具链 `1.99.0` 未安装 rustfmt/clippy 组件，未做自动格式化；手写风格与周边一致。

## 3. 测试用例表

### 3.1 `agent-memory/tests/recall_hybrid.rs`（16：首轮 11 + 返工 5，全部 fake 后端）

| # | 用例 | 覆盖点 |
|---|---|---|
| 1 | `vectors_are_reused_incrementally_and_reembedded_on_model_change` | 首建 `built`（只嵌入 2 条对话文本，工具输出不计）；重复构建 `unchanged` 且文件字节不变（`reused=2`）；新增 1 条 → `updated`（`reused=2`/`embedded=1`）；换模型 → `built` 且 `reused=0`/`embedded=3`，生成键含新模型 |
| 2 | `deleting_the_vector_cache_and_rebuilding_gives_identical_bytes` | 删除 `vectors.bin` 后重建，字节完全一致；`fts.sqlite` 仍在 |
| 3 | `an_unavailable_model_degrades_only_the_vector_report` | `backend=python` + 不存在的解释器：`report.status=built`、`rows>0`、`vectors.status=degraded` + 原因含 `unavailable`、不写向量文件；CLI `index` 退出 0 且报告 `ok=true` |
| 4 | `hybrid_fuses_both_branches_and_labels_channels` | 同一查询命中三通道：`["lexical","semantic"]`/`["semantic"]`/`["lexical"]`；双支路命中排首位；`score_kind=rrf`、`rank_basis=rrf_lexical_semantic`、`session_hits`、`detail.hybrid` 的 `semantic=available`/`thresholdsVersion=archive-relevance-v4`/`fusion=rrf`/`rrf_k=60` |
| 5 | `hybrid_dedupes_by_session_and_counts_its_candidates` | 同会话两条命中 → 每会话仅 1 条、`session_hits=2` |
| 6 | `hybrid_applies_filters_before_semantic_candidates` | worker 会话与查询文本逐字相同（必然最相似）：无过滤时排第一；`session_kinds=interactive` 后不出现；`session_id` 后只返回该会话；`roles=user` 后助手轮次消失 |
| 7 | `hybrid_degrades_to_the_lexical_branch_explicitly` | 无 embedding 配置 → `semantic=degraded`/`embedding_not_configured`；损坏/截断 `vectors.bin` → `vectors_corrupt`；三种情形都仍有词面结果且 `channels=["lexical"]` |
| 8 | `default_mode_switches_to_hybrid_only_when_a_cache_exists` | 无索引时 auto 与显式 `legacy` 逐字节一致且无 exact/hybrid/channels 键；建索引后 auto 出现 `detail.hybrid` 与 `channels`；`legacy` 仍逐字节等于建索引前；`exact` 无 `channels`、有自己的块；`order` 在 hybrid 报 `hybrid_fixed_order`、`roles` 在 legacy 报 `exact_only`、非法 `since` 报 `invalid_rfc3339` |
| 9 | `strict_snapshot_answers_from_the_index_like_exact_mode` | `--strict-snapshot` 下 hybrid 与 exact 都仍按索引回答（`status=ok`、`semantic=available`） |
| 10 | `mcp_hybrid_search_never_writes_the_cache` | MCP `tools/call memory_search`（缺省 hybrid）前后 `cache` 树（路径+大小+mtime_ns）完全一致 |
| 11 | `cli_and_mcp_agree_on_hybrid_envelopes` | 同一请求 CLI 与 MCP `structuredContent` 完全相等 |
| R1 | `the_embedding_input_carries_the_title_and_the_preceding_user_turn`（返工） | 稳定上下文 → `reused=3` 且字节不动；改会话标题 → 该会话 3 个单元全部重嵌入且字节变化；改写用户消息 → 3 个单元重嵌入（助手携带前序用户正文）；改写助手正文 → 只 1 个重嵌入 |
| R2 | `a_newer_projector_version_rebuilds_the_index_and_marks_queries_stale`（返工） | 篡改 `meta.projector_version` → hybrid `degraded` + `INDEX_STALE` + `cause=projector_version_changed`，exact 同因，coverage 仍可见；`index` 重建并写 `reason=projector_version_changed`；同一单元集下向量按输入哈希复用（`unchanged`），`projector_version=projector-2` |
| R3 | `the_vector_cache_reports_a_projector_mismatch_on_its_own`（返工） | 等长改写 `vectors.bin` 头里的投影器版本 → hybrid 只走词面并报 `projector_version_changed`，coverage `vectors=null`；下次 `index` 全量重嵌入并恢复原字节 |
| R4 | `coverage_reports_the_index_snapshot_and_marks_a_lagging_source`（返工） | 建索引后 `coverage` 有 `indexed_at`/`newest_unit_ts`/`rows=4`/`sessions=3`/`vectors=2` 且无 `stale_since`；写入新消息后 `stale_since` 出现且状态仍 `ok`；重建后消失且 `rows=5` |
| R5 | `coverage_marks_a_jsonl_source_whose_file_grew`（返工） | Claude Code JSONL：初始无 `stale_since`、`vectors=null`（未配置嵌入）；追加一行后 `stale_since` 出现 |

### 3.2 `agent-memory/tests/recall_fulltext.rs`（新增 5 + 改写 1）

| # | 用例 | 覆盖点 |
|---|---|---|
| 12 | `a_growing_file_with_an_older_timestamp_is_rescanned_whole`（N1） | 文件变长但 mtime 回拨 → 整文件重扫：旧前缀命中消失、新前缀与追加行命中 |
| 13 | `oversize_lines_are_counted_in_the_index_report_and_query_detail`（N3） | >64 MiB 行：`report.detail.skipped_oversize=1`、`rows=1`，查询 `detail.exact.skipped_oversize=1` |
| 14 | `exact_newest_and_oldest_order_across_sources`（N6a） | 四源 `PrependPath=1`：`newest` 全局非增、`oldest` 全局非减、命中集合相同、涉及 ≥3 源 |
| 15 | `a_truncated_tool_output_reads_back_over_its_full_range`（N6b） | 索引截断（20 000 字符）但 ref `byte_end>20000`；`max_bytes` 足够时读到 `TAILMARK`、`range_sha256` 与 ref 指纹一致、`truncated=false`；默认预算 `truncated=true` 且 `ok` |
| 16 | `opencode_v2_tool_blocks_are_indexed_by_legacy_and_exact_and_read_back`（543dbcf） | V2 形态（`name` + `state.content` 文本块数组）：legacy 词面命中入参与输出；exact 分别得到 `tool_input`/`tool_output`；两个 ref 都回读到同一条投影（含 `V2CMD=1` 与 `V2OUT=2`） |
| 17 | `auto_mode_uses_hybrid_only_after_an_index_exists_and_rejects_exact_keys`（改写，见 §6） | 无索引 auto=legacy 逐字节一致；建索引后 auto→hybrid；`legacy` 仍一致；auto 下 `roles`/`order`/`group_by_session` 仍 `exact_only` |

### 3.3 单元测试（`src/cache_store.rs` 2 + `src/recall/fulltext/vectors.rs` 3）

`cache_store`：`components_cannot_escape_the_root`（`..`/`.`/空/含分隔符/盘符组件全部拒绝）、`write_is_atomic_and_replaces_the_previous_bytes`（覆盖写 + 无残留临时文件）。
`vectors`（返工）：`embedding_input_carries_title_and_the_preceding_user_turn`（用户/助手/无前序用户/空白前序用户的构造与分隔符）、`the_preceding_user_turn_is_capped_by_characters`（300 字符按字符截断）、`the_input_rule_is_part_of_the_live_generation_key`（`…;ctx=ctx-v1`）。

### 3.4 既有测试

`cargo test --locked --no-fail-fast`：**604 passed / 2 ignored / 0 failed**。基线（HEAD `543dbcf`）为 578 passed / 2 ignored；L5b 首轮新增 18 条，返工（见 §8）新增 8 条。Python：`test_recall_onboarding` 27 + `test_recall_embedding` 21 + `test_recall_eval` 12 = **60 passed**（返工新增 1 条 onboarding 用例）。

## 4. 命令与退出码

| 命令 | 结果 |
|---|---|
| `cargo test --locked --no-fail-fast`（`agent-memory/`） | 604 passed / 2 ignored / 0 failed，exit 0（返工后连续 2 次） |
| `cargo build --locked` | exit 0，无 warning |
| `cargo clippy` / `cargo fmt` | 不可用（钉定工具链缺组件），未执行 |
| `python -m pytest test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py -q`（`assets/scripts/`） | 60 passed，exit 0 |
| 合成 fixture `agent-memory index --config config.json`（fake 后端） | exit 0；`vectors={status:built, units:2, embedded:2, bytes:351}`；`rows=4` |
| 同上：删除 `vectors.bin` 后重新 `index` | exit 0；`sha256(vectors.bin)` 前后同为 `4c182a2b0d79f0465eaca986f37753f237c2c9416f34cbccde73282509852358` |
| `memory_search` `mode=hybrid`（`缓存 失效 py3`） | exit 0；`detail.hybrid.semantic=available`、`thresholdsVersion=archive-relevance-v4`、命中 `channels=["lexical","semantic"]`、`session_hits=1`、`gated_out=1` |
| `memory_search` `mode=exact`（`V2CMD=1` / `V2OUT=2`） | exit 0；分别返回 `kind=tool_input` / `kind=tool_output` |
| `memory_search` `mode=legacy` | exit 0；2 条命中，结果键只有旧字段（无 exact/hybrid/channels） |
| `agent-memory index`（配置缺 `cache_root`） | exit 2；`{"ok":false,"code":"INVALID_REQUEST","detail":{"path":"cache_root"}}` |
| `agent-memory index`（`backend=python` + 不存在解释器） | exit 0；`status=built`、`rows=4`、`vectors.status=degraded`（`python_unavailable: …`），目录内只有 `fts.sqlite` |

## 5. 限制与未做

- **未接触真实数据**：没有在真实 OpenCode/Codex/Claude 历史上建索引或复跑 L5 指标；本步不改变计划 §4a 的评估结论，只实现通道。
- **未做分布物**：未重建 `assets/dist`、未跑 61/62/90、未 `git commit`（由 orchestrator 执行）。
- **向量维度/模型**：用例一律 `dim=32`/`fake-hash-v1`；真实 `qwen3-embedding:4b`、dim 512 下的门控表现不在本步验证范围（常量未改）。
- **短词与 CJK**：词面支路只把 ≥3 字词交给 `MATCH`，因此纯短词/未分词的中文问句主要靠语义支路；这是规格要求的行为，已在 README 写明。
- **向量新鲜度**：与全文索引共用同一水位语义；原地改写且时钟不动时两者同时陈旧，恢复路径是 `--rebuild`。查询期用文本哈希剔除「向量属旧文本」的单元，但不检测「全文行本身陈旧」。
- **并发**：缓存写入为单进程设计（`index` 是唯一写方）；没有跨进程锁。
- **`skipped_oversize` 非累计**：见 §2.11。
- **rustfmt/clippy 不可用**：格式为手工风格。
- **环境既有抖动**：`tests/recall_archive.rs::retrieval_index_carries_generator_identity_and_rebuilds_identically` 在本机**全量并行**跑时偶发 `Io(NotFound)`（`atomic_write` 的 tmp 文件在 rename 前消失，Windows Defender/临时目录环境所致）。已在 HEAD `543dbcf` 的原始检出上复现同样失败（`git archive` 到临时目录 + 独立 target 跑全量），单跑 5/5 通过，本工作区最终连续 2 次全量通过。与本步改动无关，未修改该用例。

## 6. 既有断言/测试的修改与原因（逐条）

1. `tests/recall_contract.rs::frozen_tables_have_no_duplicates_and_consistent_sets`
   - `DETAIL_KEYS.len()` 12 → 13，并新增 `hybrid ∈ DETAIL_KEYS`：hybrid 的分支计数、融合标识、`thresholdsVersion` 与降级原因需要一个 detail 块（规格要求「detail 写 thresholdsVersion / semantic degraded」）。
2. `tests/recall_cli_mcp.rs::mcp_search_schema_declares_the_session_id_filter`
   - `mode` 枚举 `["auto","exact"]` → `["auto","exact","hybrid","legacy"]`：新增两个模式，MCP 与 CLI 必须同一合同。属性数量仍为 17（未新增参数）。
3. `tests/recall_fulltext.rs::auto_mode_is_unchanged_by_the_index_and_rejects_exact_keys`
   - 改写为 `auto_mode_uses_hybrid_only_after_an_index_exists_and_rejects_exact_keys`。原因：该断言的前提是「建索引后缺省响应逐字节不变」，而 L5b 规格明确要求「缺省模式在有可用 fts 缓存时走 hybrid」。改写后仍保留原意图的两半：**无缓存时** auto 与 `legacy` 逐字节一致、且不含 exact/hybrid 键；建索引后 `mode="legacy"` 仍逐字节等于建索引前的响应；auto 下 `roles`/`order`/`group_by_session` 仍被拒绝。这是唯一一处因规格变更而调整语义的既有断言。
4. `tests/recall_fulltext.rs` 清理 2 条 unused warning（N4）：删除未使用的 `index_source` 导入与未使用的 `hit_texts` 辅助函数（无行为变化）。

未修改其它既有断言；无人为放宽。

## 7. 未做（明确排除）

- N2（`session_kind` 只用了标题+父会话两路信号，未读 agent 列与 ocsr 派发记录）不在本步范围，常量仍集中在 `fulltext/mod.rs`。
- 未改 AGENTS.md、未改计划文件（checkpoint 由 orchestrator 写入）、未改 `docs/CURRENT.md`。
- 未做 hybrid 的模式级性能基准（L5 已给出语义与词面 p95；hybrid 额外成本仅为一次关键词扫描 + 一次全量余弦，本步未单独计量）。

## 8. 返工（orchestrator 追加的四项）

- **状态**：完成。起点 HEAD `f2156a7`（上一轮 L5b 已提交）；本轮仍未提交、未重建分发物、未跑 61/62/90。
- **真实数据 = 否**：全部用例仍为临时目录合成 fixture + `backend=fake`，未读取真实会话/知识库/生产 Archive。
- **范围**：只做下列四项，其余行为有意保持（模式判定、门控常量、RRF、按会话去重、`exact`/`legacy` 语义、缓存可删除性）。

### 8.1 实现

| 项 | 实现 | 文件 |
|---|---|---|
| 1 对话嵌入带上下文 | 嵌入输入改为 `标题\n[前序用户正文≤300 字符]\n本单元正文`（助手）与 `标题\n本单元正文`（用户）。**全文文本、snippet、回读区间不变**。输入规则版本 `EMBED_INPUT_RULE = "ctx-v1"` 进入**活源**生成键（`…;backend=fake;ctx=ctx-v1`）；Archive 语义生成仍用 `plan.generation_key()`，不受影响。缓存行同时存**文本哈希**（查询期判定陈旧）与**输入哈希**（复用键），因此改标题只重嵌入该会话、改用户消息会连带重嵌入其后携带该前序的助手单元。 | `fulltext/vectors.rs`（`live_generation_key`/`embedding_input`/`embedding_inputs`/`truncate_chars`） |
| 2 投影器版本进索引身份 | 新增常量 `fulltext::PROJECTOR_VERSION = "projector-2"`（覆盖 543dbcf 的 V2 工具块投影与 `ctx-v1`）。写入索引 `meta.projector_version` 与 `vectors.bin` 文件头。`index` 发现不同即整源重建并写 `reason: "projector_version_changed"`（`--rebuild` 时不误标）；查询侧 `open_for_query` 判 `INDEX_STALE`，`detail.exact.cause`/`detail.hybrid.cause = projector_version_changed`；向量侧加载返回同名原因。存储格式因此升到 `VECTORS_VERSION=2`（旧文件另报 `vectors_version_mismatch`）。 | `fulltext/mod.rs`、`fulltext/build.rs`（`rebuild_reason`）、`fulltext/vectors.rs` |
| 3 覆盖范围与新鲜度 | exact/hybrid 的每个活源 `detail.coverage = {indexed_at, newest_unit_ts, rows, sessions, vectors, stale_since?}`。`indexed_at` 取 `meta.built_at_ms`、`newest_unit_ts` 取构建期记录的 `MAX(ts)`、`vectors` 只读缓存文件头且**仅当生成键/量化/投影器/维度都匹配当前配置**才给数字（否则 `null`）。`stale_since` 只在轻量检查发现源已更新时出现：OpenCode 一次 `SELECT COUNT(*), MAX(time_updated)`；JSONL/设备只做文件大小+mtime 与水位对比（删文件时用索引时间兜底）。任何失败只省略字段，不改变状态/退出码；索引缺失或过期时同样挂载，扫描回退时与扫描自身的 coverage 字段合并。MCP description 增加「结果为空时先看 coverage」。 | `fulltext/coverage.rs`（新增）、`fulltext/query.rs`、`fulltext/hybrid.rs`、`recall/search.rs`（回退注记）、`mcp.rs` |
| 4 知识库不精排 | `recall_onboarding.py` 在写 `interpreter.kb.env` 时 `env.setdefault('ASK_AUTO_RERANK', '0')`：新配置写入 `0`，既有配置已显式设置则保留（不覆盖）。理由写进脚本注释与文档：agent 检索路径只返回证据、由调用方 agent 判断，交叉编码器精排每次多耗数秒，只对直接运行 `ask.py` 有价值。 | `assets/scripts/recall_onboarding.py`、`test_recall_onboarding.py` |

### 8.2 设计决定（返工新增）

1. **复用键 = 嵌入输入哈希，不是文本哈希**：上下文（标题、前序用户）变化必须改变复用键，否则改名后仍会命中旧向量。文本哈希保留在行内，用于查询期「该向量是否仍对应这条正文」的廉价判定（`stale_vectors`）。
2. **投影器版本与存储布局分开**：`INDEX_SCHEMA_VERSION` 管表结构，`PROJECTOR_VERSION` 管「同一份源数据应该投影出什么」。前者变化是读不了，后者变化是读得懂但内容不对——两者都必须重建，但原因要能分开报告。
3. **向量不随 fts 的投影器重建而盲目重嵌入**：投影器重建后若对话单元集合与正文不变，向量按输入哈希复用（报告 `unchanged`）。向量缓存自己的文件头仍带投影器版本，供「旧代码写的缓存」在升级后明确失效。
4. **coverage 永不改变结果语义**：它是旁路信息，失败即省略；`stale_since` 只在源确实领先时出现，避免把「刚 index 完」误标成陈旧。OpenCode 用一次聚合查询而非多次探测，JSONL/设备不打开任何文件正文。
5. **`stale_since` 取值**：使索引落后的那次源变更时间（OpenCode 为 `MAX(ts)`，JSONL/设备为该文件 mtime）；因删除而落后时没有更合适的时钟，取索引自身时间。README 与参考文档写明该口径。
6. **用户消息的前序上下文只管助手**：用户单元只带标题；助手单元带「同一会话中按 `(ts, rowid)` 排序的最近前序用户正文」，无前序时退化为标题+正文（不留空分隔符）。
7. **`ASK_AUTO_RERANK` 用 `setdefault`**：保持「用户显式设置优先」，且重装幂等。
8. **回退时合并而非覆盖 `detail.coverage`**：扫描 provider 早就用同名键报告 `scanned_sessions`/`total_sessions`/`session_filter`。索引回退时答案是扫描给的，因此把索引快照字段并入同一对象（字段名互不冲突），不丢扫描口径。

### 8.3 测试（返工增量，已并入 §3）

- `tests/recall_hybrid.rs` 新增 5：`the_embedding_input_carries_the_title_and_the_preceding_user_turn`、`a_newer_projector_version_rebuilds_the_index_and_marks_queries_stale`、`the_vector_cache_reports_a_projector_mismatch_on_its_own`、`coverage_reports_the_index_snapshot_and_marks_a_lagging_source`、`coverage_marks_a_jsonl_source_whose_file_grew`（详见 §3.1 的 R1–R5 行）。
- `src/recall/fulltext/vectors.rs` 新增 3 条纯函数单元测试（输入构造/300 字符截断/生成键含 `ctx-v1`）。
- `assets/scripts/test_recall_onboarding.py` 新增 1：`test_rerank_default_is_written_and_an_explicit_value_is_kept`（新配置写 `0`、显式 `1` 被保留、缺键时补默认）。
- 既有断言：**本轮无任何修改**（无放宽、无改写）。
- 合计：`cargo test --locked --no-fail-fast` **604 passed / 2 ignored / 0 failed**；Python **60 passed**（27 + 21 + 12）。

### 8.4 命令与退出码（返工）

| 命令 | 结果 |
|---|---|
| `cargo test --locked --no-fail-fast` | 604 passed / 2 ignored / 0 failed，exit 0（连续 2 次） |
| `cargo build --locked` | exit 0，无 warning |
| `python -m pytest test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py -q` | 60 passed，exit 0 |
| 旧 L5b 合成缓存上运行 `agent-memory index`（无 `projector_version` 的 meta） | exit 0；`status=built`、`reason=projector_version_changed`、向量 `status=built`、生成键含 `;ctx=ctx-v1`、`projector_version=projector-2` |
| 同 fixture `memory_search`（缺省 hybrid） | exit 0；`detail.coverage = {indexed_at, newest_unit_ts, rows:4, sessions:1, vectors:2}`，无 `stale_since`；`detail.hybrid.semantic=available` |
| 篡改 `meta.projector_version` 后查询（测试内） | `source_status.status=degraded`、`detail.reason=INDEX_STALE`、`cause=projector_version_changed`，coverage 仍挂载 |
| 篡改 `vectors.bin` 头投影器版本后查询（测试内） | 状态仍 `ok`，`detail.hybrid.semantic=degraded`、`reason=projector_version_changed`，`coverage.vectors=null` |

### 8.5 限制与未做（返工）

- 未在真实数据上验证 `ctx-v1` 对召回的影响（任务边界：不读真实会话）。上下文会改变向量，因此**升级后必须先跑一次 `index`**（`projector_version_changed` 会强制重建，无需手工删缓存）。
- `coverage.stale_since` 的 OpenCode 检查是一次 `COUNT(*)`+`MAX(ts)` 全表聚合：单值、无正文读取，但在超大库上仍有成本；本轮未单独基准（合成 fixture 上可忽略）。[UNCERTAIN] 真实 20 万行库的单次成本需在本机复测。
- JSONL/设备的新鲜度检查需要枚举文件（与 `index` 相同的目录遍历），只做 `stat`，未做缓存。
- 助手单元的「前序用户正文」取同会话最近一条用户文本单元；跨会话/跨派生的用户消息不参与。
- Archive 语义通道未改（仍用 `plan.generation_key()`、无 `ctx` 后缀）。
- 未做覆盖率的 MCP schema 变更（`coverage` 是响应字段，不是请求参数）。
- `cargo fmt`/`clippy` 仍不可用（工具链缺组件）。

## 9. 返工 2（orchestrator 本机真实数据评估发现的两处合同/检索缺陷）

- **状态**：完成。起点 HEAD `dac91b9`（含 `cb7cb6b` 上一轮成果、`babfc8c` 套件移除精排探测、`dac91b9` 嵌入输入整条截断至 8000 字符 + `ctx-v2`）。本轮仍未提交、未重建分发物、未跑 61/62/90。
- **真实数据 = 否**：用例仍为临时目录合成 fixture + `backend=fake`，未读取真实会话/知识库/生产 Archive。
- **范围**：只修两处合同/检索缺陷（缺省模式的过滤参数、词面支路的中日韩切分），其余行为有意保持（门控常量与 `thresholdsVersion`、RRF、按会话去重、exact/legacy 语义、向量缓存格式与 `ctx-v2`）。

### 9.1 实现

| 项 | 实现 | 文件 |
|---|---|---|
| 1 缺省模式与过滤参数 | 请求解析不再在 `auto` 下拒绝 `since`/`until`/`roles`/`kinds`/`session_kinds`，也不再拒绝 `order`/`group_by_session`（记录为 `exact_hints` / `has_index_filters`）。按源解析：有可读索引 → 带 `order`/`group_by_session` 时 **exact**、否则 **hybrid**；无索引时——若请求带了过滤或排序，该源回退扫描并标 `degraded` + `reason = INDEX_REQUIRED`（`detail.hybrid|exact.hint = "run agent-memory index"`，索引存在但过期时附 `cause`），其它源与整请求不受影响；若什么都没带，保持原来的 legacy 扫描。每个源 `detail` 写 `resolved_mode`（`hybrid`/`exact`/`legacy`）。显式 `legacy` 仍拒绝 7 个键（`exact_only`，只是文案改为「use the default mode (auto) or exact」）；显式 `hybrid` 仍拒绝 `order`/`group_by_session`（`hybrid_fixed_order`）。 | `recall/search.rs`、`recall/contract.rs`（`INDEX_REQUIRED`、`DETAIL_KEYS` 13→14 加 `resolved_mode`）、`fulltext/query.rs`、`fulltext/hybrid.rs`、`mcp.rs` |
| 2 词面支路 CJK 切分 | `lexical_parts(terms)`：引号短语保持字面整段；非引号词项按字符切 CJK/非 CJK 段——CJK 段（汉字含扩展 A/B、假名、谚文）≥3 字符切成重叠 3 字符片段，非 CJK 段维持「≥3 字符整段」规则，短段不进 `MATCH`。片段按首次出现去重、最多 48 项，超出数量写 `detail.hybrid.lexical_terms_truncated`。门控的「词面覆盖」分子/分母改用**同一批片段**（含 CJK trigram），阈值常量与 `thresholdsVersion` 未动；exact 模式不切分。 | `fulltext/hybrid.rs` |

### 9.2 设计决定（返工 2）

1. **缺省模式不再在解析期否决参数**：解析期能判断的只有「显式 legacy 不支持」，其余取决于该源有没有索引，因此下沉到 `effective_mode` 按源判定。这让 agent 最自然的写法（不写 `mode` + 加时间范围）直接可用。
2. **`INDEX_REQUIRED` 是新状态值而不是复用 `INDEX_MISSING`**：前者表示「缺省模式为了兑现过滤/排序需要索引」，后者是 exact 自身路径的降级；两者补救动作相同但语义不同，且前者不应让整请求失败。它仍走 `degraded`（退出码 4/10、不进 `errors[]`），与既有 `degraded` 规则一致。
3. **`order`/`group_by_session` 强制 exact**：只有 exact 的候选流能兑现排序与每会话计数；静默忽略会违反「不静默降级」。解析结果写进 `resolved_mode`，并用「与显式 exact 逐字节一致」的测试钉住。
4. **`resolved_mode` 放在 `detail` 顶层**：它与 `exact`/`hybrid` 块并列，回退/直连/成功三种情况都写，agent 不需要从请求反推实际路径。
5. **CJK 切分只在 hybrid 词面支路**：exact 的合同是「短语/多词 AND」，切分会改变其语义；hybrid 的目标是「自然问句不该因为整句不连续而零候选」。
6. **覆盖门控复用同一批片段**：若门控仍按原始词项计数，中文问句的分母是 1、分子通常是 0，v4 的「词面覆盖足」与「同轮双支持」永远失效——这正是本轮要修的根因 2。用同一批片段后两条规则对中文可用，阈值保持不变。
7. **48 项上限**：`OR` 项随 CJK 长度线性增长（每字一个 trigram）；48 足以覆盖 50 字以内的问句，同时把 FTS 表达式与 bm25 计算量锁住，超出部分计数上报而不是静默丢弃。
8. **`exact_only` 错误码保留**：显式 `legacy` 的拒绝仍是 `INVALID_REQUEST` + `path` + 旧 reason 值，避免破坏已依赖该码的调用方；只有文案更新。
9. **`INDEX_REQUIRED` 附 `cause`**：索引存在但过期（如在旧投影器代际）时，`reason` 保持 `INDEX_REQUIRED`（同一补救动作），`cause` 给出精确原因（`projector_version_changed` 等），不丢失诊断信息。

### 9.3 测试（返工 2 增量）

单元测试（`src/recall/fulltext/hybrid.rs`，7）：`cjk_classification_covers_han_kana_and_hangul`、`a_chinese_phrase_becomes_overlapping_trigrams`、`cjk_runs_shorter_than_three_characters_do_not_enter_match`、`mixed_scripts_keep_ascii_runs_and_split_the_cjk_part`、`kana_and_hangul_are_split_like_han`、`quotes_keep_the_literal_phrase`、`duplicate_parts_are_dropped_and_the_cap_reports_truncation`。

集成测试（`tests/recall_hybrid.rs`，5）：

| 用例 | 覆盖点 |
|---|---|
| `the_default_mode_accepts_filters_and_uses_hybrid` | 无 `mode` + `since`：`resolved_mode=hybrid`、状态 `ok`；被时间过滤的会话不出现在结果中，而同请求不带 `since` 时出现 |
| `the_default_mode_degrades_only_the_source_without_an_index` | oc 有索引、dev 无：oc `ok`/`hybrid`；dev `degraded` + `reason=INDEX_REQUIRED` + `resolved_mode=hybrid` + `hint`；结果仍来自 oc、`errors[]` 为空 |
| `order_and_group_by_session_resolve_the_default_mode_to_exact` | `order`/`group_by_session` 无 `mode` → `resolved_mode=exact`，序列化结果与显式 `mode:"exact"` 完全一致；`group_by_session` 带 `session_hits`；不带提示键时仍是 hybrid |
| `explicit_legacy_and_hybrid_keep_rejecting_unsupported_keys` | legacy+since → `INVALID_REQUEST`(`path=since`/`exact_only`)；hybrid+order/group_by_session → `hybrid_fixed_order`；缺省模式接受四类参数 |
| `a_chinese_question_without_spaces_reaches_the_lexical_branch` | 整句不连续的中文问句：`lexical_candidates>0`、目标会话命中且 `channels` 含 `lexical`；命中经门控通过（覆盖率通路） |
| （并入）`a_newer_projector_version_…` | 追加断言：缺省模式 + 过滤在过期索引上给 `INDEX_REQUIRED` + `cause=projector_version_changed` + `resolved_mode=hybrid` |

### 9.4 既有断言/测试的修改与原因（返工 2 逐条）

1. `tests/recall_fulltext.rs::auto_mode_uses_hybrid_only_after_an_index_exists_and_rejects_exact_keys`
   - 末尾三条「auto 拒绝 roles/order/group_by_session」改为「显式 `mode:"legacy"` 拒绝同一批键」。原因：本轮规格明确要求缺省模式接受这些参数，原断言的前提被推翻；拒绝能力本身仍被测到（换成 legacy），未放宽为「不报错」。
2. `tests/recall_fulltext.rs::exact_filters_cover_time_role_kind_session_kind_and_session_id`
   - 末段「无 mode 的 `since` 被拒」改为「无 mode 的 `since` 被接受」，并把拒绝断言改为 `mode:"legacy"`。原因同上。
3. `tests/recall_contract.rs::frozen_tables_have_no_duplicates_and_consistent_sets`
   - `DETAIL_KEYS.len()` 13 → 14，新增 `resolved_mode ∈ DETAIL_KEYS`。原因：规格要求每个结果源在 detail 里报告实际路径。
4. 未修改其它既有断言；无人为放宽（新增的 5 条集成断言都比原断言更强：既验路径也验过滤/降级/一致性）。

### 9.5 命令与退出码（返工 2）

| 命令 | 结果 |
|---|---|
| `cargo test --locked --no-fail-fast` | **617 passed / 2 ignored / 0 failed**，exit 0（基线 605/2：+7 单元 +5 集成） |
| `cargo build --locked` | exit 0，无 warning |
| `python -m pytest test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py -q` | 59 passed，exit 0 |
| 合成 fixture：`index --source oc` 后 `memory_search` `{"query":"缓存失效 py3","since":"2023-11-14T22:15:00Z"}`（无 mode） | exit 0；`status=ok`、`resolved_mode=hybrid`；结果只含时间过滤后那条消息 |
| 同上 `{"query":"当时为什么说第二阶段还不能完成？","sources":["oc"]}`（无 mode，无空格） | exit 0；`resolved_mode=hybrid`、`lexical_candidates=1`、`gated_out=1`、目标会话命中且 `channels=["lexical","semantic"]` |
| 同上 `{"query":"缓存失效","roles":["user"]}`（无 mode，dev 未建索引） | exit 0；`dev`=`degraded`/`INDEX_REQUIRED`/`resolved_mode=hybrid`/`hint="run agent-memory index"`；`oc`=`ok` |

### 9.6 限制与未做（返工 2）

- **未复跑真实数据**：22+7 用例的指标不在本步验证范围；本步只修合同与词面候选生成，门控常量未动。
- **门控阈值对中文仍可能偏严**：阈值（0.34 / ≥2 词 / 0.50×2 / 0.60）是按 Archive 开发集标定的，未针对 CJK trigram 重新标定。本轮只保证「词面覆盖」这条通路对中文**可用**（分母不再恒为 1），不保证每条中文问句都能过门控。[UNCERTAIN] 中文问句的典型覆盖率分布需在真实数据上另行测量后再决定是否重标定（本轮规格明确不许改阈值）。
- **48 项上限**：超过约 50 个汉字的长问句会被截断（计数上报），被截断的片段不参与 `MATCH` 与覆盖率分母。
- **CJK 标点被当作分隔符**：`，。！？` 等不属于汉字/假名/谚文，会把一个词项切成多段；因此「，」后仅 2 字的片段不进 `MATCH`（按规格「<3 的 CJK 段不参与」）。
- **引号短语不切分**：`"缓存 失效"` 保持字面整段（短语语义优先），可能与不加引号时的宽松匹配结果不同。
- **`resolved_mode` 对 KB/Archive**：这两个源没有索引路径，缺省模式下报 `legacy`；显式 exact/hybrid 时按请求报。
- **`INDEX_REQUIRED` 的退出码**：与既有 `degraded` 一致（有结果时 10、无结果时 4），不新增退出码。
- `cargo fmt`/`clippy` 仍不可用（工具链缺组件）。
