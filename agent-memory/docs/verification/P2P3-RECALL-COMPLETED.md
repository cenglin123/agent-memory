# P2/P3 最小统一回忆 · 完工记录

> **状态更新（验收后）**：本记录生成时两仓改动尚未提交；随后由 orchestrator 提交为套件 `6e824c1`
> 与 vault `810523a0`。验收发现的问题与修复见 `P2P3-FIX-COMPLETED.md`（F1 严格快照、F3 `-` 开头 query、
> F2 空正文 device ref、F4 KB CRLF 证据等）。下文第 9 行的"未执行任何 `git commit`"仅描述生成时状态。

结论：**P2（KB/device 接入与三源聚合）与 P3（CLI + stdio MCP）已实现并通过全部合成测试。**
真实知识库端到端、真实会话试查、`kb_python_baseline.json` 导出仍未执行（本任务明确禁止），因此本记录只声明"契约与合成 fixture 全部验证"，不声明"真实库已验证"。

- 套件 worktree 仓：`<suite-worktree>`（仓根相对路径 `agent-memory/`）
- vault worktree 仓：`<vault-worktree>`（分支 `kb-task/20261025-115042-0ce610d5`）
- 计划：`<vault-worktree>/docs/plans/active/20261025-最小统一回忆.md`（最终版）
- **未执行任何 `git commit`**（按任务约束）。所有改动留在工作区，两仓均为可审阅状态。

---

## 1. 两仓修改清单

### 1.1 套件仓 `<suite-worktree>/agent-memory/`

| 文件 | 类型 | 内容 |
|---|---|---|
| `src/recall/providers/kb.rs` | 新增 | KB 适配器：按 `interpreter.kb.command/args/env` 启动 `<kb.root>/.meta/scripts/ask.py --json --no-log --top-k <n> <query>`；子进程硬超时 `max(1000, min(18000, max_elapsed-2000))`，到点 kill；stdout 必须**恰好一个** JSON 文档；`contract_version=1`、`vault_root` 规范化比较、`detail.mode ∈ {dense,hybrid,rerank}` → `score_kind`/`rank_basis`；`degraded`/`detail.rerank.skipped`/`detail.graph_fallback` → `warnings[]`；`chunk_text` 在**当前文件原始字节**中整段唯一匹配生成 `text_range`，0 次/多次或 `chunk_index=null` → 剔除 + `CONTENT_UNRESOLVED` + `partial`；ask 退出码映射表 `ASK_EXIT_TABLE`；观测根整树前后 diff，已知 A/B 路径命中 → `SIDE_EFFECT_VIOLATION` + `error` |
| `src/recall/providers/device.rs` | 新增 | 配置根下的**直接子文件** + `^\d{4}-\d{2}-\d{2}_.+\.md$`；隐藏项/子目录/symlink 排除、非法日历日期 `RECORD_SKIPPED`；文件名+正文同一词面命中；正文命中取首命中行字节区间（CRLF 视为一个行尾，两者都不含），**仅文件名命中**取正文首块 `[0, min(首行末, 4096))`（UTF-8 边界对齐）；`score=null`/`score_kind=none`/`rank_basis=lexical_window`；源内名次沿用 P1 `rank_hits` |
| `src/recall/ops.rs` | 新增 | CLI/MCP 唯一执行入口：`Op::{Search,Read}` + `execute()` → `{value, exit_code, is_error}`；请求/配置失败的冻结信封 |
| `src/mcp.rs` | 新增 | stdio JSON-RPC 2.0（行分隔）：`initialize`/`ping`/`shutdown`/`tools/list`/`tools/call`；仅暴露 `memory_search`、`memory_read`，均 `readOnlyHint:true`；`-32602`（INVALID_REQUEST/INVALID_OFFSET，`data` 带冻结信封 + `isError:true`）、`-32601`（未知方法/未知工具）、`-32700`（解析失败） |
| `src/recall/search.rs` | 修改 | 接入 KB/device；删除 `adapter_pending`；按源聚合 `warnings[]`；`score`/`score_kind`/`rank_basis` 由命中携带（KB 来自 `detail.mode`）；新增 `SearchOptions{strict_snapshot}` + `memory_search_with`（旧 `memory_search` 签名不变） |
| `src/recall/read.rs` | 修改 | 新增 `ReadOptions{strict_snapshot}` + `memory_read_with` + `ReadResponse::request_failure`；`read_root`（严格快照副本或配置根）与 `db`/`storage` 副本内重映射；KB/device 的 `md-v1` 回读沿用 P1 区间校验 |
| `src/recall/providers/opencode.rs` | 修改 | 新增 `search_at(..., read_root, observe_root)`（旧 `search` 保留为包装）；`db`/`storage` 在严格快照下重映射到副本；命中携带 `score/score_kind/rank_basis` |
| `src/recall/providers/mod.rs` | 修改 | `SourceHit` 增加 `score`/`score_kind`/`rank_basis`，`SourceOutcome` 增加 `warnings`；删除 `adapter_pending` |
| `src/recall/contract.rs` | 修改 | 新增 KB 子契约常量（`KB_CONTRACT_VERSION`、`ALL_KB_MODES`、`score_kind_for_mode`/`rank_basis_for_mode`、`ASK_EXIT_*`、`kb_hard_timeout_ms`、`DEVICE_FILE_PATTERN`、`DEVICE_FILENAME_BLOCK_MAX_BYTES`、`MCP_TOOL_NAMES`、`IS_ERROR_CODES`/`is_error_code`）；`WarningEntry` 可选键按契约省略 |
| `src/recall/snapshot.rs` | 修改 | `StrictSnapshot`：把源根复制到临时目录（上限 512 MiB / 200 000 项，超限显式失败），Drop 清理；不进入 `side_effects[]`（原根从未打开） |
| `src/recall/port/reader.rs` | 修改 | `rel_from_root` 改为 `pub`（严格快照重映射用）；解析逻辑与语义不变 |
| `src/recall/mod.rs` | 修改 | 导出 `ops`、`memory_search_with`、`memory_read_with`、选项结构 |
| `src/lib.rs` | 修改 | `pub mod mcp;` |
| `src/bin/agent-memory.rs` | 修改 | 新增 `memory_search` / `memory_read`（`--config`、`--strict-snapshot`、`--request <json\|@file>`，缺省读 stdin）与 `mcp` 子命令；退出码取冻结信封推导值；原有 archive 子命令行为不变 |
| `tests/fixtures/recall/kb_stub/ask.py` | 重写 | 分支改由 `interpreter.kb.env` 的 `KB_STUB` 选择（解释器参数与脚本路径顺序与真实调用一致）；`vault_root` 由自身位置推导；覆盖 `ok/ok-empty/degraded/rerank-timeout/graph-fallback/escalated/chunk-missing/chunk-zero/chunk-two/root-mismatch/mode-missing/no-version/bad-version/error/profile/auth/unknown-exit/exit2-empty/plain-text/two-documents/timeout/writes-log/writes-log-empty/writes-cache` |
| `tests/recall_kb_device.rs` | 新增 | 18 个用例：KB ok 命中/回读、空源、三类告警、升级、退出码映射、契约破坏、chunk 不可回读、硬超时、A/B 泄漏（有结果 10 / 无结果 4）、C 级缓存、device 白名单/文件名命中/无关词/CRLF+多字节区间、三源交错、稳定分页、部分失败、严格快照 |
| `tests/recall_cli_mcp.rs` | 新增 | 6 个用例：CLI/MCP 同一信封、同一 ref 回读一致、`INVALID_*` → 退出 2 + `-32602`、只读工具白名单（无写工具）、MCP 运行中不阻塞 CLI、`--strict-snapshot` 在副本内跑 KB 子进程 |
| `tests/recall_opencode.rs` | 修改 | 占位用例 `pending_adapters_are_explicit_not_empty` → `adapters_report_real_statuses_instead_of_a_placeholder`（KB 缺 `ask.py` → `unavailable`/`SOURCE_ROOT_INVALID`；device 无白名单事件 → `empty`/无 code/退出 0），断言更强而非更弱 |
| `tests/support/mod.rs` | 修改 | 新增 `tree_state()`（path+size+mtime，只 stat）供"读取不改变根/严格快照不动原根"断言 |
| `README.md` | 修改 | 新增 KB/device/聚合/传输/严格快照说明、CLI 用法与退出码、测试计数 |

未改动：`Cargo.toml` / `Cargo.lock`（依赖白名单未变，无 `tauri`/GUI）、`NOTICE`、`port/` 既有原语、P0/P1 全部用例逻辑。

### 1.2 vault worktree 仓 `<vault-worktree>/`

| 文件 | 类型 | 内容 |
|---|---|---|
| `.meta/scripts/ask.py` | 修改 | ① `--json`：`contract_version=1`/`ok`/`vault_root`/`degraded`/`detail{mode,escalated[],rerank?,graph_fallback?}`/`results[{path,score,chunk_index,chunk_text?}]`；stdout 唯一出口（`_emit_json` 一个文档后以自身退出码退出），全部诊断走 stderr；② `--json` 分支**前置**到 `mode_label` 回显与 `--check` 标题行之前（两者在 JSON 模式下不执行）；`print_results`/`--orphans`/`--backlinks`/`--neighbors`/`--path`/`--deep`/`--save`/`--bm25` 在 `--json` 下显式拒收（退出 2 信封）；③ 查询链抽成 `_retrieve()`，文本模式与 JSON 模式**共用**同一实现（dense/hybrid/bm25 + `auto_escalate` + `rerank` 顺序不变）；④ `_open_db_ro()` 唯一直读入口（`file:<as_posix>?mode=ro` + `uri=True`，缺库抛 `LocalUnavailableError` 不建文件）；⑤ `_hit_chunk_text()`：原始字节→UTF-8→LF 归一→`embed.chunk_text()`→第 `chunk_index` 段，且与**活动代**（`active_retrieval_generation`）`corpus_chunk_manifest.content_hash` 一致才回 `chunk_text`，否则 `chunk_index=null` 且不输出（无首块兜底）；⑥ 升级只写 `detail.escalated[]`（`mode` 变 `hybrid`）；rerank 降级只置 `degraded:true`（不再打印第二个 JSON 文档）；两处 rerank 超时折叠为 `detail.rerank={skipped:"timeout",elapsed_ms}`；⑦ graph/mtime 回退只写 `detail.graph_fallback`；⑧ `--no-log`（`--json` 下默认）门控 `log_script_run`/`co_retrieval.record`/`_log_escalation`/`_log_rerank_timeout` 四处，无 `--log` 反开关；⑨ `--json` 下先 `sys.stdout.reconfigure(encoding='utf-8')`；⑩ 默认文本模式行为、排序与文本输出不变（`_text_out()` 仅为 JSON 门控） |
| `.meta/tests/test_ask_readonly_json.py` | 新增 | 22 个用例（临时 SQLite + 临时"vault"，零网络）：只读连接/缺库不建 0 字节文件/WAL 不修改主库、JSON 单文档（成功+三类失败）、`contract_version`、升级只进 detail、`degraded` 只置位、rerank 超时门控与折叠、四处遥测门控、chunk 门控（越界/缺失/哈希不符/双代只取活动代）、ask code 与退出码 12/13/16/2、GBK 控制台下仍单文档、静态断言（唯一 `sqlite3.connect` 在 helper 内、无 `init_db(`、JSON 出口无裸 `print`、`print_results` 不在 JSON 链上、JSON 分支在人类标签之前） |

未改动：`embed.py`、`build_graph.py`、`bm25_index.py`、`common.py`、`co_retrieval.py` 及任何检索/排序常量。

---

## 2. 契约映射（实现即常量，测试断言）

- **`score_kind`/`rank_basis`**：只由 ask 信封 `detail.mode` 映射（`dense→native_dense`、`hybrid→native_hybrid`、`rerank→native_rerank`），缺失/越界 → `KB_QUERY_FAILED`（`detail.reason=ask_mode_invalid`）。
- **ask 退出码**：12→`DB_UNAVAILABLE`/`unavailable`、13→`SCHEMA_UNSUPPORTED`/`unsupported`、16→`KB_QUERY_FAILED`/`error`、2(有信封)/1/14/15/未列出→`KB_QUERY_FAILED`/`error`、2(空 stdout)→`KB_CONTRACT_UNSUPPORTED`/`unsupported`（`detail.reason=ask_json_unsupported`）。
- **`warnings[]`**：`degraded:true`→`WARN_RERANK_UNAVAILABLE`；`detail.rerank.skipped="timeout"`→`WARN_RERANK_TIMEOUT`；`detail.graph_fallback:true`→`WARN_GRAPH_FALLBACK`；均不改 `source_status`/`next_offset`/退出码/`isError`。
- **失败剔除**：`chunk_index` 缺失或 `chunk_text` 未恰好出现 1 次 → 不入 `results`，计 `detail.CONTENT_UNRESOLVED`，源 `partial`。
- **A/B 泄漏**：观测根 diff 命中已知遥测路径 → `kb` 置 `error` + `errors[{code:SIDE_EFFECT_VIOLATION,source_id:"kb"}]`；有结果 10、无结果 4（两用例）。
- **MCP `isError`**：`INVALID_REQUEST`/`INVALID_OFFSET`/`KB_CONTRACT_UNSUPPORTED`/`REF_*` → `true`；`partial`/`truncated`/`warnings[]` → `false`。

---

## 3. 测试命令与实测退出码

| 命令（工作目录） | 结果 | 退出码 |
|---|---|---|
| `cargo test`（`<suite-worktree>/agent-memory`） | 257 passed / 0 failed / 1 ignored（含 5 个 compile-fail doctest） | **0** |
| `cargo test --doc`（同上） | 5 passed / 0 failed | **0** |
| `cargo tree -p agent-memory --depth 1`（同上） | 直接依赖未变、无 `tauri` | **0** |
| `python .meta/tests/test_ask_readonly_json.py`（`<vault-worktree>`） | `Ran 22 tests ... OK` | **0** |

新增用例分布：`tests/recall_kb_device.rs` 18、`tests/recall_cli_mcp.rs` 6、`src/recall/providers/kb.rs` 单元 6、`src/mcp.rs` 单元 3、`src/recall/providers/device.rs` 单元 3。

> 说明：`python -m unittest .meta/tests/test_ask_readonly_json.py` 在本机不可用——`.meta` 以点开头不是合法模块路径（`ValueError: Empty module name`），且未安装 pytest。该文件本身是标准 `unittest` 脚本，直接以文件路径运行即等价（上表命令）。

---

## 4. 已知限制与偏差（均不阻断交付）

1. **`rank_basis`/`source_rank` 的位置**：任务文字提到 `results(... rank_basis/source_rank)`，实现按计划 §3.4"结果面**只回** `path/score/chunk_index/chunk_text?`"落地——`rank_basis` 由 recall 依 `detail.mode` 派生（该模式是唯一生产者，避免双权威漂移），`source_rank` 由 recall 的源内名次生成（1-based）。两条均在 `memory_search` 响应中可见并有用例断言。
2. **`content_hash` 校验归属**：哈希门控由 ask.py 执行（计划 §3.4 第 4 条的唯一权威）；Rust 侧校验 `chunk_index` 存在性并做"整段唯一字节匹配"生成 `text_range`，不重复计算哈希。
3. **真实库/真实 KB 端到端未执行**（任务禁止）。已用 Python 临时 fixture 验证生产端契约、用 `kb_stub` 验证消费端契约；真实 `ask.py` 在主库上的单次只读端到端仍属 `integrate` 之后的单独授权项。真实 `vault_root` 若是 junction/`\\?\` 别名，会按契约报 `KB_ROOT_MISMATCH`（配置根与脚本自我定位必须同一物理路径）。
4. **`--strict-snapshot`** 复制整棵观测根（512 MiB / 200 000 项上限），超限显式 `DB_UNAVAILABLE`（不静默回退读原根）。大库下该模式成本与库体积成正比，且每次调用重新复制；`side_effects[]` 为空（原根未打开）。
5. **KB 观测根窗口**每次查询对 `kb.root` 做一次整树 `stat` 快照（计划 §2.3 要求），成本与文件数成正比；空 `side_effects[]` 仍是正常态。
6. **既有失败（与本次改动无关，未修）**：`python .meta/tests/test_ask_rerank.py` 2 个用例失败（`common.rerank_with_llm` 的排序期望与实现不符）；该文件只导入 `common`，不导入 `ask.py`，故与本次改动无关。
7. **`__pycache__`**：直接以源码方式导入 `ask.py` 的测试会生成 `.meta/scripts/__pycache__`（git 已忽略）。生产路径由 `interpreter.kb.args=["-B"]` + `PYTHONDONTWRITEBYTECODE=1` 消除。
8. **P0/P1 用例替换**：`pending_adapters_are_explicit_not_empty` 断言的是 `adapter_pending_p2` 占位本身，占位按任务要求删除，故替换为真实状态断言（KB 缺脚本 `unavailable`、device 空源 `empty`）。其余 P0/P1 用例未改。
9. **未提交**：按任务约束未执行 `git commit`；两仓工作区保留全部改动供审阅。
10. **cua-driver / 套件其它部分未触碰**：本任务只覆盖 `agent-memory/**` 与 vault 两个文件，未改安装脚本、技能、CHANGELOG 与 Archive 计划。

---

## 5. 未完成即阻断的判定

无阻断项。计划 §6 中"真实库对比、真实会话试查、`kb_python_baseline.json` 一次性导出、KB 真实端到端"四项按任务约束不在本次范围，已在上文第 3 条显式登记为"待单独授权"，不冒充已完成。
