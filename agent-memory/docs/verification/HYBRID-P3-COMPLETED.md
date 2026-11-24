# 统一混合检索 P3：embedding 混合召回完成记录

状态：**P3 完成**（本文件是本阶段唯一 sentinel）。

真实数据：**否**。全部用例使用临时目录合成 fixture（合成 Codex JSONL 语料、合成 archive 运行时根、合成评估集）；未读取真实会话、真实知识库或生产 Archive，未对真实数据执行采集或 embedding，未重建 `assets/dist/agent-memory/`，未执行 git commit。开发集指标使用本机 **Ollama loopback** 上的 `qwen3-embedding:4b`（仅嵌入合成语料文本），以及 fastembed 离线缓存做算法对照；不涉及任何真实会话文本。

范围：Python `--embed` 辅助、fake/降级/离线分支；replay 派生向量代际；provider 级检索单元与去重；查询向量、权限前置过滤与语义主导（v4）融合；N4（索引 identity+access 准入）与 N2（回读上下文记录级 access）；`recall_eval.py`、评估集格式文档与 50 条开发集（第 2 轮补充 9 条主题外/零星重合负例）。

## 1. 实现

### 1.1 Python 辅助（`assets/scripts/recall_embedding.py`）

- `--embed` 从 stdin 读 `{schema_version, texts[], model{model_id, dimension, normalization, backend, model_path?}, ollama_url?}`，stdout 输出 `{ok, status, backend, model_id, dimension, native_dimension, normalization, generation, encoding:"f32", vectors[][]}`。
- `backend=python`：`interpreter.kb.command` 启动辅助进程；强制 `HF_HUB_OFFLINE=1`、`TRANSFORMERS_OFFLINE=1`、`HF_HUB_DISABLE_TELEMETRY=1`；`model_path` 必须是已存在目录（fastembed 缓存根或 `models--*` 目录），否则 `model_path_not_configured` / `model_unavailable`，**导入 fastembed 之前**就结束，绝不联网下载。fastembed 模型名由 `model_id` 解析（`bge-small-zh-v1.5` → `BAAI/bge-small-zh-v1.5`），缓存目录显式传入。
- `backend=ollama`：仅接受 loopback（`127.0.0.1`/`localhost`/`::1`，默认 `http://127.0.0.1:11434`），调用 `/api/embed`；非 loopback URL 在发请求前拒绝（`ollama_url_not_loopback`），无需本地 `model_path`。
- `backend=fake`：确定性字节 trigram/短文本 unigram FNV-1a 哈希向量（Rust `semantic::embed::fake_vector` 与 Python `fake_vector` 同一定义），无模型/网络/子进程；只在配置显式写 `fake` 时可用，输出 `backend=fake` 与 `backend=fake` 生成键，绝不作默认或回退。
- 维度：原生维度大于 `dimension` 时截断并按 `normalization` 重归一（实际维度写入生成键）；小于配置则 `dimension_mismatch`。请求上限 4096 条、单条 64 KiB。
- 降级码：`EMBEDDING_UNAVAILABLE`（exit 4）+ `lexical_continues:true`，覆盖 `model_path_not_configured` / `model_unavailable` / `model_not_supported_by_fastembed` / `backend_unavailable` / `ollama_unreachable` / `ollama_url_not_loopback` / `dimension_mismatch` / `ollama_invalid_response`。非法请求 exit 2 `INVALID_REQUEST`。

### 1.2 Rust 侧（`src/recall/semantic/{embed,units,index}.rs`，新增）

- `embed.rs`：`EmbeddingPlan`（冻结 spec + KB 解释器 + 辅助脚本 + loopback URL）。脚本只按运行二进制目录及其祖先查 `<ancestor>/scripts|assets/scripts/recall_embedding.py`，不读 HOME/环境变量默认路径。`embed_batch` 对 fake 本地计算，对 python/ollama 分批（64/批）调用子进程并校验 `backend`、向量条数与维度。所有失败返回 `EmbedError{reason, detail}`，由调用方转成显式降级。
- `units.rs`：`archive-unit-v3`。按 provider 投影：Codex `response_item.message`（content 字符串或块数组）、`function_call`/`custom_tool_call`/`local_shell_call`、`function_call_output`、`event_msg{agent_message,user_message,item_completed}`；Claude `user`/`assistant` 的 text/thinking/tool_use/tool_result、`summary`。`session_meta`/`turn_context`/`system`/`result`/`file-history-snapshot` 是元数据，**不建单元**；未知顶层形状才回退整条记录文本。超长文本按 `\n\n` → `\n` → UTF-8 边界切到 ≤2048 字节。单元身份 `sha256(归一化文本 | 单元类型 | archive-unit-v3)`；同一物理来源内去重，跨源不共享向量。
- `index.rs`：int4 标量量化（每单元 `f32 scale` + 每字节两个有符号 4-bit 值，取值 `-7..=7`；每单元 `4 + ceil(dimension/2)` 字节，dimension 512 时为 260 B）；`retrieval-index@3` 加载/校验（schema、`unitParser`、replay identity、生成键、`generator.quantization`、`access`、`units.bin`/`vectors.bin` SHA 与长度）；逐维反量化后按余弦打分；`rrf_score`/`RRF_K` 保留为第 3 轮辅助函数，不再参与 Archive 排序。加载从最新代际向前扫描，选**生成键匹配且量化方式为 int4** 的那一代（换模型后旧代仍可服务旧配置；int8 旧代因 `generator.quantization` 缺失/不符判为 `semantic_generation_mismatch`，不得混用）。

### 1.3 派生向量与代际（`src/replay.rs`）

- 检索单元来自**已提交记录**（`providers::archive::load_units` + 解码投影），因此每个单元保留 `recordSha256` 锚点，语义命中可直接生成 `archive-record-v1` ref。
- 代际选择：同一 parser identity **且**同一 generator key 才复用；否则新建 `g<N>`。切换模型/维度/归一化/后端不会覆盖旧向量。生成键沿用 P0 冻结格式 `embed-v1;model=…;dim=…;norm=…;backend=…`。
- replay 写 `derived/generations/g<N>/retrieval/index.json`（紧凑 JSON；`schema=agent-memory/retrieval-index@3`，`unitParser=archive-unit-v3`，`thresholdsVersion`，来源/会话映射、`access`、`unitTextBytes`、`generator{...,quantization:"int4"}`、`unitsTable{path,sha256,bytes,count,schema}`、`vectors{path,sha256,bytes,dimension,quantization:"int4",count}`）、`units.bin`（二进制单元表：定宽 SHA-256、类型/标签字符串表、行号、`turn`、`textBytes`、`access`）与 `vectors.bin`。文档无时间戳/采集 id/独立代际号；`generator.quantization` 进入代际元数据，量化方式变化会新建一代而不是复用旧向量。
- `replay_with_plan` 要求生成键与 plan 一致；`replay_with_generator` 对 embedding generator 无 plan 时显式 `ArchiveError::Embedding("embedding_plan_missing")`，不再写“声称 embedding 但无向量”的索引。显式失败会删除本次的 `rebuilding` fail-closed 标记（回滚到上一完整代）；硬中断仍保留标记。`ReplayReport.semantic` 报告 `available|degraded|disabled` + reason/backend/generationKey/dimension/units。
- CLI：`replay --config <recall.json>` 读取 `interpreter.embedding`；模型失败回退词面代际并在报告与 `anomalies` 写明降级；不给 `--config` 保持 P1 纯词面代际。

### 1.4 查询、权限与融合（`src/recall/providers/archive.rs`、`search.rs`）

- `ArchiveRequest` 新增 `query` 与 `embedding`；`search` 每次请求最多构造一个 plan。
- 准入顺序：① 读取派生代际并校验 identity/`unitParser`/`index.access`；② 逐记录对象/记录 access 与 session 过滤；③ 仅对通过者做单元投影与向量相似度；④ 词面与语义候选融合。被拒域的单元**不会进入候选生成**（拒绝计数与 P1 一致）。
- 融合（`archive-relevance-v4`）：候选生成与 P1 一致（词面不设覆盖率门槛、语义不设绝对下限，仅 top-N 截断）。语义可用时融合分 = `semantic_cosine + LEXICAL_BONUS × coverage`，`coverage = distinct_terms / query_terms`（无词面命中 0；仅词面命中用其记录余弦，无向量记 0），`LEXICAL_BONUS` 由开发集网格标定为 0.0；同轮多记录命中只加 `0.001×min(count-1,3)` 的 tie-break，只改分数、不改候选集合。排序稳定（score → 词面秩 → 语义秩 → sort_id）；`score_kind=semantic_plus_lexical_bonus`、`rank_basis=semantic_cosine_lexical_bonus`。
- 融合后门控（仅语义可用时；`Vec::retain` 保持保留项顺序）：命中保留当且仅当 (a) 该记录或所在轮次任一条余弦 ≥0.64；(b) 词面覆盖率 ≥0.34 且命中 ≥2 个不同查询词（单词查询命中即可）；(c) 所在轮次 ≥2 个不同记录余弦 ≥0.50 且轮次最高 ≥0.60。丢弃数量计入 `detail.archive.gated_out`。语义不可用（降级/`--mode none`/模型缺失）时完全使用 P1 词面行为，不做门控。
- `detail.archive` 记录 `thresholdsVersion`（语义配置时另含 `scoreFusion`/`lexicalBonus`）；`detail.semantic` 同时给出阈值版本与阈值数值（`semanticAcceptSimilarity` 0.64、`semanticSupportSimilarity` 0.50、`semanticSupportUnits` 2、`semanticTurnMaxSimilarity` 0.60、`lexicalAcceptCoverage` 0.34、`lexicalAcceptDistinct` 2、`turnAggregationBonus` 0.001）。
- `detail.semantic`：`{status, reason?, generationKey, generation?, dimension, units, candidates, fusion, scoreFusion, lexicalBonus, thresholdsVersion, thresholds}`；`DETAIL_KEYS` 由 10 项扩为 11 项（新增 `semantic`）。`RANK_BASIS_SEMANTIC_BONUS`/`SCORE_KIND_SEMANTIC_BONUS` 为 v4 合同常量；第 3 轮的 `RANK_BASIS_RRF`/`SCORE_KIND_RRF` 保留为历史常量。
- N4：`session_map` 同时校验索引 schema、`unitParser`、`index.identity` 与 `index.access`；identity/parser/schema 不匹配 → `identity_mismatch`，access 不含生效域 → `access_mismatch`（新状态）。两者都不使用映射或向量，不放宽。
- N2：`read_archive` 的前后上下文记录补记录级 `classify_access`，跨域邻记录省略，不再只靠对象级交集。

### 1.5 评估工具（`assets/scripts/recall_eval.py`，新增）

- 输入评估集目录；在系统临时根内：按 corpus domain 分组 `init` + `capture` → 每个 archive 根 `replay --config`（带 manifest 的 embedding）→ 每条查询 `memory_search` → 对返回的每个 ref（至多 top-5）`memory_read`。
- **轮次口径**：从语料 JSONL 推导轮次——每条用户消息开启新轮次；元数据（`session_meta`/`turn_context`/Claude `system` 等）归入其后第一个轮次；首条用户消息之前的记录归入轮次 0；Claude 仅含 `tool_result` 的 `type=user` 记录不开启新轮次。`recall@5` / `first_hit_accuracy` 按 `(文件, 轮次)` 比较；`contains` 只要求出现在锚点轮次内任一记录（标注校验），不要求命中记录本身包含。另输出严格行级诊断 `recall_at_5_line` / `first_hit_line_accuracy`（旧口径，仅供参考）。
- **回读口径**：对每条查询的全部返回 ref（至多 top-5）逐一 `memory_read`；成功 = 退出 0 + `ok=true` + 返回的 `range_sha256`（单段顶层或任一段）等于 ref 的 `fingerprint.range_sha256`。分母是所有被回读 ref 数（`readback_refs`），不是查询数。
- 输出：`stages`（init/capture/replay 退出码与 semantic 状态）、`metrics.{recall_at_5, first_hit_accuracy, recall_at_5_line, first_hit_line_accuracy, readback_success_rate, readback_refs, readback_refs_ok, negative_pass_rate}`、`latency_ms{cold_start,p95,p95_warm,max}`、`index_size{retrieval_bytes,corpus_bytes,ratio,unit_text_bytes,unit_ratio}`、分类分项、逐查询 `hits/ref/source_status/latency/match_ranks/match_ranks_line/expected_turns/readbacks`。
- `--mode none` 跑同语料词面基线；`--binary/--python/--out/--keep/--limit` 可控。退出码 0 报告、2 评估集非法、3 阶段失败。

### 1.6 评估集格式（`agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md`，新增）

`manifest.json`（schema/name/domain/embedding{python 或 ollama 或 fake, model_path/model_path_env}/python/corpora[]/queries）、Codex 与 Claude JSONL 最小字段、**轮次推导规则**（用户消息开启轮次、元数据归入下一轮、Claude `tool_result` 不开新轮、首条用户消息前为轮次 0）、`queries.jsonl`（`id/query/category/domain/limit/expect|expect_any`；锚点 `file/line/contains` 的轮次语义：`line` 定位期望轮次、`contains` 校验该轮内存在文本；负例 `empty/no_hits_from_source/source_status/code`）、指标口径（轮次指标判定、行级指标仅诊断、回读按全部 ref + `range_sha256`）、多 domain 语义、运行命令与“独立构造 held-out 集”的完整步骤 + 最小可运行示例。

### 1.7 开发集（`agent-memory/tests/fixtures/hybrid-eval-dev/`，新增）

6 个合成 Codex 语料（缓存、CI、数据库迁移、58 行长会话、词面锚点、`work` 私有语料）+ 50 条查询：exact 9、synonym 9、cross_lingual 8、long_session 6、model_missing 4、permission_negative 5、unrelated_negative 6、weak_overlap_negative 3。实现者构造，**不是 held-out**。

## 2. 设计决定（规格未逐字规定处）

1. **检索单元来自已提交记录，而不是 replay 后的消息模型**：消息模型没有 record sha，无法在保持 `archive-record-v1` 身份的同时把向量映射回原记录；直接从 `load_units` 投影可保证单元与回读 ref 一一对应，且索引与查询时用同一确定性函数重算 unit sha。
2. **元数据不建单元**：`session_meta`/`turn_context` 等 JSON 会显著污染语义候选（实测 meta 的余弦高于目标轮次），因此已知元数据类型显式跳过；未知形状仍回退整条文本，避免静默丢失证据。
3. **去重按物理来源**：§3.5 允许“来源族内去重”，但同条要求“跨源不共享向量”，因此去重键取 `(source, unitSha)`；两个来源捕获同一内容时各自持有向量。**[UNCERTAIN]** “来源族”是否指 provider 族（claude/codex）而非物理来源，规格未给定义；取物理来源是唯一同时满足“不跨源共享向量”的解释。
4. **语义主导融合（返工第 5 轮/v4）**：语义可用时融合分 = `semantic_cosine + LEXICAL_BONUS × coverage`，语义余弦是唯一排序源，词面覆盖率只是有界加成；`LEXICAL_BONUS` 仅在开发集网格上标定为 0.0。词面仍负责候选生成与门控（覆盖率规则），但不再以 1.5× 权重压过语义目标。同分按词面秩、语义秩、稳定 id 决胜，轮次 tie-break 量级不变。
5. **融合后门控（返工第 3 轮）**：绝对语义阈值无法分开正负例（正确目标可低至 0.53，无关 top-1 在 0.55–0.65），因此候选照常生成，最终结果由语义与词面两路信号共同门控：语义 ≥0.64、或词面覆盖率 ≥0.34 且 ≥2 词、或同轮双记录支持（≥0.50，最高 ≥0.60）。阈值版本 `archive-relevance-v4` 写入索引与状态；语义不可用时门控关闭，保持 P1 词面。
6. **int4 量化 + 维度选择**：int4 每字节两个有符号 4-bit 值，每单元 `4 + ceil(dimension/2)` 字节；推荐 dimension 512（`qwen3-embedding:4b` + l2 + ollama）时仍为 260 B/单元，与旧 int8/dim256 同体积，开发集索引比 1.70；实际维度进生成键，量化方式进 `generator.quantization` 并参与代际复用判定，删除重建一致。
7. **显式失败回滚 rebuilding 标记**：P1 用 marker 防止中断代被读；显式模型失败不是崩溃，若保留 marker 会让最新代永远不可读。因此显式错误路径删除标记并回退上一完整代，硬中断路径语义不变。
8. **重新生成 `retrieval-index@3` / `archive-unit-v3`**：单元规则与字段集变化，旧索引必须显式重建，不能按旧规则继续读；旧代仍可被同一生成键的查询读取，但 schema 不匹配时降级为 `semantic_index_*` 而不是猜。
9. **评估口径以 P0 冻结定义为准（返工后）**：`recall@5` / `first_hit_accuracy` 按 `(文件, 轮次)` 比较；轮次 = 一条用户消息 + 其后直到下一条用户消息前的全部助手/工具记录；行级指标只作诊断。`readback_success_rate` 对全部返回 ref 逐一 `memory_read`，要求退出 0、`ok=true`、`range_sha256` 与 ref 一致，分母是 ref 数。
10. **权限负例允许两种口径**：严格 `empty` 用于“整库无证据”的场景；`no_hits_from_source` 用于“被拒域不得出现、其他域可以正常返回”的场景，后者不会因语义噪声误判为权限泄露。

## 3. 测试

`cargo test --locked`：**523 passed / 1 ignored / 0 failed**（P2 基线 491/1，旧测试无回归，净增 32：集成 13 + 单元 19）。

### 3.1 新增 `tests/recall_archive_semantic.rs`（13 项，全部 fake 后端 + 临时目录）

| 用例 | 覆盖 |
|---|---|
| `vectors_are_written_into_the_derived_generation_with_identity_and_access` | 向量写入同代际 `vectors.bin`；index@3/unit parser/生成键/`generator.quantization=int4`/`vectors.quantization=int4`/backend=fake/access/单元 access；`recordSha256` 为 64 hex；`message:user`/`tool_call`/`tool_result` 单元存在；vectors SHA/长度/维度一致；检索 `rank_basis=semantic_cosine_lexical_bonus`、`score_kind=semantic_plus_lexical_bonus`、`detail.semantic.available`，命中回读原记录 |
| `changing_the_model_allocates_a_new_generation_and_keeps_the_old_one` | 模型 A → g1，模型 B → g2；g1 索引/向量字节不变；各自维度正确；配置 A 查 g1、配置 B 查 g2 |
| `quantization_change_allocates_a_new_generation_instead_of_reusing_int8` | 把既有代的 `retrieval.vectors.quantization` 改为 `int8`（模拟旧构建）后，同模型 replay 不复用而新建 g2；新代仍为 int4；检索使用 g2 |
| `int8_generation_metadata_is_reported_as_a_generation_mismatch` | 把 `generator.quantization`/`vectors.quantization` 改为 `int8` 后检索：语义 `degraded` + `semantic_generation_mismatch`，不按 int4 解码；词面路径继续返回结果 |
| `deleting_the_derived_tree_rebuilds_identical_vectors` | 删除 `derived/` 重建后索引与 fake 向量字节一致 |
| `denied_units_never_reach_the_vector_candidate_list` | 仅记录级 access 改为 `work`（对象仍 default）；该记录是最相似向量但不出现在结果，`semantic.candidates=0`，拒绝计数 +1；同时验证基线（语义-only 命中）可回读 |
| `missing_model_degrades_and_lexical_results_match_the_lexical_baseline` | 仅有词面代际 + 配置 python 缺失模型：`hit_id` 顺序与纯词面完全一致、`rank_basis=lexical_window`、`detail.semantic=degraded` + reason；`replay_with_plan` 对缺失脚本返回 `ArchiveError::Embedding`；随后词面 replay 自愈且结果不变 |
| `semantic_hits_fuse_with_lexical_ranks_and_read_back` | 词面+语义双命中融合分 `2/61`；无共享 CJK bigram 的语义-only 命中（`甲丙乙丁`）分 `1/61`，回读含目标文本 |
| `session_mapping_and_vectors_require_index_identity_and_access` | 改 `index.access=["work"]` → 映射 `access_mismatch` + 语义 `semantic_access_restricted`；改 index identity → `identity_mismatch` + 语义降级（词面仍可用）；还原后恢复 available（N4） |
| `unrelated_queries_return_empty_and_one_incidental_bigram_is_not_a_hit` | 语义模式：语料外主题返回空、exit 0、`gated_out≥1`；纯词面模式保持 P1 行为（同一查询命中偶然二元组）；阈值版本出现在 `detail.archive`；相关查询仍命中；门控前后分页顺序一致 |
| `semantic_target_stays_above_a_high_coverage_lexical_neighbour` | 语义正确目标优先于同主题但弱语义的高覆盖词面近邻；top-1 仍是 `semantic_cosine_lexical_bonus` 且可回读 |
| `high_coverage_lexical_candidate_wins_a_semantic_tie` | 两条记录共享相同文本单元的语义余弦时，词面覆盖更高者（`版本号`）赢得同分决胜并回读 |
| `turn_aggregation_lifts_a_multi_unit_turn_and_returns_a_readable_ref` | 同轮两条记录命中时该轮代表排第一、score 含轮次加成；返回 ref 可回读；`detail.semantic.thresholdsVersion` 与阈值数值存在 |

### 3.2 新增单元测试（19 项）

- `semantic/embed.rs`（5）：fake 确定性/归一化/不同文本不同向量；fake 无脚本无模型；缺脚本显式降级；生成键；测试二进制能定位辅助脚本。
- `semantic/units.rs`（5）：Codex 消息/调用/结果分单元且 meta 跳过；Claude text/tool_use/tool_result 投影且不含 uuid；超长结构切分在预算内；单元身份依赖文本/类型/parser；非法 JSON 回退整条记录。
- `semantic/index.rs`（6）：int4 往返保持方向；int4 打包/解包覆盖 `-7..=7` 边界与负值二补码；奇数维度补零且坏长度拒绝；RRF 双列表优先；量化确定性；二进制单元表往返（含类型/access/行号/turn）与坏哈希/截断拒绝。
- `providers/archive.rs`（2）：词面质量门槛（单词查询豁免、单个二元组拒绝、覆盖率/强命中边界）；`semantic_cosine_dominates_the_bounded_lexical_bonus`（0.70 语义目标不会被满覆盖词面候选压下；同语义分时覆盖率只加有界 bonus；轮次 tie-break <0.01）。
- `semantic/units.rs`（+1）：用户轮次识别（Codex user、Claude 纯 tool_result 不开启新轮、元数据不开启）。
- Python：`assets/scripts/test_recall_embedding.py` **21 项**（原 9 项扩为 21）：fake 确定性/归一化/CLI、fake 不导入模型栈、缺模型不导入 fastembed、python 后端强制离线 + 截断重归一、原生维度不足降级、ollama 非 loopback 拒绝、非法请求、CLI 降级 exit 4、坏 JSON exit 2、probe 原有用例。`test_recall_onboarding.py` 仍 24 项；embedding 默认批量超时 120 s（probe 仍 2 s），避免冷模型首次批调用被误判为不可达。
- Python（返工新增）：`assets/scripts/test_recall_eval.py` **12 项**（无模型、合成 JSONL）：Codex/Claude 轮次推导、元数据归入下一轮、首条用户消息前为轮次 0、Claude `tool_result` 不开新轮、同轮跨行匹配、`contains` 按轮校验、严格行级诊断、`expect_any`、`range_sha256` 回读契约（顶层/分段/失败条件）、语料 basename 去重。

### 3.3 修改的既有断言（逐条原因）

1. `tests/recall_archive.rs::retrieval_index_carries_generator_identity_and_rebuilds_identically`：改用 fake spec（dim 64）+ `replay_with_plan`，并新增“embedding generator 无 plan 必须被拒绝”“向量字节重建一致”。原因：P3 规定 embedding 代际必须带向量，P1 的“只有身份无向量”不再合法；原断言的身份/access/重建意图完整保留。
2. `tests/recall_contract.rs::frozen_tables_have_no_duplicates_and_consistent_sets`：`DETAIL_KEYS` 10 → 11 并断言含 `semantic`。原因：P3 要求在 `source_status[].detail` 报告语义状态，合同键表必须同步。
3. `assets/scripts/test_recall_embedding.py::test_cli_exit_codes`：`--embed` 从“P0 未实现 exit 3”改为“缺 stdin 请求 → exit 2 INVALID_REQUEST”，并拆出 fake/降级 CLI 用例。原因：`--embed` 已实现，P0 的 `EMBEDDING_NOT_IMPLEMENTED` 断言不再成立。

## 4. 开发集指标（v4 语义主导融合，参考，不作为验收）

环境：Ollama loopback `qwen3-embedding:4b`，`dimension=512`（原生 2560 截断 + L2），`backend=ollama`，int4 存储，`LEXICAL_BONUS=0.0`；工具 `recall_eval.py`；**50 条查询**（37 正/权限 + 6 主题外严格 empty + 3 零星字面重合负例）、6 个语料、2 个 domain 根。轮次口径（P0 判定）；回读校验全部返回 ref 的 `range_sha256`。

| 指标 | 语义（qwen3 dim512 int4，v4） | 语义（行级诊断） | 词面基线（`--mode none`） |
|---|---|---|---|
| recall@5 | **1.000**（37/37） | 1.000（37/37） | 0.865（32/37） |
| 首命中轮次正确率 | **0.865**（32/37） | 0.703（26/37） | 0.703（26/37） |
| 回读成功率 | **1.000**（133/133 ref） | 1.000 | 1.000（133/133 ref） |
| 负例通过率 | **1.000**（13/13） | — | 0.615（8/13；P1 词面会命中部分主题外查询，符合“无门控”的设计） |
| p95 延迟 | **172.9 ms**（warm 169.1 ms） | — | 14.2 ms |
| 冷启动 | 172.9 ms | — | 13.9 ms |
| 索引体积比（retrieval/corpus） | **1.703**（32,949 / 19,344 B） | — | 0.556（10,762 / 19,344 B） |

分类（语义轮次 / 行级 / 词面轮次）：

| 类别 | 查询 | 语义 recall@5 | 语义首命中 | 行级 recall@5 | 行级首命中 | 词面 recall@5 | 词面首命中 |
|---|---|---|---|---|---|---|---|
| exact | 9 | 1.000 | 0.889 | 1.000 | 0.778 | 1.000 | 1.000 |
| synonym | 9 | 1.000 | 0.889 | 1.000 | 0.444 | 0.778 | 0.556 |
| cross_lingual | 8 | 1.000 | 0.625 | 1.000 | 0.500 | 0.625 | 0.125 |
| long_session | 6 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 |
| model_missing（词面锚点） | 4 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 |
| permission_negative | 5 | recall 1.000 / 负例 1.000 | — | 1.000 | — | 1.000 / 1.000 | — |
| unrelated_negative（严格 empty） | 6 | 负例 **1.000** | — | — | — | 0.500 | — |
| weak_overlap_negative | 3 | 负例 **1.000** | — | — | — | 0.333 | — |

解读：v4 融合使语义 recall@5 达到 1.000（§12 为 0.946），cross_lingual 从 0.875/0.500 升到 1.000/0.625；首命中保持 0.865（32/37），剩余错误为 q02/q10/q19/q20/q25，主要是同主题相邻轮次或跨会话 top-1，留给 P4 上下文组装。行级首命中 0.703 低于轮次口径，说明命中常落在期望轮次的相邻行，属诊断差异。索引 32,949 B（比 §12 的 32,819 B 多 130 B，来自 `scoreFusion`/`lexicalBonus` 元数据），体积比 1.703，仍在 3.0 预算内；词面基线与索引字节保持 P1 不变（0.556）。**该集合由实现者构造，不是 held-out，不得用于验收结论。**

## 5. 命令与退出码

| 命令（工作区 `C:\wt\hybrid-retrieval-20261028`） | 结果 |
|---|---|
| `cargo test --locked`（`agent-memory`） | **523 passed / 1 ignored / 0 failed** |
| `cargo test --locked --test recall_archive_semantic` | 13 passed |
| `python -m unittest test_recall_embedding`（`assets/scripts`） | 21 项通过 |
| `python -m unittest test_recall_eval`（`assets/scripts`） | 12 项通过 |
| `python -m unittest test_recall_onboarding test_recall_embedding test_recall_eval` | 57 项通过 |
| `python assets\scripts\recall_eval.py --set agent-memory\tests\fixtures\hybrid-eval-dev --out <temp>\semantic.json` | **exit 0**；replay 两根均 `semantic.available`，generationKey `embed-v1;model=qwen3-embedding:4b;dim=512;norm=l2;backend=ollama` |
| 同上 `--mode none --out <temp>\lexical.json` | **exit 0**（词面基线） |
| `powershell -File assets\scripts\check-docs.ps1` | **exit 0**（6 项、57 个 Markdown） |
| `git diff --check` | exit 0 |

未运行 `cargo fmt`：钉定工具链未安装 rustfmt（P2 已记录同一环境事实）。
未执行 61/62/90，未重建 `assets/dist/agent-memory/`：源码树与二进制已变化，分发物由 orchestrator 重建后复跑。

## 6. 限制与已知边界

- **无真实数据**：所有指标来自合成语料；没有真实查询分布、生产规模单元数或真实并发延迟。Ollama 延迟是本机 loopback 实测，不代表其他机器。
- **权限分类按单 domain 根**：一次 replay 使用一个访问域；不同 domain 的语料通过不同 archive 根参与评估。真实单根混合标签仍按 P1 记录级谓词处理。
- **相关性门控与融合权重是模型/开发集相关的**：`archive-relevance-v4` 的门控常量仍在 qwen3-embedding:4b（dim 256）上标定且未重标定；`LEXICAL_BONUS=0.0` 只在开发集 dim512 网格上选定（§13），不是 held-out 调参结论。bge-small-zh 分布不同，换模型必须重测（常量在 `semantic/relevance.rs`）。语义不可用时门控关闭，词面行为为 P1。
- **截断维度会改变语义**：推荐 `dimension=512`（qwen3-embedding:4b 原生 2560 的前缀截断 + 重归一）；对非 MRL 模型可能损失召回，实际维度已写入生成键，可重放比较。
- **轮次正确不等于整轮组装**：评估已按 `(文件, 轮次)` 判定首命中，但检索返回的仍是单条记录（消息/工具调用/结果）；命中同轮助手回答可判对，返回内容本身尚未组装整轮，留给 P4。
- **int4 量化误差**：每维只有 15 个有符号档位，比 int8 粗；orchestrator 的纯语义对照在 dim512 下未测到精度损失，但端到端近邻排序仍可能翻转（§12/§13）。每单元 260 B（dim512），开发集索引比 1.70、`unit_ratio` 4.26；预算仍按“源文本”口径计，不按被嵌入文本自证。
- **fake 后端不是语义模型**：只用于离线回归；其向量仅字符 n-gram 哈希，不能代表真实语义质量。
- **辅助进程无显式超时**：`Command::wait_with_output` 没有超时，慢模型会拖长单次检索；`budget.max_elapsed_ms_per_source` 只覆盖源内扫描，不中断子进程。
- **脚本定位基于二进制祖先目录**：分发时必须保证 `assets/scripts/recall_embedding.py` 在二进制祖先的 `scripts/` 或 `assets/scripts/` 下；找不到即 `script_unavailable` 降级，不按 HOME 猜测。
- **评估工具默认选开发构建**：优先 `target/{release,debug}`，其次 `assets/dist`；评估已安装分发物需显式 `--binary`。
- **`read_current_index` 供会话映射使用，向量加载独立扫描代际**：两者都校验 identity/access，但会话映射取最新代际（映射与模型无关），语义加载取生成键匹配代际。
- **未验证真实 Ollama 断线/超时切换**：仅覆盖非 loopback 拒绝与不可达 URL 的 Python 层；Rust 侧把 helper 非零/坏 JSON 映射为降级。

## 7. 未做

- 未实现 P4 图关联与跨轮上下文组装（首命中偏低的主因留给 P4）。
- 未改 MCP 只读语义、事件存储或遥测路径；检索仍不产生任何事件/副作用。
- 未重建 KB `.meta/embeddings.sqlite`，KB/device/OpenCode/Codex 活源行为不变。
- 未重建 `assets/dist/agent-memory/`、未改 `build-manifest.json`、未执行 git commit（提交由 orchestrator 执行）。
- 未运行 61/62/90，未读取任何真实会话、知识库或生产 Archive。
- 未交付 held-out 集；开发集仅供实现者自测与指标对照。
- 未按 dim512 重新标定 `archive-relevance-v4` 的门控常量，也未重跑 held-out；`LEXICAL_BONUS` 仅在开发集网格上选定（§13），不代表 held-out 最优。

## 8. 修改文件

新增：`agent-memory/src/recall/semantic/{embed,index,units,relevance}.rs`、`agent-memory/tests/recall_archive_semantic.rs`、`agent-memory/tests/fixtures/hybrid-eval-dev/`（6 语料 + `queries.jsonl` + `manifest.json`，manifest 为 qwen3-embedding:4b / dim512 / l2 / ollama）、`agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md`（示例 dim512、int4 体积口径）、`assets/scripts/recall_eval.py`。

修改：`agent-memory/src/replay.rs`、`src/recall/providers/archive.rs`、`src/recall/search.rs`、`src/recall/read.rs`、`src/recall/contract.rs`、`src/recall/semantic/{mod,model,degrade}.rs`、`src/archive/mod.rs`、`src/bin/agent-memory.rs`、`tests/{recall_archive,recall_contract}.rs`、`assets/scripts/{recall_embedding,test_recall_embedding}.py`、`agent-memory/README.md`、`assets/references/agent-memory-archive.md`、`agent-memory/docs/verification/HYBRID-P3-COMPLETED.md`、`CHANGELOG.md`。

## 9. 返工记录（orchestrator 验收意见）

原因：初版评估口径与 `HYBRID-P0-METRICS.md` 冻结定义不一致——把“一行 = 一个轮次”，导致命中同轮助手回答被判错；回读只测 top-1 且未校验 `range_sha256`。本次**只改评估工具、格式文档与报告**，Rust 检索实现与配置/合同未改。

改动：

1. **轮次推导**（`recall_eval.py`）：从语料 JSONL 推导行→轮次映射。用户消息开启新轮次；`session_meta`/`turn_context`/Claude `system` 等元数据归入其后第一个轮次；首条用户消息之前的记录归入轮次 0；Claude 仅含 `tool_result` 的 `type=user` 记录属于当前轮次，不开启新轮。语料 basename 必须唯一（重复直接报评估集非法）。
2. **匹配**：`recall@5`/`first_hit_accuracy` 比较 `(文件, 轮次)`；`contains` 改为在锚点轮次内任一记录中校验（标注正确性），不再要求命中记录自身包含；`expect_any` 保留，但只用于“多个不同轮次均可接受”，同轮多行不再需要。新增严格行级诊断 `recall_at_5_line`/`first_hit_line_accuracy`（旧口径），不参与阈值。
3. **回读**：对每条查询返回的全部 ref（至多 top-5）逐一 `memory_read`；成功 = 退出 0 + `ok=true` + `range_sha256`（顶层或任一段）与 ref 的 `fingerprint.range_sha256` 一致；分母为 `readback_refs`（所有被回读 ref），并输出 `readback_refs_ok`。
4. **报告**：`per_query` 增加 `match_ranks`（轮次）/`match_ranks_line`/`expected_turns`/`readbacks[]`（逐 ref 退出码、`ok`、`range_sha256_matches`）；分类分项增加行级指标与 `refs_read/refs_ok`。
5. **文档**：`HYBRID-EVAL-FORMAT.md` 重写 §2.3/§3/§4 的轮次、锚点与指标定义，保持“只读该文档即可独立造出可运行 held-out 集”。
6. **测试**：新增 `assets/scripts/test_recall_eval.py` 12 项（无模型、合成 JSONL），覆盖 Codex/Claude 轮次、元数据、轮次 0、tool_result、同轮跨行、严格行级、`expect_any`、`range_sha256` 回读契约与 basename 唯一性。

复跑（同一开发集，qwen3-embedding:4b dim256）：轮次 recall@5 **1.000**、首命中 **0.838**、行级诊断 1.000/0.703、回读 **1.000（148/148 ref）**、负例 1.000、p95 **152.7 ms**、索引比 **2.640**；词面基线轮次 recall@5 0.865、首命中 0.703、回读 1.000（120/120）。详见 §10.2。

证据：`cargo test --locked` 仍 **511 passed / 1 ignored / 0 failed**（Rust 未改）；Python 21 + 24 + 12 = **57 项通过**；`check-docs.ps1` exit 0；`git diff --check` exit 0。未 commit、未读真实数据、未重建分发物。

## 10. 返工第 2 轮（独立 held-out 验收；门槛已被 §11 取代，保留为历史）

外部 held-out（49 查询 / 7 语料 / ~23.5 KB，qwen3 dim256）暴露：recall@5 0.977、首命中 0.860（<0.90）、回读 1.000、p95 160 ms、索引比 3.33（>3.0）、负例 0.5（3 条严格 empty 失败）；词面基线 recall 0.837 / 首命中 0.814 / 体积 1.88 / 负例 0.5。本轮只改 Archive provider 与派生 artifact；活源词面行为未动。

### 10.1 改动

1. **相关性下限（`semantic/relevance.rs`，`archive-relevance-v2`）**
   - 语义：候选收集下限 0.50；轮次接受 = 最高余弦 ≥0.64，或同轮 ≥2 个不同单元各 ≥0.50 且最高 ≥0.60。低于下限的单元不进入融合。
   - 词面：≥2 个查询词且覆盖率 ≥0.34（单词查询豁免）；覆盖率 ≥0.75 记为强词面命中。单个偶然中文二元组/JSON 结构词不再是命中，计入 `detail.archive.weak_matches`。
   - 标定：开发集 qwen3 dim256 上相关查询 top-1 ≥0.615、无关行政查询 top-1 ≤0.598（单元级可读材料 ≤0.593）。取 0.64 作为单候选接受线、0.60 作为双支持最低线，给 0.598 噪声留出间隔；0.50 支持线让“问题+回答”同轮双命中可被接受。
   - 版本写入 `index.json`（`thresholdsVersion`）、`detail.archive.thresholdsVersion`、`detail.semantic.thresholdsVersion` 与具体阈值数值。
2. **强词面保底**：覆盖率 ≥0.75 的词面命中在融合排序中整体高于非强命中，避免“纯词面本已第 1、融合后被弱语义挤到第 2–4”。
3. **轮次聚合**：检索时按“用户消息开启轮次”计算每条记录的 `turn`（与评估口径一致）；同一 `(source, turn)` 有多个不同记录命中时，给该轮候选加 `0.001 × min(count-1,3)`，该轮最佳记录仍是返回的 `archive-record-v1` ref（可回读）。加成刻意小于一个 RRF 名次差（~0.0003–0.001），只用于打平/近似平局，不覆盖明显更优的语义命中。
4. **体积压缩**：派生单元表改为 `units.bin` 二进制（定宽 SHA-256、类型/标签字符串表、行号、`turn`、`textBytes`、access），`index.json` 只保留身份、来源/会话映射、access、artifact 哈希与 `unitTextBytes`；向量仍 int8（第 4 轮起改为 int4，见 §12）。`index.json`/`units.bin`/`vectors.bin` 全部在 `retrieval/` 内计数，重建字节一致（含二进制表）。schema 升 `retrieval-index@3` / `archive-unit-v3`。
5. **评估集**：新增 6 条主题完全不在语料中的严格 empty 负例与 3 条“仅零星字面重合”负例（共 50 查询、13 条负例）。工具/格式口径沿用第 1 轮返工（轮次、全 ref 回读 + range_sha）。

### 10.2 前后指标（开发集，轮次口径）

| 指标 | 第 1 轮返工后（41 查询，无主题外负例） | 第 2 轮后（50 查询，含 9 条新负例） |
|---|---|---|
| 语义 recall@5 | 1.000 | **1.000** |
| 语义首命中 | 0.838 | **0.811** |
| 语义回读 | 1.000 | **1.000（116/116）** |
| 负例通过 | 1.000（4/4 权限） | **1.000（13/13）** |
| p95 | 152.7 ms | 165.3 ms |
| 索引体积比 | 2.640 | **1.694** |
| 词面 recall@5 | 0.865 | 0.622 |
| 词面首命中 | 0.703 | 0.622 |
| 词面索引比 | 1.502 | 0.556 |

首命中下降是门槛收紧的副作用（部分偶然 top-1 被过滤/重排）；分类上 exact/long/model_missing 仍 1.000，synonym 0.667、cross_lingual 0.500，剩余错误为同主题相邻轮次/跨会话。外部 held-out 未重跑（不可见），是否达到 0.90 由 orchestrator 复测；本轮直接针对其两类失败（无关查询空结果、强词面被语义压低）与体积超限。

### 10.3 测试与证据

- `cargo test --locked`：**517 passed / 1 ignored / 0 failed**（第 1 轮 511/1；新增 6：集成 3 + 单元 3，另扩展重建断言到 `units.bin`）。
- 新增集成：无关查询空结果/单偶然二元组、强词面不被弱语义压下、轮次聚合返回可回读 ref。
- 新增单元：词面质量门槛、二进制单元表往返/坏输入、用户轮次识别。
- Python 21 + 24 + 12 = 57 项通过；`check-docs.ps1` exit 0；`git diff --check` exit 0。
- 未 commit、未读真实数据、未重建分发物。

## 11. 返工第 3 轮（orchestrator 设计裁决）

背景：第 2 轮在 held-out 上退步——语义首命中 0.860→0.744、recall@5 0.977→0.907、纯词面 recall@5 跌到 0.395。原因是把语义绝对下限放在候选生成、把词面覆盖率门槛放在候选生成：正确同义/跨语言目标余弦可低至 0.53，而无关查询 top-1 在 0.55–0.65，绝对阈值无法分开；长口语化查询的 CJK 二元组覆盖率天然偏低。本轮按裁决恢复候选生成，改为融合后由两路信号共同门控。

### 11.1 改动

1. **候选生成恢复 P1/第 1 轮**：删除语义候选绝对下限与词面覆盖率门槛；语义候选只保留 top-200 截断。`detail.archive.weak_matches` 移除。
2. **融合改为覆盖率加权 RRF**：词面贡献 `(0.5 + coverage)/(60+lexical_rank+1)`，语义 `1/(60+semantic_rank+1)`；删除“强词面整体置顶”。同轮多记录仍加 `0.001×min(count-1,3)` 的 tie-break，只影响分数。
3. **融合后门控**（仅语义可用）：保留命中当且仅当 (a) 该记录/轮次余弦 ≥0.64；或 (b) 词面覆盖率 ≥0.34 且 ≥2 个不同查询词（单词查询命中即可）；或 (c) 同轮 ≥2 个记录余弦 ≥0.50 且轮次最高 ≥0.60。`Vec::retain` 保持保留项顺序；丢弃数写入 `detail.archive.gated_out`。
4. **降级路径**：语义不可用（模型缺失/index degraded/`--mode none`）时跳过门控，完全使用 P1 词面，状态如实 `degraded`。
5. **阈值版本升 `archive-relevance-v3`**，写入 `index.json`、`detail.archive`、`detail.semantic`（含具体数值）。命名常量在 `semantic/relevance.rs`。
6. **保留第 2 轮体积优化**：`units.bin` 二进制单元表与 `retrieval-index@3`/`archive-unit-v3` 不变。

标定依据（开发集 qwen3 dim256）：正确释义/跨语言目标可低至 0.53，无关查询 top-1 落在 0.55–0.65，单一绝对阈值必然误伤；因此 0.64 只作为门控的“强语义”分支，弱语义必须由词面覆盖率或同轮双证据支持。0.34/2 词来自 P1 分词下“两个以上不同术语”与单偶然二元组的边界；0.50/0.60 的双记录支持用于保留“问题+回答”同轮命中。

### 11.2 三列对比（开发集，轮次口径）

| 指标 | 第 1 轮（41 查询） | 第 2 轮（50 查询） | 第 3 轮（50 查询） |
|---|---|---|---|
| 语义 recall@5 | 1.000 | 1.000 | **0.973** |
| 语义首命中 | 0.838 | 0.811 | **0.892** |
| 语义回读 | 1.000（148 ref） | 1.000（116 ref） | **1.000（147 ref）** |
| 语义负例 | 1.000（4/4） | 1.000（13/13） | **1.000（13/13）** |
| 语义 p95 | 152.7 ms | 165.3 ms | 166.2 ms |
| 语义索引比 | 2.640 | 1.694 | **1.694** |
| 词面 recall@5 | 0.865 | 0.622 | **0.865** |
| 词面首命中 | 0.703 | 0.622 | **0.703** |
| 词面负例 | 1.000（4/4） | 1.000（13/13） | 0.615（8/13；门控关闭，P1 会命中部分主题外查询） |
| 词面索引比 | 1.502 | 0.556 | **0.556** |

第 3 轮语义 1 条 recall 缺口是 q10（正确轮次被高覆盖词面的其它会话记录挤出 top-5），首命中 4 条错误为同主题相邻轮次/跨会话；纯词面恢复到第 1 轮水平，证明降级路径未受门控影响。第 2 轮数值与 held-out 退步一致，保留作对照。

### 11.3 测试与证据

- `cargo test --locked`：**517 passed / 1 ignored / 0 failed**（第 2 轮 517/1；第 1 轮 511/1）。集成测试 10 项、单元 16 项；新增/调整覆盖：无关查询语义模式返回空且 `gated_out≥1`、纯词面模式保持 P1 命中、覆盖率加权下高覆盖词面不被弱语义压下、轮次聚合代表可回读、门控前后顺序/分页一致、`units.bin` 重建字节一致、`archive-relevance-v3` 进入状态与索引。
- Python 21 + 24 + 12 = 57 项通过；`check-docs.ps1` exit 0；`git diff --check` exit 0。
- 未 commit、未读真实数据、未重建分发物；held-out 未重跑（不可见），由 orchestrator 复测。

## 12. 返工第 4 轮（int4 量化与 dimension 512，v3 融合对照）

> 本节记录 v3 融合下的 int4/dim512 返工；融合已在 §13 改为 v4 语义主导，以下指标与「阈值常量不变」的描述保留为对照。

范围：只改向量存储量化与推荐维度；第 3 轮后的候选生成、覆盖率加权 RRF、融合后门控、阈值常量、权限/回读路径全部保持不变。未 commit、未读真实数据、未重建分发物。

### 12.1 改动

1. **int4 标量量化（`semantic/index.rs`）**：每单元 `f32 scale` + `ceil(dimension/2)` 字节；每个字节低 nibble 存偶数维、高 nibble 存奇数维，值为 `-7..=7` 的二补码；`scale = max(|v|)/7`（零向量取 1.0）。反量化按 nibble 符号扩展，余弦用反量化后的向量计算（等价于整数点积 × scale），零向量仍打 0 分。`VECTORS_QUANTIZATION` 改为 `int4`，新增 `quantized_row_bytes(dimension) = 4 + ceil(dimension/2)`。int4 是**唯一写入格式**，int8 写入路径已删除。
2. **量化进入派生身份**：`index.json` 的 `generator.quantization` 与 `vectors.quantization` 都写 `int4`。replay 复用代际时除 parser identity + 生成键外，还要求既有代的 `retrieval.vectors.quantization == int4`；量化变化直接新建 `g<N>`，不覆盖旧代。加载端把缺失 `generator.quantization`（旧 int8 代）或值不符的代判为 `semantic_generation_mismatch` 并继续向前扫描；`load_document` 仍保留 `vectors.quantization` 校验，旧代不会被 int4 解码器误读。
3. **推荐维度改为 512**：开发集 manifest、`HYBRID-EVAL-FORMAT.md` 两处示例、`README.md` 推荐配置与 `agent-memory-archive.md` 配置统一为 `qwen3-embedding:4b` / `dimension: 512` / `l2` / `ollama`。文档写明 int4 体积为 `4 + ceil(dimension/2)` 字节/单元（dimension 512 = 260 B，与旧 int8/dim256 相同）。
4. **测试**：`semantic/index.rs` 原 int8 往返改为 int4，并新增打包/解包 `-7..=7` 边界与负值二补码、奇数维度补零/长度拒绝（2 项）；`tests/recall_archive_semantic.rs` 新增量化变化强制新代（1 项）与 int8 旧代加载报 `semantic_generation_mismatch`（1 项）；既有向量字节长度断言改用 `quantized_row_bytes`。

### 12.2 对照依据（orchestrator 纯语义排序实验，qwen3-embedding:4b）

| 配置 | 首命中 | recall@5 | 字节/单元 |
|---|---|---|---|
| dim 256 int8（第 3 轮） | 0.86 | 0.953 | 260 |
| dim 512 int8 | 0.907 | 1.0 | 516（超 3.0 体积预算） |
| dim 512 int4（本轮） | 0.907 | 1.0 | 260 |
| dim 768 int4 | 与 512 相同 | 1.0 | 388 |

结论：维度截断到 256 是首命中瓶颈；int4 在 512 维下无可测精度损失，且体积与旧配置相同。该实验只比较纯语义排序，不含词面融合与融合后门控。

### 12.3 开发集新指标（第 3 轮 vs 第 4 轮，轮次口径）

| 指标 | 第 3 轮（dim256 int8） | 第 4 轮（dim512 int4） |
|---|---|---|
| 语义 recall@5 | 0.973（36/37） | **0.946（35/37）** |
| 语义首命中 | 0.892（33/37） | **0.865（32/37）** |
| 语义行级 recall@5 / 首命中 | 0.946 / 0.784 | **0.946 / 0.757** |
| 语义回读 | 1.000（147 ref） | **1.000（133 ref）** |
| 语义负例 | 1.000（13/13） | **1.000（13/13）** |
| 语义 p95 / 冷启动 | 166.2 / 168.4 ms | **166.1 / 198.2 ms** |
| 索引体积比 | 1.694（32,775 B） | **1.697（32,819 B）** |
| 向量字节/单元 | 260（4 + 256） | **260（4 + 256）** |
| 词面 recall@5 / 首命中 | 0.865 / 0.703 | **0.865 / 0.703**（未变） |
| 词面索引比 | 0.556 | **0.556**（未变） |

体积明细：`vectors.bin` 20,800 B = 80 单元 × 260 B，`units.bin` 7,760 B，`index.json` 增加 44 B（维度数字 256→512），retrieval 合计 32,819 B；`unit_text_bytes` 7,726，`unit_ratio` 4.248。索引比仍在 3.0 预算内，且与第 3 轮基本同体积。

分类差异：exact 1.000/1.000、synonym 0.889/0.889、long_session 1.000/1.000、model_missing 1.000/1.000 均未变；cross_lingual 由 1.000/0.625 变为 0.875/0.500。q10 仍是原有 recall 缺口；q19 新增 recall 缺口；首命中错误为 q10/q19/q24/q25/q26。原因是 `archive-relevance-v3` 的融合/门控常量仍在 dim256 上标定、本轮按范围要求未改，纯语义 512 维的收益没有完整传导到端到端排序；是否按 dim512 重新标定由下一轮裁决，本轮不擅自改阈值。

### 12.4 测试与证据

- `cargo test --locked`：**521 passed / 1 ignored / 0 failed**（第 3 轮 517/1；新增 4：集成 2 + 单元 2）。
- `cargo test --locked --test recall_archive_semantic`：**12 passed**（含量化变化强制新代、int8 旧代报 generation mismatch）。
- Python：`test_recall_embedding` 21 + `test_recall_onboarding` 24 + `test_recall_eval` 12 = **57 项通过**（生成键格式未变，helper 仍只返回 f32）。
- 开发集：`recall_eval.py` 语义模式 **exit 0**、两根 `semantic.available`、generationKey `embed-v1;model=qwen3-embedding:4b;dim=512;norm=l2;backend=ollama`；`--mode none` **exit 0**。
- 未 commit、未读真实数据、未重建分发物；held-out 未重跑（不可见）。

## 13. 融合改为语义主导（v4）

裁决：orchestrator 在独立集上的轮次口径对照显示，纯语义 top-1 首命中约 0.91–0.98，而覆盖率加权 RRF 的端到端首命中只有约 0.75；失败集中在跨语言/同义查询——词面二元组命中同语言但错误的记录，以最高 1.5 倍权重压过语义正确目标。因此语义可用时词面改为有界加成，不再与语义平权。

### 13.1 改动

1. **排序分**：语义可用时 `score = semantic_cosine + LEXICAL_BONUS × coverage`（`coverage = distinct_terms / query_terms`，无词面命中记 0；仅词面命中用其所在记录的语义余弦，无向量记 0），按降序排列；同分仍按词面秩 → 语义秩 → 稳定 id 决胜。轮次聚合 `0.001 × min(count-1,3)` 保持不变，只作用于融合分，量级远小于语义差。
2. **门控不变**：融合后门控的规则与常量（0.64 / 0.50 / 2 / 0.60 / 0.34 / 2）完全未改；`retain` 保持保留项相对顺序，`gated_out` 计数不变。
3. **降级路径不变**：语义不可用（降级/模型缺失/`--mode none`）时不做门控、不进入新融合，继续使用 P1 词面 `lexical_window`，索引字节与第 3 轮一致（0.556，10,762 B）。
4. **元数据**：`ARCHIVE_RELEVANCE_VERSION = archive-relevance-v4`；`SCORE_FUSION = semantic_cosine_lexical_bonus`；`score_kind = semantic_plus_lexical_bonus`、`rank_basis = semantic_cosine_lexical_bonus`。`scoreFusion`/`lexicalBonus` 写入 embedding 代际的 `index.json` 以及 `detail.archive`/`detail.semantic`；词面代际不写这两个字段，保持 P1 字节不变。
5. **测试**：更新融合/门控/分页用例到新 rank_basis 与 score_kind；新增「同语义余弦时高词面覆盖赢得平局」集成用例与 `semantic_cosine_dominates_the_bounded_lexical_bonus` 单元用例（0.70 语义目标不会被满覆盖词面候选压下；同语义分时覆盖率只加有界 bonus；轮次 tie-break <0.01）。

### 13.2 `LEXICAL_BONUS` 开发集网格（qwen3 dim512，轮次口径）

仅使用开发集 `agent-memory/tests/fixtures/hybrid-eval-dev/`；每格重建二进制并跑 `recall_eval.py` 一次。

| LEXICAL_BONUS | recall@5 | 首命中 | 行级首命中 | 回读 | 负例 | p95 (ms) | 索引比 |
|---|---|---|---|---|---|---|---|
| **0.0（选中）** | **1.000** | **0.864865** | 0.702703 | 133/133 | 1.000 | 168.9 | 1.703319 |
| 0.02 | 1.000 | 0.864865 | 0.702703 | 133/133 | 1.000 | 164.4 | 1.703422 |
| 0.05 | 1.000 | 0.837838 | 0.675676 | 133/133 | 1.000 | 168.5 | 1.703422 |
| 0.08 | 1.000 | 0.837838 | 0.675676 | 133/133 | 1.000 | 168.9 | 1.703422 |
| 0.1 | 1.000 | 0.837838 | 0.702703 | 133/133 | 1.000 | 172.2 | 1.703319 |
| 0.15 | 1.000 | 0.864865 | 0.729730 | 133/133 | 1.000 | 165.1 | 1.703422 |
| 0.2 | 1.000 | 0.864865 | 0.729730 | 133/133 | 1.000 | 167.2 | 1.703319 |

选择规则：首命中最高者，平局取较小值。最高 0.864865 出现在 0/0.02/0.15/0.2，取 **0.0**。0.05–0.1 的首命中下降到 0.837838（q26 被同语言弱词面命中挤掉），说明词面加成在该开发集上不改善且可能伤害跨语言排序；0.15/0.2 回到并列但按规则不选。

### 13.3 开发集最终指标（与 §12 对比）

| 指标 | §12（v3 RRF） | §13（v4 语义主导，bonus 0.0） |
|---|---|---|
| 语义 recall@5 | 0.946（35/37） | **1.000（37/37）** |
| 语义首命中 | 0.865（32/37） | **0.865（32/37）** |
| 语义行级 recall@5 / 首命中 | 0.946 / 0.757 | **1.000 / 0.703** |
| 语义回读 | 1.000（133/133） | **1.000（133/133）** |
| 语义负例 | 1.000（13/13） | **1.000（13/13）** |
| 语义 p95 / 冷启动 | 166.1 / 198.2 ms | **172.9 / 172.9 ms** |
| 索引体积比 | 1.697（32,819 B） | **1.703（32,949 B）** |
| 词面 recall@5 / 首命中 | 0.865 / 0.703 | **0.865 / 0.703**（未变） |
| 词面索引比 | 0.556（10,762 B） | **0.556（10,762 B）**（字节不变） |

分类变化：cross_lingual 0.875/0.500 → **1.000/0.625**；synonym recall 0.889 → **1.000**（首命 0.889 不变）；exact 1.000/1.000 → **1.000/0.889**；long_session 与 model_missing 保持 1.000/1.000。首命中仍为 32/37，剩余错误 q02/q10/q19/q20/q25：q02 的锚点轮次（line 3）与下一轮用户消息（line 4）都含“整表缓存”，纯语义把下一轮排在首位；其余多为同主题相邻轮次。该限制属于轮次组装/上下文问题，留给 P4。

索引增加 130 B 来自 `scoreFusion`/`lexicalBonus` 元数据；向量仍 260 B/单元，`unit_ratio` 4.265。

### 13.4 测试与证据

- `cargo test --locked`：**523 passed / 1 ignored / 0 failed**（§12 521/1；新增 2：集成 1 + 单元 1）。
- `cargo test --locked --test recall_archive_semantic`：**13 passed**（含新融合、同分覆盖、门控顺序、分页稳定断言）。
- Python：`test_recall_embedding` 21 + `test_recall_onboarding` 24 + `test_recall_eval` 12 = **57 项通过**。
- 开发集：语义模式 exit 0、两根 `semantic.available`、generationKey `embed-v1;model=qwen3-embedding:4b;dim=512;norm=l2;backend=ollama`；`--mode none` exit 0，索引字节与 P1 一致。
- 未 commit、未读真实数据、未重建分发物；held-out 未重跑，`LEXICAL_BONUS` 仅由开发集选定，不得当作 held-out 结论。

## 14. held-out 验收（orchestrator）

最终验收在独立构造、此前未参与任何调参的 set3 上完成，验收配置运行前预先确定（qwen3-embedding:4b / dimension 512 / l2 / ollama / int4，`archive-relevance-v4` 语义主导融合，LEXICAL_BONUS=0.0）。

| 集合 | recall@5 | 首命中轮次 | 回读 | 负例 | p95 ms | 索引比 |
|---|---|---|---|---|---|---|
| set3（验收） | 0.979 | 0.936 | 1.000 | 1.000 | 183.4 | 1.960 |
| set2（诊断，已消耗） | 1.000 | 0.979 | 1.000 | 1.000 | 175.6 | 1.911 |
| set1（诊断，已消耗） | 0.977 | 0.907 | 1.000 | 0.833 | 176.1 | 2.142 |

全部冻结阈值（recall@5 ≥0.80、首命中 ≥0.90、回读 =1.00、p95 ≤20 s、索引比 ≤3.0）在三份集合上均达标。评估集、构造说明与逐轮轨迹见 `hybrid-p3-heldout/README.md`。三份集合均已消耗，P4 及以后须另行独立构造 held-out。
