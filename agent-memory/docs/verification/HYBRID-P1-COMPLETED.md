# 统一混合检索 P1：Archive provider 与派生索引骨架完成记录

状态：**P1 完成**（本文件是本阶段唯一 sentinel）。
真实数据：**否**。全部用例为临时目录合成 fixture（合成 JSONL / 合成 `.jsonl.zst` / 合成清单）；未读取真实会话、知识库或生产 Archive，未对真实会话执行采集（合成语料的归档由测试在临时目录内自行完成），未实现 P2 事件存储与 P3 embedding。

范围：`SourceKind::Archive` 只读 provider、`archive-record-v1` 引用与回读、access 标签判定收窄、派生 session 映射、replay `derived/generations/g<N>/retrieval/` 骨架、onboarding v2 archive 配置生成/保留。

## 1. 实现

### 1.1 Archive provider（`src/recall/providers/archive.rs`，新增）

- 只读：仅 `Archive::open` / `manifests()` / `read_object_bytes()`（回读走 `ReadHandle::read_object`）/ `state/schema.json`，不调用 `capture_source`、`write_rollout`、`put_object`、`commit_manifest`、不取 capture/derived 锁。根只来自 `sources[].root`，无环境变量或默认路径推导。
- 证据选择与派生 replay 一致：`whole_file` / `compressed_jsonl_file` 取**最后**一份清单（整文件采集是替换），其余取**当前采集代际**（`source.generation` 最大）的全部清单；attachment 清单不进入检索语料。同一物理来源内按 `source_offset`/`seq` 排序，同内容记录去重。
- 身份：`rel_path = archive/objects/<aa>/<bb>/<object sha256>`（`object_rel_path`，非真实地址一律拒绝）；locator 仅 `{record_sha256, projection_offset}`（新增 `Locator::archive`），`capture_id`/`generation` 不参与 `hit_id`；`range_sha256` 按**解压后**记录投影计算（`.jsonl.zst` 对象本就是解码字节）。因此同一内容在同一解码对象/载体上两次采集 `hit_id`、`range_sha256` 一致；同一记录行落在两个不同对象时是两个 ref（各有真实对象地址，均可回读）。
- 检索：折叠后 `lexical_match`，命中按 `rank_hits`（distinct terms → 首个命中偏移 → 时间 → id）排序；`snippet` 以命中为中心；预算按记录字节与耗时推进，`detail.budget` 与 `detail.CONTENT_UNRESOLVED` 沿用既有键。
- 访问判定一次完成：先用同一次清单快照算出每个对象的**严格交集**标签（与 `Archive::object_accessible` 同谓词，含"未引用的孤儿对象不可读"），再逐记录用 `classify_access` 判定，避免逐对象重列清单。
- 记录标签非空但属其他域 = 正常范围外（计入 `denied`/`unreachable`，不降级）；**空标签或对象无可读映射 = 缺权限映射**（计入 `labels_missing`）：全部如此 → `unavailable` + `DOMAIN_DENIED`，部分如此 → `partial` + `RECORD_SKIPPED`，不会静默消失。
- 状态：无记录 → `empty`（`reason=archive_has_no_records`）；全部记录因域被拒 → `unavailable` + `DOMAIN_DENIED`/`DOMAIN_UNREACHABLE`（`reason=archive_records_not_readable`）；对象缺失 → `unavailable` + `DB_UNAVAILABLE`（`archive_object_missing`）；对象损坏 → `unavailable` + `DB_CORRUPT`；记录不可投影 → `partial` + `CONTENT_UNRESOLVED`；根未初始化 → `unavailable` + `SOURCE_ROOT_INVALID`；schema/派生代际不匹配 → `unavailable` + `SCHEMA_UNSUPPORTED`；标签缺失 → `unavailable`（全缺）或 `partial`（部分缺）。凡因访问缺失、对象缺失或代际不匹配而不可读，都不是健康空结果；其他域标签的范围外记录按域正常过滤，数量在 detail 中可见。
- `source_status[].detail` 新增 `archive` 块（`records`/`searched`/`denied`/`unreachable`/`unmapped`/`labels_missing`/`session_mapping`/`derived_generation`/`missing_objects`/`corrupt_objects`）。`DETAIL_KEYS` 由 9 项扩为 10 项。

### 1.2 引用与回读（`src/recall/refs.rs`、`src/recall/read.rs`）

- 新增 schema gen `archive-record-v1`（`ALL_SCHEMA_GENS` 5 → 6）；locator schema 为 `record_sha256` + `projection_offset`，`record_sha256` 必须是 64 位小写十六进制。
- `memory_read`：校验 `source_kind`/`root_sha256`/`schema_gen`；`rel_path` 必须指出记录所在的**真实对象**，记录按 `(record_sha256, object)` 定位（同一记录行存在两个载体时各自可回读，上下文取自各自来源）；`projection_offset` 必须等于 `text_range.byte_start`；`text_range` 先按字节校验非空、在界内且落在 UTF-8 字符边界（空区间/半个字符 → `REF_OUT_OF_RANGE`，手写 ref 不会触发 panic），再比对 `range_sha256`（不匹配 → `REF_STALE`，exit 5）。
- 上下文只在同一物理来源内扩展（`context_before`/`context_after`），相邻记录不可读时省略（目标记录必须可读）；组装时**优先保留目标记录的字节预算**，前置上下文再长也不会把被引用记录挤出结果；多段响应按 `max_bytes` 截断并置 `truncated`。
- `--strict-snapshot` 对 archive 源显式拒绝（`reason=strict_snapshot_unsupported_for_archive_root`），不复制对象库。

### 1.3 domain 收窄（`src/recall/search.rs`、`src/recall/read.rs`）

- 删除 P0 的"任何非 default 生效 domain 一律 `DOMAIN_UNREACHABLE`"短路。
- 仅 Archive 记录参与 access 标签判定：`classify_access(labels, effective, explicit)`，空标签 fail-closed，非 default 标签在隐式 default 下 `DOMAIN_UNREACHABLE`。
- KB / device / OpenCode / Codex 活源在任意生效 domain 下行为与 P0 前一致（请求级或配置级非 default 均不影响其结果与 `hit_id`）。

### 1.4 session 映射

- 派生层声明来源归属：读取 `derived/state.json` 的最大代际，要求其 identity（parser/model/archive schema）与当前一致，再读 `derived/generations/g<N>/retrieval/index.json` 的 `sources[].sessionId`。
- 状态显式：`ok` / `absent` / `identity_mismatch` / `unreadable`。未知映射即 `unmapped`，`session_id` 过滤不猜；被打掉的记录计入 `archive.unmapped`。
- 派生代际不匹配不会退回旧映射，也不会按文件名或最近会话推断。

### 1.5 派生检索骨架（`src/replay.rs`）

- `replay()` 现在委托 `replay_with_generator()`；每次 replay 在该代际写入 `derived/generations/g<N>/retrieval/index.json`（`agent-memory/retrieval-index@1`）。
- 文档内容：`identity`（parser/model/archive schema）、`generator`（`kind` + `lexical-v1` 或冻结的 `embed-v1;model=…;dim=…;norm=…;backend=…` 及 `model_id`/`dimension`/`normalization`/`backend`）、`unit_parser=archive-record-v1`、`access`（所有来源标签的严格交集，空即 fail-closed）、`sources[]`（含 `sessionId`/`sessionMapping`/`access`）、`units[]`（`unitSha256` = sha256(归一化文本 | 单元类型 | parser 代际)、`artifact`、`messageIndex`、`textSha256`、`textBytes`、`access`）、`unitCount`。
- 无时间戳、无采集 id、无独立代际号字段；`units[].artifact` 仍是 `derived/generations/g<N>/sources/<key>.json`，所以"重建一致"指同一代际目录下的字节一致：删除整个 `derived/` 后从头 replay 仍得到 `g1` 与相同字节。`replay` CLI 使用 `lexical-v1`；embedding 身份只在调用方传入 `EmbeddingModelSpec` 时写入（P3 才接配置）。P1 只写骨架：无向量、无遥测、不读真实会话。

### 1.6 onboarding（`assets/scripts/recall_onboarding.py`、`64-install-recall.ps1`）

- 新增 `--archive-root`（PS 侧 `-ArchiveRoot`）：显式写入/更新 v2 `archive` 只读源；目录不存在显式失败；省略时保留既有 archive 源且绝不推导默认路径。
- `build_config` 的既有合并逻辑保持：本次未给出的源原样保留，`source_order` 仍精确覆盖 enabled 源。

## 2. 测试

`cargo test --locked`：**472 passed / 1 ignored / 0 failed**（P0 基线 453 passed / 1 ignored；旧测试无回归，新增 19）。
Python：`test_recall_onboarding.py` **24 项**（新增 1）、`test_recall_embedding.py` **9 项**，共 33 项通过。

### 2.1 新增 `tests/recall_archive.rs`（16 项）

| 用例 | 覆盖 |
|---|---|
| `archive_and_live_source_hit_and_read_back_independently` | Archive 与 device 各自命中/回读；`rel_path` 为真实对象地址；locator 不含 session/pointer |
| `same_content_captured_twice_keeps_hit_id_and_range_sha` | 同内容两次采集同一 `hit_id`/`range_sha256`，不产生重复结果 |
| `compressed_and_plain_capture_share_identity_and_readback` | 明文与 `.jsonl.zst` 解码对象共享身份与回读文本 |
| `domain_labels_gate_archive_records_but_never_live_sources` | 隐式 default → `DOMAIN_UNREACHABLE`；`work` → 命中并回读；`private` → `DOMAIN_DENIED`；read 同样拒绝（exit 5）；同请求下 device 源不受影响 |
| `empty_access_labels_fail_closed` | 清单 access 为空 → 检索/回读均 `DOMAIN_DENIED`（`labels_missing`） |
| `missing_object_and_generation_mismatch_are_explicit` | 对象删除 → `archive_object_missing` + read `REF_LOCATOR_MISSING`；schema 代际不匹配 → `SCHEMA_UNSUPPORTED` |
| `session_mapping_is_unmapped_until_the_derived_layer_says_otherwise` | 无派生 → `unmapped` 且过滤为空（不猜）；replay 后可过滤；identity 改动 → `identity_mismatch` |
| `retrieval_index_carries_generator_identity_and_rebuilds_identically` | 索引含 model id/维度/归一化/generation key/access；删除 `derived/` 重建字节一致 |
| `context_max_bytes_stale_and_pagination_follow_the_read_contract` | 多段上下文、`max_bytes` 截断、`REF_STALE`、伪造 `rel_path` 拒绝、`next_offset` 分页 |
| `malformed_ranges_and_carriers_are_rejected_without_panicking` | 空区间与跨字符边界的 `text_range` → `REF_OUT_OF_RANGE`（不 panic）；指向其他对象的 `rel_path` 拒绝 |
| `context_cannot_push_the_target_past_the_byte_budget` | `context_before` + 极小 `max_bytes` 时目标记录仍被返回（首段即目标、`truncated`） |
| `duplicate_record_in_a_second_carrier_reads_back_from_its_own_object` | 同一记录行存在两个对象载体时两个 ref 都可回读 |
| `out_of_scope_records_are_filtered_and_missing_labels_are_an_anomaly` | 其他域记录属正常范围外（计数不降级）；缺标签记录 → `partial` + `RECORD_SKIPPED` + `labels_missing` |
| `non_utf8_records_are_reported_as_unresolved` | 不可投影记录 → `partial` + `CONTENT_UNRESOLVED` 计数 |
| `archive_search_and_read_are_read_only` | 检索/回读前后 archive 根树指纹一致，且不产生 `capture.lock` |
| `onboarding_writes_and_preserves_an_archive_source` | `--archive-root` 生成只读源；缺目录显式失败且不改写；省略时保留；生成配置加载后合成 `memory_search` 命中并回读 |

单元测试（`src/recall/providers/archive.rs`，3 项）：对象地址往返与拒绝逃逸、投影为解码记录且不可投影返回 `None`、locator 只含 `record_sha256`/`projection_offset`。

### 2.2 更新既有 P0 用例（因 P1 语义收窄）

- `tests/recall_config_v2.rs`：`domain_precedence_is_request_then_configuration_then_default` 改为"配置级非 default 不再影响活源，且不出现 `DOMAIN_*`"；`archive_is_v2_only_and_an_uninitialized_root_is_explicit` 改为未初始化根 → `SOURCE_ROOT_INVALID`、read → `REF_LOCATOR_MISSING`。
- `tests/recall_cli_mcp.rs`：`non_default_domain_does_not_restrict_live_sources`（原"非 default 全拒"反例），断言 KB 命中/回读在 `domain=work` 下与 default 完全一致，MCP 与 CLI 一致。
- `tests/recall_contract.rs`：`DETAIL_KEYS` 10 项（含 `archive`）、`ALL_SCHEMA_GENS` 6 项（含 `archive-record-v1`）。
- `tests/recall_boundary.rs`：反向 tripwire 从"禁止一切 `crate::archive`/`crate::replay`"收窄为禁止**写**入口（`put_object`、`commit_manifest`、`save_*`、`acquire_*_lock`、`Archive::init`、`crate::capture`、`crate::replay::replay*`、`crate::scrub`、`atomic_write`、`fs::write`、`File::create`、`OpenOptions` 等），读取路径仍受约束。

## 3. 命令与退出码

| 命令（工作区 `C:\wt\hybrid-retrieval-20261028`） | 结果 |
|---|---|
| `cargo test --locked` | **472 passed / 1 ignored / 0 failed** |
| `python assets\scripts\test_recall_onboarding.py` | 24 项通过 |
| `python assets\scripts\test_recall_embedding.py` | 9 项通过 |
| `powershell ... assets\scripts\check-docs.ps1` | exit 0（6 项、55 个 Markdown 全覆盖） |
| `powershell ... assets\scripts\62-verify-agent-memory-offline.ps1` | **exit 1**（3 项未通过，均为同因：`-AgentMemoryOnly` 审计因源码树哈希已变化而 exit 1；其余 26 项含全部反向用例 PASS） |
| `powershell ... assets\scripts\90-audit.ps1 -AgentMemoryOnly` | **exit 1**（唯一 FAIL：构建谱系——源码树哈希 `babe9ab3…` ≠ 分发物 `92eadd42…`；其余 6 项 PASS） |
| `64-install-recall.ps1` Parser 校验 | PARSE OK；文件仍为 UTF-8 BOM |
| 手工 CLI 合成冒烟（`init` → `capture --domain default` → v2 配置 → `memory_search`） | search exit 0（2 命中，`archive-record-v1`）；`memory_read` 带 `context_before` exit 0；`domain=work` search exit 4 + `DOMAIN_DENIED`（显式域，标签仅 `default`） |
| `git diff --check` | exit 0 |

62/90 的失败原因单一且已知：本执行器**不重建** `assets/dist/agent-memory/`，而 90 要求"源码树哈希 = 分发物清单哈希"。orchestrator 重建分发物后必须复跑 `62` 与 `90 -AgentMemoryOnly` 并记录实际退出码 0。

## 4. 限制与已知边界

- **不实现 embedding**：`interpreter.embedding` 只作为索引生成器身份写入 artifact（且仅当调用方以 `EmbeddingModelSpec` 构建索引；`replay` CLI 用 `lexical-v1`）；检索仍是词面；无 Ollama/Python 调用。
- **不实现遥测/事件存储**：MCP 仍恒只读，检索路径不产生任何事件或副作用；`events` 子命令保持 P0 行为。
- **投影是解码后的记录字节**：P1 不做按 provider 的字段级投影（不解析 JSON 提取正文），因此 snippet 是记录文本片段；字段级单元模型属于后续阶段。
- **session 映射依赖派生层**：未 replay 的 Archive 只能得到 `unmapped`；这是有意的 fail-closed 行为。索引里的 per-unit `sessionId` 供 P3 使用；多会话来源（SQLite 观测）在 P1 无法把记录归属到会话，因此保持 `unmapped`。
- **混合标签按记录判定**：同一来源中部分记录被拒、部分可读时，命中仍返回；"其他域"计入 `detail.archive.denied`/`unreachable`（正常范围外），"标签缺失"计入 `labels_missing` 并降级为 `partial` + `RECORD_SKIPPED`。
- **read 路径的代际错误**：archive 根未初始化 → `REF_LOCATOR_MISSING`；schema/派生代际不匹配 → `REF_READ_FAILED`（消息含具体版本），不新增 read 专用错误码。
- **合同版本保持 2**：P1 是同一 v2 合同的增量扩展（新增 `archive-record-v1` schema gen 与 `detail.archive`），不改变顶层键集合；v1 归一逻辑不变。`ARCHIVE_PROVIDER_UNAVAILABLE` 仍保留在冻结错误码表中（P1 起不再产生），避免改动 27 项冻结表。
- **单对象整读**：对象按内容地址整份读入并校验，单个大对象的峰值内存等于对象大小；已在合成 33 MB 记录下验证，但不承诺生产规模。
- **未验证真实环境**：无真实 Archive 结构、无真实采集并发、无生产规模延迟数据；P1 只承诺合成语料下的行为。

## 5. 未做

- 未实现 P2（`state/retrieval_events.sqlite`、幂等计数、导出、保留与 prune）与 P3（embedding 执行、融合、held-out 评估）。
- 未实现 P4 图关联、P5 生产试采。
- 未重建 `assets/dist/agent-memory/`、未改 `build-manifest.json`、未执行 git commit（提交由 orchestrator 执行）。
- 未读取任何真实会话、知识库或生产 Archive 数据。

## 6. 收口（orchestrator）

- 实现提交：`2369e70`；分发物重建 `e9eaa5e`，谱系刷新 `92ae8e5`（树哈希 `b45fcaece6bb…`，二进制 SHA256 `93f61d0f37f3…`，双构建一致，静态 CRT）。
- 复跑：`cargo test --locked` 472 passed / 1 ignored / 0 failed（orchestrator 独立复跑）；`62-verify-agent-memory-offline.ps1` exit 0（28/28）；`90-audit.ps1 -AgentMemoryOnly` 在本机重装分发物后 exit 0（7/7）。
- fresh verifier：`zhipuai-coding-plan/glm-5.3-flash`（与执行者不同 lineage），target `2369e70`，verdict **通过**，无阻断。
- 非阻断遗留（后续阶段认领）：
  - N1 `searched==0` 且同时存在域外记录与 `labels_missing` 时，源级状态只报 `DOMAIN_*`，标签缺失仅在 `detail.archive.labels_missing` 可见。
  - N2 `read_archive` 对上下文记录只做对象级 access 交集检查，未对记录自身再跑 `classify_access`（正常采集下二者同源同值）。
  - N3 `recall_boundary` tripwire 为全路径子串匹配，模块别名调用可绕过文本检查（当前无此类调用）。
  - N4 `session_map` 未校验索引冻结的 `access`；P3 接入向量/图扩展时须把 `index.access` 与 identity 一起作为准入条件。
