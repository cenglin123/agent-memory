# 混合检索评估集格式（`agent-memory/hybrid-eval-set@1`）

本文是 `assets/scripts/recall_eval.py` 的输入格式契约。目标：另一个执行者**只读本文**就能独立构造一份 held-out 评估集，并让工具直接运行、产出与开发集同口径的指标。

评估集目录结构：

```text
<eval-set>/
  manifest.json          # 集合元数据 + 语料清单 + 模型规格
  queries.jsonl          # 每行一条查询与标注
  corpus/s01.jsonl       # 合成会话语料（路径自定，manifest 引用）
  corpus/s02.jsonl
  ...
```

工具只读该目录与它自己创建的临时运行时根；不会读取真实 `~/.claude`、`~/.codex`、知识库或生产 Archive。语料必须是**合成或已脱敏**内容。

---

## 1. `manifest.json`

```json
{
  "schema": "agent-memory/hybrid-eval-set@1",
  "name": "team-held-out-2026-11",
  "description": "独立 held-out 集，不含开发集语句",
  "domain": "default",
  "embedding": {
    "model_id": "qwen3-embedding:4b",
    "dimension": 512,
    "normalization": "l2",
    "backend": "ollama"
  },
  "python": null,
  "corpora": [
    {"file": "corpus/s01.jsonl", "provider": "codex", "domain": "default"},
    {"file": "corpus/s02.jsonl", "provider": "codex", "domain": "default"},
    {"file": "corpus/s03_private.jsonl", "provider": "codex", "domain": "work"}
  ],
  "queries": "queries.jsonl"
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `schema` | 是 | 固定 `agent-memory/hybrid-eval-set@1`；其他值 exit 2。 |
| `name` / `description` | 建议 | 仅进入报告，不参与判定。 |
| `domain` | 否 | 默认访问域，缺省 `default`。查询可用 `domain` 覆盖。 |
| `embedding` | 否 | 省略即纯词面运行。字段与 v2 配置 `interpreter.embedding` 一致：`model_id`（1–128 ASCII 字母数字及 `. _ : / -`）、`dimension`（1–8192 整数）、`normalization`（`l2`/`none`）、`backend`（`python`/`ollama`/`fake`）、可选 `model_path`（绝对路径）。 |
| `embedding.model_path_env` | 否 | 当 `model_path` 为空时从该环境变量取路径；两者都空时 Python 后端按 `model_path_not_configured` 降级。 |
| `python` | 否 | 运行 embedding 辅助进程的解释器绝对路径；缺省用运行 `recall_eval.py` 的 `sys.executable`。 |
| `corpora[]` | 是 | 非空数组；每项见下。 |
| `queries` | 否 | 查询文件名，缺省 `queries.jsonl`。 |

`corpora[]` 每项：

| 字段 | 必填 | 说明 |
|---|---|---|
| `file` | 是 | 相对评估集目录的路径，必须存在。 |
| `provider` | 否 | `codex`（默认）或 `claude`；传给 `capture --provider`。 |
| `domain` | 否 | 该语料的采集访问域，缺省取 `manifest.domain`。 |

**多 domain 语义**：每个不同的 `corpus.domain` 会得到**独立的 archive 运行时根**并单独 `replay`（replay 的 `--domain` 必须能读取该根的记录）。配置里会把这些根都列为 archive 源；查询用 `domain` 选择生效域。这样 `work` 语料在 `default` 查询下必须表现为 `unavailable` + `DOMAIN_UNREACHABLE`，而不是被混在一起。

`recall_eval.py` 生成配置时固定 `version=2`、`telemetry.enabled=false`、`interpreter.kb.command=<python>`。`--mode none` 会去掉 embedding 块，用同一份语料跑纯词面基线。

---

## 2. 会话语料（`corpus/*.jsonl`）

一行一条记录，UTF-8。工具通过 `agent-memory capture --provider <p>` 归档，再由 replay 解析。语料只需满足以下**最小字段**；其他字段可以省略，未知字段会被解析器忽略。

### 2.1 Codex（`provider=codex`）

首行建议是 `session_meta`，其后是 `response_item` / `event_msg`。一行一个 JSON 对象：

```json
{"type":"session_meta","payload":{"id":"s-cache","cwd":"C:/synthetic/project"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":"缓存命中率一直上不去，怎么排查？"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":"先看缓存键的粒度与失效策略……"}}
{"type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":\"rg cache_hit_ratio src/\"}"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"src/cache.rs:18: hit_ratio = hits / (hits + misses)"}}
```

- `message.content` 可以是字符串，也可以是块数组 `[{"type":"input_text","text":"…"}]`。
- `function_call` 用 `name` + `arguments`（或 `custom_tool_call` 的 `input`）；`function_call_output` 用 `output`（字符串或对象）。
- `event_msg` 的 `agent_message` / `item_completed`（`CommandExecution.aggregated_output`、`McpToolCall.result.content`）也会被投影为单元。
- `session_meta` / `turn_context` 是元数据，不产生检索单元；未知顶层 `type` 才回退为整条记录文本。
- `timestamp` 可选；缺失不影响检索与行号标注。

### 2.2 Claude（`provider=claude`）

```json
{"type":"user","message":{"role":"user","content":"数据库迁移失败了怎么回滚？"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"先做快照，再在事务里执行 down 脚本。"},{"type":"tool_use","name":"Bash","input":{"command":"psql -c 'select 1'"}},{"type":"tool_result","content":"ok"}]}}
```

`message.content` 可以是字符串或块数组；`text` / `thinking` / `tool_use`（`name`+`input`）/ `tool_result`（`content`）分别投影。`summary.summary` 可作摘要单元；`system` / `result` / `file-history-snapshot` 跳过。

### 2.3 记录行与「轮次」

一个 JSONL 行是**一条记录**，不是一轮对话。评估口径中的「轮次」按计划 §3.5 定义：

> 一条用户消息 + 其后直到下一条用户消息之前的全部助手消息、工具调用与工具结果。

工具从语料文件自行推导轮次（`queries.jsonl` 不写轮次号）：

1. 每条用户消息开启一个新轮次，轮次号从 1 递增。
   - Codex：`response_item.payload.type=message` 且 `payload.role=user`，或 `event_msg.payload.type=user_message`。
   - Claude：`type=user` 且 content 不是**仅由 `tool_result` 块组成**（Claude 把工具结果记成 user 记录，它属于当前助手轮次，不开启新轮次）。
2. `session_meta` / `turn_context` 等元数据行归入**其后第一个轮次**；其后没有用户消息时归入当前轮次。
3. 文件开头、第一条用户消息之前的记录（例如孤立的 assistant/system 输出）归入**轮次 0**。
4. 其余助手消息、工具调用、工具结果都归入最近一次用户消息开启的轮次。

`queries.jsonl` 的 `line` 是**语料文件中的 1-based 行号**；它只用于定位锚点，工具会把该行换算成轮次。`memory_search` 命中的 title 形如 `codex s01.jsonl line 3` / `claude s02.jsonl line 7`，工具同样把命中行换算成轮次后比较。

约束：

- 一行 = 一条记录；不要把多条消息塞进同一行，否则轮次与行号都无法标注。
- 语料文件在评估期间必须保持不变（工具按文件内容采集，`hit_id`、行号与轮次都来自这次采集）。
- 同一轮内的多行是等价的，**不需要**用 `expect_any` 罗列；只有“多个不同轮次都可接受”时才用 `expect_any`。

---

## 3. `queries.jsonl`

每行一个 JSON 对象，UTF-8，空行忽略。字段：

| 字段 | 必填 | 说明 |
|---|---|---|
| `id` | 是 | 集合内唯一字符串，进入报告。 |
| `query` | 是 | 非空查询文本；必须能被分词（至少 2 个 ASCII 字符或 1 个 CJK 字符），否则检索返回 `INVALID_REQUEST`。 |
| `category` | 否 | 分项统计键；缺省 `uncategorized`。建议覆盖 `exact` / `synonym` / `cross_lingual` / `long_session` / `model_missing` / `permission_negative`。 |
| `domain` | 否 | 请求级访问域；缺省 `manifest.domain`。 |
| `limit` | 否 | 单条查询上限（`memory_search.limit`），缺省 5。 |
| `expect` | 二选一 | 单个期望锚点，或空结果反例（见下）。锚点指向**期望轮次**，`line` 只需落在该轮次内。 |
| `expect_any` | 二选一 | 锚点数组；任一对上即算命中。只在**多个不同轮次**都可接受时使用；同一轮内的多行不需要。 |

### 3.1 正向锚点

```json
{"id":"q01","query":"缓存失效策略","category":"exact",
 "expect":{"file":"s01.jsonl","line":5,"contains":"版本号"}}
```

| 字段 | 必填 | 匹配规则（轮次口径） |
|---|---|---|
| `file` | 是 | 命中 title 的文件名必须等于该值（写 basename，不含目录）；缺省的锚点永不匹配。 |
| `line` | 建议 | 用于定位**期望轮次**：该行所在轮次即期望轮次。命中记录只要在同一文件、同一轮次内即算匹配，不要求同一行。 |
| `contains` | 建议 | **标注校验**：该子串必须出现在锚点**轮次内任一记录**（不要求出现在命中记录本身）。若整轮都不含该文本，锚点视为无效、永不匹配（防止行号标错）。 |

```json
{"id":"q01","query":"缓存失效策略","category":"exact",
 "expect":{"file":"s01.jsonl","line":2,"contains":"版本号"}}
```

上例中 `line=2` 是用户提问，`contains="版本号"` 出现在同轮助手回答（第 3 行）。命中第 3 行时按轮次口径算**首命中正确**；严格行级诊断 `first_hit_line_accuracy` 仍会判错。

`expect_any` 示例（两个**不同轮次**都可接受；同一轮内的行不必列出）：

```json
{"id":"q11","query":"怎么让热点数据更快命中","category":"synonym",
 "expect_any":[
   {"file":"s01.jsonl","line":2,"contains":"命中率"},
   {"file":"s01.jsonl","line":4,"contains":"热点数据"}
 ]}
```

锚点的 `file`/`line`/`contains` 全部成立才算该锚点匹配；命中只要匹配任一锚点即可。`expect` 与 `expect_any` 同时出现时取两者锚点的并集。没有任何 `expect*` 的查询会以 0 命中参与 recall（通常是你写错了，工具不报错但指标会很难看）。

### 3.2 空结果 / 权限反例

```json
{"id":"q37","query":"薪资","category":"permission_negative",
 "expect":{"empty":true,
           "no_hits_from_source":"archive-work",
           "source_id":"archive-work",
           "source_status":"unavailable",
           "code":"DOMAIN_UNREACHABLE"}}
```

| 字段 | 说明 |
|---|---|
| `empty` | `true` 表示该查询应得到 0 条结果。 |
| `no_hits_from_source` | 可选；只要**任何**结果来自该 source id 即失败。用于“被拒域的数据不得出现”，同时允许其它可读源给出结果（语义噪声不应被误判成权限泄露）。与 `empty` 同时给出时优先按 `no_hits_from_source` 判定。 |
| `source_id` | 可选；配合 `source_status`/`code` 断言某个 `source_status[]` 条目。 |
| `source_status` | 可选；`ok`/`empty`/`unavailable`/`partial`/`truncated` 等。 |
| `code` | 可选；如 `DOMAIN_UNREACHABLE`、`DOMAIN_DENIED`。 |

负例不计入 recall@5 分母，单独统计 `negative_pass_rate`。source id 由工具按 `<corpus domain>` 生成：`archive-default`、`archive-work`…

主题完全不在语料中的查询应写成严格 `empty` 负例：在**语义可用**时，Archive 的融合后门控（`archive-relevance-v4`）会让这类查询返回空，正常情况下 `memory_search` 退出 0。语义不可用（降级/`--mode none`）时按 P1 词面行为，可能命中偶然二元组，因此严格 empty 负例应在语义模式评估。如果同一配置里还有被拒域来源导致非零退出，必须同时声明期望的 `source_status`/`code`（见 §7），否则按计分细则判失败。建议另加若干“仅零星字面重合”的负例（例如只共享一个中文二元组），用于验证门控而不是词面门槛。

---

## 4. 指标定义（工具固定口径）

在临时运行时根内：`init` → 逐语料 `capture` → 逐 domain `replay`（带 manifest 的 embedding 规格）→ 每条查询 `memory_search` → 对返回的**每个 ref（至多 top-5）** `memory_read`。

| 指标 | 定义 |
|---|---|
| `recall_at_5` | 正向查询中，top-5 内至少一个命中的**(文件, 轮次)**等于任一锚点轮次的比例。分母 = 带正向 `expect`/`expect_any` 的查询数。 |
| `first_hit_accuracy` | 正向查询中 top-1 命中的**(文件, 轮次)**等于任一锚点轮次的比例（即 P0 的“首命中轮次正确率”）。 |
| `recall_at_5_line` / `first_hit_line_accuracy` | **严格行级诊断**（旧口径）：命中行号必须等于锚点 `line`。仅供参考，不参与阈值判定。 |
| `readback_success_rate` | 所有被回读 ref 中，`memory_read` 退出 0、`ok=true` 且返回的 `range_sha256`（单段顶层或任一段）等于 ref 的 `fingerprint.range_sha256` 的比例。分母是 `readback_refs`（所有返回 ref，含负例查询的误命中），分子是 `readback_refs_ok`。 |
| `negative_pass_rate` | 负例（`expect.empty` 或 `no_hits_from_source`）按上述规则全部通过的比例。 |
| `turn_context_contains` | **仅诊断（P4）**：对每个正向查询的 top-1 命中以 `context: "turn"` 回读，检查任一锚点的 `contains` 文本是否出现在组装出的整轮里。输出 `{checked, passed, rate}`；不进分母、不改任何冻结指标。 |
| `latency_ms.cold_start` | 第一条查询的 `memory_search` 墙钟耗时（包含模型/进程冷启动）。 |
| `latency_ms.p95` | 全部查询耗时的 nearest-rank p95；`p95_warm` 排除第一条。 |
| `index_size.ratio` | `retrieval_bytes / corpus_bytes`：`derived/generations/g<N>/retrieval/` 全部文件字节 / `manifest.corpora` 语料文件总字节。向量为 int4 标量量化（每字节两个有符号 4-bit 值加每单元一个 f32 scale，即 `4 + ceil(dimension/2)` 字节/单元；dimension 512 时为 260 B），量化方式写入代际元数据。`unit_ratio` 额外给出与被索引单元文本字节的比值（更严格，仅供参考）。 |
| `categories.<cat>` | 每个分类的查询数、正向数、负例数、轮次 recall@5/首命中、行级 recall@5/首命中、`refs_read/refs_ok` 与回读率、负例通过率。 |

`per_query[]` 保留逐查询证据：`match_ranks`（轮次命中名次）、`match_ranks_line`（行级命中名次）、`expected_turns`、`readbacks[]`（每个 ref 的退出码/`ok`/`range_sha256_matches`）、`turn_context_checked`/`turn_context_contains`（P4 诊断）、`source_status` 与延迟。

`stages` 记录每个 `init/capture/replay` 的退出码与 semantic 状态（`available`/`degraded` + reason + generationKey），便于定位“指标差”是模型降级还是检索排序。任何阶段非 0 退出，工具以 exit 3 失败，不产出伪报告。

---

## 5. 运行

```powershell
# 语义跑（manifest 指定的模型）
python assets\scripts\recall_eval.py --set <eval-set> --out report.json

# 词面基线（同语料、同查询，去掉 embedding）
python assets\scripts\recall_eval.py --set <eval-set> --mode none --out lexical.json

# 指定二进制/解释器/保留临时根
python assets\scripts\recall_eval.py --set <eval-set> --binary <agent-memory.exe> --python <python.exe> --keep
```

- 默认二进制优先级：`AGENT_MEMORY_BIN` → `agent-memory/target/release` → `agent-memory/target/debug` → `assets/dist/agent-memory`。分发物是旧阶段产物时请显式 `--binary`。
- 退出码：`0` 报告已产出（指标好坏不影响退出码）· `2` 用法/评估集非法（缺 `manifest.json`、schema 不符、语料或查询不存在、查询 id 重复等）· `3` 某个阶段失败（init/capture/replay 非 0，报告带 stage 与 stderr 摘要）。
- `--mode none` 时 `embedding` 块不会进入配置，replay 写纯词面代际；此时 `model_missing` 类查询仍应通过，用来验证“模型缺失时词面路径不中断”。

---

## 6. 独立构造 held-out 集的步骤

1. 新建目录与 `corpus/`；写合成 JSONL（§2），每个主题一个文件，长会话至少 40 行且目标轮次不在开头。
2. 写 `manifest.json`：固定 schema、选模型/维度/后端、把所有语料列入 `corpora`；需要权限反例时给一条语料 `domain: "work"`。
3. 写 `queries.jsonl`（§3）：至少 40 条，覆盖精确、同义、中英跨语言、长会话、模型缺失、权限反例；正向查询标注 `file`+`line`+`contains`（`line` 指向期望轮次内任一行，`contains` 必须在该轮内出现）；只有多个不同轮次都可接受时才用 `expect_any`。负例至少包含 1 条严格 `empty` 与 1 条 `no_hits_from_source`。
4. 先跑 `--mode none` 确认语料/轮次标注正确（精确项应命中；若精确项都失败，先修标注）。注意区分两套指标：轮次指标用于阈值，`*_line` 只是行级诊断。
5. 再跑语义模式；把 `stages.replay[].semantic`、`index_size.ratio`、`latency_ms` 与各分类指标一起存档。
6. **不要**用开发集 `agent-memory/tests/fixtures/hybrid-eval-dev/` 调参后再把同一集当 held-out；held-out 集在实现冻结后才首次运行。

### 最小完整示例

`manifest.json`：

```json
{
  "schema": "agent-memory/hybrid-eval-set@1",
  "name": "example",
  "domain": "default",
  "embedding": {
    "model_id": "qwen3-embedding:4b",
    "dimension": 512,
    "normalization": "l2",
    "backend": "ollama"
  },
  "corpora": [{"file": "corpus/s01.jsonl", "provider": "codex", "domain": "default"}],
  "queries": "queries.jsonl"
}
```

`corpus/s01.jsonl`：

```json
{"type":"session_meta","payload":{"id":"s1"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":"缓存命中率一直上不去，怎么排查？"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":"把整表缓存改成按记录缓存，并用版本号做失效标记。"}}
```

`queries.jsonl`：

```json
{"id":"q01","query":"缓存命中率","category":"exact","expect":{"file":"s01.jsonl","line":2,"contains":"缓存命中率"}}
{"id":"q02","query":"缓存粒度怎么降","category":"synonym","expect":{"file":"s01.jsonl","line":3,"contains":"版本号"}}
{"id":"q03","query":"needle-not-in-corpus","category":"permission_negative","expect":{"empty":true}}
```

这三条足以验证工具链；held-out 集请按 §6 扩展规模与类别。

## 7. 计分细则补充

- 只有正向查询（无 `empty`、无 `no_hits_from_source`）进入 `recall_at_5` / `first_hit_accuracy` 分母；两类负例只进入 `negative_pass_rate`。
- 负例要求 `memory_search` 返回完整响应信封（含 `contract_version` 与 `results` 数组）；某些源 `partial`/`unavailable` 导致的非零退出（如 4、10）属正常检索结果，不影响判定；信封缺失或无法解析即视为检索失败，负例不通过。
- 回读单次上限 1 MiB（`max_bytes`），超出按 P0 口径记为回读失败。
- 报告 `config.binary` / `config.binary_sha256` 记录被测二进制，便于复核。
