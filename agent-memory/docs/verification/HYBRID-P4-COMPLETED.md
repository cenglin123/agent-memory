# 统一混合检索 P4：图关联与上下文组装完成记录

状态：**P4 完成**（本文件是本阶段唯一 sentinel）。
真实数据：**否**。全部用例使用临时目录合成 fixture（合成 Codex / Claude JSONL 语料、临时 Archive 运行时根、临时配置）；未读取任何真实会话、真实知识库或生产 Archive，未对真实数据执行采集、回读或 embedding，未重建 `assets/dist/agent-memory/`，未执行 git commit。开发集诊断使用本机 Ollama loopback 上的 `qwen3-embedding:4b`，只嵌入合成语料文本。

范围：可证明 `next` 边（记录级与轮次级）、`memory_read` 的 `context: "turn"` 整轮组装、`memory_search` 的 `related_hops` 有限扩展、节点/耗时预算、子代理与侧链抑制、跨域拒绝与计数、`recall_eval.py` 的 `turn_context_contains` 诊断。P3 的排序 / 融合 / 门控 / 量化、MCP 只读语义与 P2 事件存储均未改动。

## 1. 实现

### 1.1 图模块（`src/recall/graph.rs`，新增）

- **只有 `next` 边**。`SourceGraph` 按来源顺序接收**可见**记录（通过对象级 + 记录级 access 且有文本投影），每条记录记录 `unit`（在 provider 单元列表中的位置）、`turn`、`evidence`。
- **相邻**的判据是「位置差内每一条记录都存在」：`contiguous(a, b)` 要求 `records[b].unit == records[a].unit + (b - a)`。被 access 拒绝、无 UTF-8 投影、或超出源扫描预算的记录**不进入**列表，因此它们的缺口必然打断链条——归属未知即不连，不用文件名、时间或邻近会话猜测。
- **元数据是中转但不作节点**：`session_meta` / `turn_context` / Claude `system` 等 `extract()` 为空的记录保留在列表中（保持 Codex 轮间 `turn_context` 不打断相邻性），`evidence=false`，永不被选为代表或邻接记录。
- **轮次相邻**：`adjacent_turn(t, prev|next)` 取 `t` 与 `t±1` 的边界证据记录（`turn_last` / `turn_first`），要求二者 `contiguous`；否则 `Blocked`（计入 `denied`）或 `End`。
- `related_nodes(unit, hops)`：`hops∈1..=2`，输出 hop 优先、`previous` 先于 `next` 的确定性顺序；相邻轮次取该轮**首条证据记录**为代表（`hops=轮距`），同轮再取第 `n` 条证据记录（`hops=记录距`）。每 hit 上限 `RELATED_NODES_PER_HIT=6`。
- 子代理/侧链：`is_subagent_session_record` 识别 Codex `session_meta.payload.source.subagent`（与 Codex 会话解析器同一谓词）；`is_sidechain_record` 识别 Claude `isSidechain` / `isMeta`。子代理整源与侧链记录不进入图，`related_nodes` 直接返回空并计 `sidechain`。
- 命名常量：`RELATED_MAX_HOPS=2`、`RELATED_NODES_PER_HIT=6`、`RELATED_NODES_PER_SOURCE=24`、`RELATED_MAX_ELAPSED_MS=300`、`TURN_CONTEXT_MAX_RECORDS=512`、`TURN_CONTEXT_MAX_SEGMENTS=64`。

### 1.2 `memory_read`：`context: "turn"`（`src/recall/read.rs`）

- 新请求键 `context`，唯一非默认值 `"turn"`；与 `context_before`/`context_after` 同时给出是 `INVALID_REQUEST`（`conflicting_context`）；非 Archive 源请求 `context:"turn"` 是 `INVALID_REQUEST`（`unsupported_for_source`）而不是静默退回窗口模式。
- 组装：从目标记录向后扫描到开启该轮的用户记录（含），向前扫描到下一条用户记录（不含）；每条候选先跑记录级 `classify_access`，再解码投影，再判侧链。**被拒/不可读/侧链记录是硬边界**：既不进入结果，也截断该轮（`truncated=true`），不会把两轮拼成一轮。
- 只在**同一物理来源**内扫描（`source_units` 按 `unit.source` 过滤），不跨会话；元数据记录不成为段。
- 每段是一个完整记录的 `Segment`，新增可选 `segments[].ref`（完整 `archive-record-v1`，`projection_offset=0`、`text_range` 覆盖整条投影、`range_sha256` 与段文本一致），可单独回读。
- 预算：目标记录优先进预算；其余按 `|i - target|` 升序（同距按位置）整条纳入，放不下即丢弃并置 `truncated`；目标单独超预算时只返回其前缀（`truncated=true`）。
- 仍做引用校验：新增 `verify_archive_range`，即使整轮模式会返回整条记录，调用方给出的 `text_range` 必须是投影内非空、字符对齐且 `range_sha256` 相符，否则 `REF_OUT_OF_RANGE` / `REF_STALE`。

### 1.3 `memory_search`：`related_hops`（`src/recall/providers/archive.rs`、`search.rs`）

- 新请求键 `related_hops`（整数 `1..=2`，其它值/类型 `INVALID_REQUEST`）；默认缺省。
- 开启时在扫描循环里同步构图（`graphs: BTreeMap<source, SourceGraph>`、`projections: BTreeMap<unit, String>`、`hit_origin: BTreeMap<hit_id, unit>`）；关闭时不分配任何结构。
- **扩展严格在 access 过滤与融合后门控之后运行**：主结果的顺序、`score`、`rank_basis`、`source_rank`、`snippet`、`gated_out` 与 `source_status` 在扩展前已定稿；`RelatedItem` 只追加到 `SourceHit.related` → `results[].related`。
- 每个 related 项自带完整 `archive-record-v1` `ref`、`hit_id`、`title`、`edge="next"`、`direction`、`hops`、`turn`；`hit_id` 由同一 `hit_id()` 计算，可直接 `memory_read`。
- 预算：每 hit 6 节点、每源 24 节点、扩展阶段 300 ms（独立计时器）。任何因预算、不可见记录、子代理导致的省略都计入 `detail.archive.related`：`{edge, hops, nodes, denied, sidechain, truncated, limits{...}}`。该块仅在请求 `related_hops` 时出现。
- 主结果字节一致：`related` 为空时 `SearchResult.related` 带 `skip_serializing_if`，`detail.archive` 不新增键；`tests/recall_archive_graph.rs::related_off_keeps_the_p3_response_shape` 逐键比对 P3 结构。

### 1.4 合同与传输（`contract`/`mcp`/`providers`）

- `SourceHit` 增加 `related: Vec<RelatedItem>`（codex/device/kb/opencode 构造点显式置空）；`SearchResult` 增加可选 `related`。
- MCP schema：`memory_search` 增 `related_hops`（1..2），`memory_read` 增 `context`（enum `["turn"]`）。两个工具仍只有 `readOnlyHint: true`，未新增工具、未改事件存储路径。
- `READ_KEYS` 5→6、`SEARCH_KEYS` 8→9；`READ_RESPONSE_KEYS`、`DETAIL_KEYS`、错误码与状态表均未变。两个请求键与其取值范围固化为合同常量（`SEARCH_REQUEST_KEY_RELATED_HOPS`、`READ_REQUEST_KEY_CONTEXT`、`READ_CONTEXT_TURN`，上限复用 `graph::RELATED_MAX_HOPS`），并有合同测试 `p4_request_keys_are_frozen`。

### 1.5 N2 回归

P3 已在 `read_archive` 的 `context_before/after` 补记录级 `classify_access`；本阶段为它补**专门回归用例**（记录级标签改成 `work` 后，窗口模式不泄露），并让整轮组装沿用同一谓词（上下文记录逐条判定，被拒即硬边界）。窗口模式的省略语义保持 P3（尽力邻居，不新增状态位），整轮模式才用 `truncated` 报告截断。

### 1.6 评估诊断（`assets/scripts/recall_eval.py`）

- 新增 `_readback(..., context=...)` 与逐查询诊断：正向查询的 top-1 命中以 `context: "turn"` 回读，任一锚点 `contains` 出现在组装文本中即通过；写入 `per_query[].turn_context_checked/turn_context_contains`、`metrics.turn_context_contains{checked,passed,rate}`、`stages.turn_context`。
- 只作诊断：不进入 recall@5、首命中、回读成功率、负例通过率的分子/分母。格式文档 `HYBRID-EVAL-FORMAT.md` §4 已登记。

### 1.7 文档

- `agent-memory/README.md`：新增「Graph expansion and turn assembly (P4)」小节，并在 Recall slice 的 Archive 条目与 CLI 请求键说明中交叉引用。
- `assets/references/agent-memory-archive.md`：新增 §4.3「图关联与轮次组装（P4）」——边类型、参数、预算、轮次组装、权限与子代理规则。
- `CHANGELOG.md`：顶部新增「统一混合检索 P4：图关联与上下文组装」。
- `agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md`：登记 `turn_context_contains`。

## 2. 设计决定（规格未逐字规定处）

1. **参数命名**：`memory_search.related_hops`（整数 1–2，而非布尔开关）与 `memory_read.context: "turn"`（字符串枚举而非布尔）。前者让"跳数"成为请求的一部分并可机械校验上限，后者为后续其它上下文模式留出枚举位。
2. **元数据是透明中转**：Codex 在轮与轮之间写 `turn_context`。若把元数据记录当作普通节点，或把它们视为缺口，真实会话的相邻轮次将永远不连。因此元数据保留在顺序中（`contiguous` 成立）但 `evidence=false`，不作为节点也不作为段。被拒/无投影的记录相反：它们**是**缺口，因为无法证明其内容。
3. **轮次代表 = 该轮首条证据记录**：通常就是开启该轮的用户消息；若该用户消息被拒，则取该轮首条可见证据记录（该轮编号本身由可见记录推导）。`hops` 对轮级节点是轮距、对轮内节点是证据记录距，二者都在 `hops` 字段如实给出并在 `direction` 上区分。
4. **`related` 默认关闭**：保持 P3 响应体积与结构。计数器只随请求出现，避免默认响应多出键；`tests/...::related_off_keeps_the_p3_response_shape` 断言结果键集与 `detail.archive` 键集与 P3 相同，且默认 `memory_read` 段不带 `ref`。
5. **子代理整源抑制**：Codex 的子代理标记在 `session_meta`（整源级），Claude 的侧链标记在记录级。两者都直接抑制扩展（`related` 空 + `sidechain` 计数），整轮组装只返回被寻址记录并标 `truncated`。这是比"只排除子代理记录"更严的 fail-closed 选择：本阶段没有统一的派生归属，无法证明子代理轮次与父轮次的关系。**[UNCERTAIN]** 是否应允许在子代理自有会话内部组装它自己的轮次，规格只要求"不得因扩展或组装出现在父会话结果中"。
6. **整轮预算**：目标的"优先"实现为"先占预算、其余按距离填充、整条纳入或整条丢弃"。只有目标可能被截成前缀（否则 8 KiB 默认预算下长记录无法回读）。非目标段永不截断，从而保证每个段的 `range_sha256` 与它自己的 ref 完全一致。
7. **窗口模式不新增状态位**：P3 的 `context_before/after` 对跨域邻居是"静默省略"；本阶段不改变 P3 行为，只在整轮模式用 `truncated` 明确表示"该轮被截断"。跨域可见性由 `related` 的 `denied` 计数与整轮 `truncated` 提供。
8. **相关项仍是 `archive-record-v1`**：`related[]` 复用 `hit_id()` 与完整 ref，不新增第二种引用形态；因此 `memory_read`、`hit_id` 与 access 判定对所有相关项同样适用，`related` 不引入第二套身份。

## 3. 测试

`cargo test --locked`：**546 passed / 1 ignored / 0 failed**（P3 基线 523/1，旧测试无回归，净增 23：集成 17 + 单元 6）。

### 3.1 新增 `tests/recall_archive_graph.rs`（17 项，全部临时目录合成 fixture）

| 用例 | 覆盖 |
|---|---|
| `turn_context_returns_the_whole_turn_and_every_segment_reads_back` | 整轮返回用户消息 + 回答 + 工具调用 + 工具结果，顺序正确、不含相邻轮；每段 `ref` 单独回读文本与 `range_sha256`/`text_range` 完全一致；默认搜索响应无 `related` |
| `turn_context_keeps_the_target_when_the_byte_budget_is_tight` | 预算仅容目标 + 8 字节：目标保留、`truncated=true`、段数减少、总字节不超预算 |
| `turn_context_never_stitches_two_sessions` | 两个物理来源；整轮只含目标会话的 2 段，另一会话的标记不出现；每段 `projection_offset=0` |
| `related_reports_adjacent_turns_for_one_and_two_hops` | 1 跳 = 前一轮代表 + 同轮下一条 + 后一轮代表（3 项，`hops=1`、`edge=next`）；2 跳新增 2 轮以外的代表（`hops=2`）；无 3 跳；`detail.archive.related.hops/limits` 正确；每个 related ref 可回读 |
| `related_hops_outside_one_to_two_are_rejected` | `0/3/-1/"2"/1.5` 均为 `INVALID_REQUEST`；1/2 通过 |
| `related_expansion_is_bounded_per_hit` | 长轮 + 双侧相邻轮：每 hit ≤ 6 节点，`related.truncated≥1` |
| `related_expansion_is_bounded_per_source` | 10 个命中：related 总数恰为 `RELATED_NODES_PER_SOURCE=24`，`truncated≥1` |
| `related_does_not_change_the_main_result_order_or_the_gate` | 有/无 `related_hops` 的 `hit_id` 顺序、`score`、`rank_basis`、`source_rank`、`snippet` 全等；`detail.archive`（去掉 `related` 后）逐键相等 |
| `related_off_keeps_the_p3_response_shape` | 默认响应序列化不含 `"related"`；结果键集与 `detail.archive` 键集与 P3 完全相同；默认 `memory_read` 段无 `ref` |
| `identical_content_in_two_archive_roots_never_links_across_sources` | 两个 archive 根同样内容：hit_id 不同（source 参与身份）；每个 related 的 `source_id`/`root_sha256`/`source_kind` 与命中一致，绝不跨源 |
| `a_live_source_never_reports_related` | 同配置下 device 命中无 `related`，archive 命中才有 |
| `a_codex_subagent_session_is_never_expanded` | Codex `session_meta.payload.source.subagent`：父会话 related 只指向自身文件；子代理命中的 related 为空且 `related.sidechain≥1` |
| `a_claude_sidechain_record_never_joins_its_parent_turn` | 同文件内 `isSidechain` 记录：父轮组装不含其文本且 `truncated`；侧链命中 related 为空、`sidechain≥1`，整轮只返回自身一条 |
| `a_record_denied_by_its_own_label_never_appears_in_context` | N2 回归：记录级 `work` 标签下窗口模式不泄露该记录；整轮模式把它当硬边界（不含该记录、也不含更早的同轮用户消息），`truncated=true` |
| `related_never_reports_a_denied_record_and_counts_the_omission` | 被拒记录不出现在 `related`；`related.denied≥1`；另一侧可读的相邻轮仍然连上 |
| `turn_context_still_verifies_the_reference_range` | 整轮模式仍校验 `range_sha256`（`REF_STALE`）与空区间（`REF_OUT_OF_RANGE`），不会把校验放宽 |
| `turn_context_is_archive_only_and_rejects_conflicting_keys` | `context` 非 `"turn"` 或与 `context_before` 同用 → `INVALID_REQUEST`；device 源请求整轮 → `INVALID_REQUEST` |

### 3.2 新增单元测试（`src/recall/graph.rs` 5 项 + `tests/recall_contract.rs` 1 项）

- `a_gap_in_record_positions_breaks_the_chain`：缺失区域的记录不连边，`denied` 计数。
- `adjacent_turns_and_same_turn_records_are_reported_with_hops`：轮相邻 + 同轮记录的 `hops`。
- `metadata_records_keep_the_chain_but_are_never_nodes`：`session_meta`/`turn_context` 不打断相邻性、不作节点。
- `subagent_sources_and_sidechain_records_never_expand`：整源与单条侧链均抑制，`sidechain=1`；两个 provider 标记谓词。
- `hops_outside_the_bound_are_rejected`：`0` 与 `3` 返回空。
- `p4_request_keys_are_frozen`（`tests/recall_contract.rs`）：`related_hops` / `context` / `"turn"` 与 `RELATED_MAX_HOPS` 由合同常量固化。

### 3.3 修改的既有断言（逐条原因）

1. `tests/recall_cli_mcp.rs::mcp_search_schema_declares_the_session_id_filter`：`memory_search` 的 `inputSchema.properties` 数量 8 → 9。原因：P4 新增可选 `related_hops`，schema 必须声明它；断言的原始意图（`session_id`/`domain` 形状）保留。

### 3.4 Python

`test_recall_embedding.py` 21 + `test_recall_onboarding.py` 24 + `test_recall_eval.py` 12 = **57 项通过**。

## 4. 开发集诊断（参考，不作为验收）

环境：Ollama loopback `qwen3-embedding:4b`、`dimension=512`、`l2`、int4，`archive-relevance-v4`；50 条查询、6 个语料、2 个 domain 根；轮次口径。

| 指标 | P3（§13.3） | P4 本次 |
|---|---|---|
| recall@5 | 1.000（37/37） | **1.000（37/37）** |
| 首命中轮次正确率 | 0.865（32/37） | **0.864865（32/37）** |
| 回读成功率 | 1.000（133 ref） | **1.000（133/133）** |
| 负例通过率 | 1.000（13/13） | **1.000（13/13）** |
| p95 | 172.9 ms | 167.7 ms |
| 索引体积比 | 1.703 | **1.703（32,949 B）** |
| `turn_context_contains`（新，诊断） | — | **33/37 = 0.892** |

- P3 的冻结指标逐项不变，说明 P4 未扰动排序/门控/量化。
- 诊断口径下仍有 4 条未通过（q10/q19/q20/q25），恰好是首命中错误的查询：top-1 落在错误轮次时，那个轮次的组装结果自然不含期望文本。这是诊断的预期语义，不是整轮组装的缺陷（回读成功率仍为 1.000）。
- 词面基线（`--mode none`，同集）：`turn_context_contains` 26/34 = 0.765。

## 5. 命令与退出码

| 命令（工作区 `C:\wt\hybrid-retrieval-20261028`） | 结果 |
|---|---|
| `cargo test --locked`（`agent-memory`） | **exit 0**；546 passed / 1 ignored / 0 failed |
| `cargo test --locked --test recall_archive_graph` | exit 0；17 passed |
| `cargo test --locked --lib recall::graph` | exit 0；5 passed |
| `cargo test --locked --test recall_contract` | exit 0 |
| `python -m pytest assets/scripts/test_recall_embedding.py assets/scripts/test_recall_onboarding.py assets/scripts/test_recall_eval.py` | exit 0；57 passed |
| `powershell -File assets/scripts/check-docs.ps1` | exit 0（6 项、63 个 Markdown） |
| `python assets/scripts/recall_eval.py --set agent-memory/tests/fixtures/hybrid-eval-dev --out <tmp>/p4-semantic.json` | exit 0（两根 `semantic.available`，generationKey `embed-v1;model=qwen3-embedding:4b;dim=512;norm=l2;backend=ollama`） |
| `python assets/scripts/recall_eval.py --set agent-memory/tests/fixtures/hybrid-eval-dev --mode none --out <tmp>/p4-lexical.json` | exit 0 |
| `git diff --check` | exit 0 |

未运行 `cargo fmt`：钉定工具链 `1.99.0-x86_64-pc-windows-msvc` 未安装 `rustfmt` 组件（与 P2 记录一致）。

未执行 `62` / `90`，未重建 `assets/dist/agent-memory/`、`build-manifest.json` 或谱系（由 orchestrator 负责）。

## 6. 限制与已知边界

- **无真实数据**：无真实会话分布、无生产规模档案；`related` 的节点/耗时预算只在合成语料上验证（单源 30 条记录量级），未做 10 万记录级别的压力测试。
- **`related` 模式的内存**：开启时会保留每个可见记录的投影文本（用于为节点构造 ref），上界约为当前源扫描预算（默认 32 MiB）的字符串副本；关闭时不分配。大会话 + `related_hops` 同时使用是当前最重的组合。
- **整轮扫描上界**：每个方向最多检查 `TURN_CONTEXT_MAX_RECORDS=512` 条记录、最多 64 段；超出即 `truncated`。极端长轮（> 512 条）只能拿到靠近目标的一段。
- **轮次编号是可见记录的编号**：被 access 拒绝的用户消息不可见，因此它不会开启新轮（其后记录会归入上一可见轮），但它同时是硬边界，所以不会被跨过。编号可能与 `recall_eval.py` 的"全量语料轮次"不同；两者不互相引用。
- **子代理识别只依赖 provider 已有标记**：Codex 只认 `session_meta.payload.source.subagent`，Claude 只认 `isSidechain`/`isMeta`。没有标记的 fork/resume 转录在文件层面与普通会话无法区分，本阶段选择"同物理来源内按记录顺序连边"（不会跨文件误连），但没有做逻辑归属判定。
- **侧链记录不参与统计**：`sidechain_records` 只存在图对象里，未写入 `detail`（`detail` 只给抑制计数 `sidechain`）；若需要"侧链记录条数"诊断，需要新键。
- **`related` 计数可能偏大**：同一 hit 的多个候选在同一预算耗尽后每个都会 `truncated += 1`，计数语义是"被砍掉的候选次数"，不是"被砍掉的节点数"。
- **窗口模式的跨域省略仍静默**：为保持 P3 行为（"同源邻记录跨域时省略"），`context_before/after` 不新增状态位；只有整轮模式与 `related` 提供可见计数。

## 7. 未做

- 未实现 P5（真实试采、生产评估、参数再标定）。
- 未新增 `reference` / `parent-child` / `resume` / `fork` / 逻辑归属边，也未做跨源图（计划 §3.6 明确不在本阶段）。
- 未改 P3 的候选生成、融合分、门控常量、int4 量化、`units.bin` 布局与代际选择；`related` 不参与排序。
- 未改 MCP 只读语义（仍两个工具、`readOnlyHint: true`）、未改 P2 事件存储与 `events` 子命令。
- 未新增 crate 依赖；未改 `assets/dist/`、`build-manifest.json`、61/62/90 谱系；未执行 git commit（由 orchestrator 执行）。
- 未读取真实会话、知识库或生产 Archive；未在真实运行时根下写入任何内容。

## 8. 修改文件

- `agent-memory/src/recall/graph.rs`（新增）
- `agent-memory/src/recall/read.rs`
- `agent-memory/src/recall/search.rs`
- `agent-memory/src/recall/contract.rs`
- `agent-memory/src/recall/mod.rs`
- `agent-memory/src/recall/providers/archive.rs`
- `agent-memory/src/recall/providers/mod.rs`
- `agent-memory/src/recall/providers/{codex,device,kb,opencode}.rs`（`SourceHit.related` 显式置空）
- `agent-memory/src/mcp.rs`
- `agent-memory/tests/recall_archive_graph.rs`（新增）
- `agent-memory/tests/recall_contract.rs`（新增 `p4_request_keys_are_frozen`）
- `agent-memory/tests/recall_cli_mcp.rs`（schema 属性数 8→9）
- `assets/scripts/recall_eval.py`
- `agent-memory/README.md`
- `assets/references/agent-memory-archive.md`
- `agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md`
- `CHANGELOG.md`
- `agent-memory/docs/verification/HYBRID-P4-COMPLETED.md`（本文件）

## 9. 收口（orchestrator）

- 实现提交 `3a9ead1`；分发物 `e2d0a44`、谱系 `354df22`；orchestrator 独立复跑 `cargo test --locked` 546 passed / 1 ignored / 0 failed；62 exit 0（28/28）。
- fresh verifier：`zhipuai-coding-plan/glm-5.3-flash`（与执行者不同 lineage），target `3a9ead1`，verdict **通过**，无阻断。verifier 自建对抗用例：多来源同 session_id、被拒记录夹在两轮之间与轮内、侧链部分/全部标记，均符合 §3.6 与 N2 要求。
- 非阻断遗留（后续认领）：① `contract.rs`/`archive.rs` 两处断行合并（工具链缺 rustfmt，未格式化）；② 侧链仅部分标记时未标记记录可参与轮次边界计算，使后一轮代表在 2 跳内可达（标记内容零泄露，真实全标记形态正确阻断）；③ Codex 子代理源 + `context:"turn"` 缺专门集成测试；④ related 时间预算耗尽后 `truncated` 按候选计数偏大；⑤ 子代理/侧链抑制组装与预算截断共用 `truncated`，调用方无法区分；⑥ `cargo fmt` 未运行。
- 同步 INSTALL.md（混合检索可选能力）、UPGRADE.md（§7 混合检索升级）、CURRENT.md（P5 待授权与本机安装复验待办）。
