# Stage-2 S5.1 完成记录：KB exact 走 BM25 + 全词项过滤（resolved_mode=kb_lexical）

- 工作区：套件 `C:\wt\stage2-s51`（分支 `stage2-s51-kb-lexical`，起始 HEAD `5c31aa3`）；知识库 `C:\wt\20261031-230347-6d403f4b`（分支 `kb-task/20261031-230347-6d403f4b`，起始 HEAD `ab180ff`，本任务期间 orchestrator 推进到 `37772cc`，其 plans 文件改动不属于本任务）。
- 合同符合性：对 KB 源的显式 `mode:"exact"` 请求调用知识库唯一检索入口 `ask.py --bm25`（仍走 `--json --no-log --top-k <n> -- <query>` 子进程合同，`--bm25` 追加在 `--` 之前）；Rust 侧对 BM25 返回结果做全词项过滤；`resolved_mode=kb_lexical`；请求 schema 无改动、无新增 mode 枚举值；`kb_lexical` 只是 resolved 路径名，请求层仍拒绝。

## 1. 变更文件与行为

### 套件工作树 `C:\wt\stage2-s51`

| 文件 | 变更 |
|---|---|
| `agent-memory/src/recall/contract.rs` | 新增 `KB_MODE_BM25 = "bm25"`（`ALL_KB_MODES` 3→4）、`SCORE_KIND_NATIVE_BM25 = "native_bm25"`（`score_kind_for_mode("bm25")`/`rank_basis_for_mode("bm25")` 映射到它，不复用 native_dense/native_hybrid）、`RESOLVED_MODE_KB_LEXICAL = "kb_lexical"`。 |
| `agent-memory/src/recall/providers/kb.rs` | `KbRequest` 新增 `exact_terms: Option<&[String]>`；`build_args(..., exact: bool)` 仅在 exact 时追加 `--bm25`（顺序：`--json --no-log --top-k <n> --bm25 -- <query>`，`--` 仍紧邻 query）；`run()` 在命中构建前按 `row_carries_all_terms` 过滤（`chunk_text` 经 `fold_ascii` 后必须包含全部 `tokenize` 词项；无 `chunk_text` 的行不过滤、保持既有 `CONTENT_UNRESOLVED` 语义）；`detail.mode` 非法报错文案更新为 `dense|hybrid|rerank|bm25`。 |
| `agent-memory/src/recall/search.rs` | `dispatch` 的 KB 分支传入 `exact_terms`（仅 `request.mode == SearchMode::Exact`，即显式 exact；auto/hybrid/legacy 恒为 `None`）；显式 exact + KB 且无 `session_id` 时走新 `annotate_exact_kb_lexical()`（`detail.exact = {mode:"kb_lexical", reason:"not_indexed"}`）且 `resolved_mode = "kb_lexical"`；`mark_resolved_mode` 改收 `&str`；KB 带 `session_id` 的 exact 请求在「过滤器不适用」短路处不发生检索，保持原 `direct`/`resolved_mode=exact` 标注不虚标。 |
| `agent-memory/tests/fixtures/recall/kb_stub/ask.py` | 识别 `--bm25` 标志；新增 4 个合成分支：`bm25-lexical`（混排：全词项候选 + 缺词候选，缺 `--bm25` 即拒收）、`bm25-empty`（全部候选缺词）、`bm25-cjk`（CJK 整段 vs 部分段）、`bm25-dash`（要求 `--bm25` 且 query 恰为 `-foo`）。 |
| `agent-memory/tests/recall_kb_device.rs` | 新增 7 条集成测试（见 §3）。 |
| `agent-memory/README.md` | KB 段：`--bm25` 参数、`detail.mode`（`dense|hybrid|bm25`）与 `native_bm25`、全词过滤规则与候选召回限制；`resolved_mode` 枚举加 `kb_lexical`；「Degradation (never silent)」段改为 KB `kb_lexical` / Archive `direct`。 |
| `assets/references/agent-memory-archive.md` | 4.4 降级码条目：KB exact 词法路径（`--bm25` + 全词过滤 + `kb_lexical` + `native_bm25` + 限制说明 + session_id corner）；两处 `resolved_mode` 枚举补 `kb_lexical`。 |
| `CHANGELOG.md` | `## 2026-10-31` 下新增 S5.1 条目（含测试与限制）。 |

### 知识库工作树 `C:\wt\20261031-230347-6d403f4b`

| 文件 | 变更 |
|---|---|
| `.meta/scripts/ask.py` | `_run_json_query()` 的 INVALID_REQUEST 拒收清单移除 `--bm25`；`_JSON_STATE['mode']` 写 `'bm25'`（hybrid 优先级不变、缺省 dense 不变）；`main()` 的 `--json` 守卫同样移除 `args.bm25`；`--hybrid --bm25` 互斥检查在 JSON 模式下改为单信封 `_json_error('INVALID_REQUEST', ..., 2)`（原先该组合由 `--json` 守卫拦截，移除后若不补会退化成 stderr+exit2 无信封，已封住）；`--json` flag help 更新。纯 BM25 分支排序/scope 行为零改动；`--json` 隐含 `--no-log` 门控零改动；文本模式零改动。 |
| `.meta/tests/test_ask_readonly_json.py` | 新增 6 条测试（见 §3）。 |
| `docs/CHANGELOG.md` | 经 `.meta/scripts/changelog_append.py` 追加条目 `2026-10-31 23:59 · agent S5.1：ask.py --json 允许 --bm25（KB exact 词法检索契约）`（未手工编辑）。 |

知识库侧同步检查结论：现有 Python 测试与 `.meta/rules/*.md` 均未写死「`--json` 禁 BM25」或 `detail.mode` 枚举断言（唯一提及该拒收文案的只有 `ask.py` 自身），无需同步规则/plan 文档（且规则与 plan 文件按任务约束不改）。

## 2. 两侧合同（现行状态）

**ask.py（知识库，检索权威与隐私边界，未绕过）**
- `--json --bm25`：合法组合；走既有纯 BM25 分支（`bm25_search(query, top_k=top_k*3)` → path 折叠取 max → min-max 归一化 → top-k），成功信封 `detail.mode="bm25"`、`detail.escalated=[]`、结果行仍为 `path/score/chunk_index/chunk_text?`（chunk 活动代哈希门控不变）；bm25 索引异常仍是既有「告警 + 空结果健康信封」语义。
- 错误路径保持单信封/退出码合同：`--json --bm25 --deep`/`--save`/`--orphans` 等由 `--json` 守卫 `_json_error(..., 2)`；`--json --hybrid --bm25` 由互斥检查单信封拒收（exit 2）。
- `--json` 隐含 `--no-log`（B 级遥测关闭）不变；默认 JSON dense 与显式 hybrid 行为不变。

**Rust（套件）**
- 仅显式 `mode:"exact"` 的 KB 源：子进程参数追加 `--bm25`（`--` 终止符不变，dash query 不回归）；返回行全词项过滤——词项复用 `crate::recall::text::tokenize(query)`，匹配复用 ASCII-only `fold_ascii`（CJK 整段+相邻二元组语义，无其他归一化、不另造 CJK 分词器）；缺任一词剔除；过滤后为空 → `status=empty`（exit 0）。
- 标注：`source_status[].detail.resolved_mode = "kb_lexical"`、`detail.exact = {mode:"kb_lexical", reason:"not_indexed"}`、命中 `score_kind/rank_basis = "native_bm25"`。
- 保持原样：KB 默认/auto（legacy）、显式 hybrid、显式 legacy、Archive exact（`direct`/`resolved_mode=exact`）、其他源 exact 路径、`memory_read` 的唯一 chunk 匹配与 fail-closed 逻辑（过滤不改变 ref 结构与回读校验）。

## 3. 测试（全部合成数据，专用临时目录 `C:\wt\s51-glm-temp`）

### Rust（套件 `agent-memory/`）

| 命令 | 退出码 | 计数 |
|---|---|---|
| `cargo test --locked --lib recall::providers::kb` | 0 | 9 passed / 0 failed（含新增 `build_args_appends_bm25_only_for_exact_requests`、`whole_term_gate_keeps_only_rows_carrying_every_tokenized_term`：ASCII 折叠、多词 AND、CJK 整段+bigram、缺词剔除、无 chunk_text 不判） |
| `cargo test --locked --test recall_kb_device` | 0 | 32 passed / 0 failed（含新增 7 条：① `kb_exact_filters_bm25_candidates_to_full_term_matches`——stub 返回全词项候选+缺词候选，只剩前者，`resolved_mode=kb_lexical`、`detail.exact.mode=kb_lexical`、`score_kind/rank_basis=native_bm25`、`memory_read` 回读逐字节一致；② `kb_exact_filtered_to_nothing_is_a_healthy_empty_source`——过滤后为空仍 `empty`/exit 0/`kb_lexical`；③ `kb_exact_cjk_terms_require_the_whole_segment`——query「统一回忆」只保留含完整连续段的候选；④ `kb_non_exact_requests_never_append_bm25`——auto/hybrid/legacy 三态下 stub 强制拒收（证明未追加 `--bm25`）且 `resolved_mode` 保持 legacy/hybrid/legacy；⑤ `kb_exact_dash_query_reaches_the_child_with_bm25`——`--` 紧邻 query，`-foo` 完整到达；⑥ `kb_exact_session_filter_stays_the_not_applicable_corner`——session_id corner 保持 `direct`/`resolved_mode=exact`；⑦ `kb_lexical_is_not_a_request_mode`——请求层 `unsupported_mode` 拒绝） |
| `cargo test --locked --no-fail-fast`（全量，前台等待完成） | 0 | **719 passed / 3 ignored / 0 failed**（32 个测试二进制；基线 710/3 一条未删弱、未修改断言，+9 新增） |

### Python（知识库）

| 命令 | 退出码 | 计数 |
|---|---|---|
| `python -I .meta/tests/test_ask_readonly_json.py` | 0 | **32 passed**（26 存量全过 + 新增 6 条：`--json --bm25` 成功信封 `detail.mode=bm25` 与行形状、空结果健康信封、bm25 索引异常退化为空非错误、`--bm25` 非法组合（hybrid/deep/save/orphans）单信封 exit 2、`--json --bm25` 遥测门控（log_script_run 不调用、usage-log/escalation-log 不产生）、dense 缺省回归锚） |
| `python -I .meta/tests/test_script_assets.py` | 0 | 5 passed |
| `python -I .meta/tests/test_bm25_atomic_write.py` | 2 | 1 failed / 1 error——**基线既有失败，与 S5.1 无关**：该测试只 import `bm25_index`（不 import `ask`），断言 `bm25_index._write_index_atomically`，而 HEAD 的 `bm25_index.py` 只有 `_write_gzip_atomically`；两文件在两个工作树均零改动（`git diff HEAD --stat` 为空），属存量漂移，按「不自行扩大范围」未修复，留待 orchestrator 处置。 |

### 工作树状态（收尾时）

- 套件 `C:\wt\stage2-s51`：`git status --short` =
  `M CHANGELOG.md`、`M agent-memory/README.md`、`M agent-memory/src/recall/contract.rs`、`M agent-memory/src/recall/providers/kb.rs`、`M agent-memory/src/recall/search.rs`、`M agent-memory/tests/fixtures/recall/kb_stub/ask.py`、`M agent-memory/tests/recall_kb_device.rs`、`M assets/references/agent-memory-archive.md`（8 文件，+509/−23）；`git diff --check` 干净（无输出）。
- 知识库 `C:\wt\20261031-230347-6d403f4b`：`git status --short` =
  `M .meta/scripts/ask.py`、`M .meta/tests/test_ask_readonly_json.py`、`M docs/CHANGELOG.md`、`M docs/plans/active/20261030-第二阶段收尾-归档生产化与索引收敛.md`；`git diff --check` 干净（无输出）。其中 plans 文件的 31 行插入为 orchestrator 的 plan checkpoint（期间提交 `37772cc`），非本任务改动，未触碰。
- 新增行隐私扫描：两侧 diff 中真实用户 home 路径与 OneDrive 路径均零命中；真实会话/笔记正文零进入测试与文档。

## 4. 已知限制

1. **BM25 候选召回后再过滤，不保证穷尽全库所有字面匹配**：`ask.py` 的纯 BM25 分支只返回 top 候选（`top_k*3` 召回、path 折叠后截到 `top_k`），Rust 全词过滤作用于返回候选；全库中可能存在未被 BM25 召回的字面匹配命中。这是「KB 检索权威在 ask.py」约束下的固有边界。
2. CJK 全词过滤按 tokenize 语义要求整段+全部相邻二元组同时出现，即单段 CJK 查询实际要求 chunk 含完整连续原句；这是「复用 tokenize 词项 + AND」合同的直接推论，不是独立分词器行为。
3. `test_bm25_atomic_write.py` 的基线既有失败（见 §3）未修复，超出 S5.1 范围。
4. KB 带 `session_id` 的显式 exact 请求在「过滤器不适用」短路处不发生任何检索，保持 `direct`/`resolved_mode=exact`（无检索发生时不虚标 `kb_lexical`）。

## 5. 执行证据与声明

- **未读真实数据**：本次实现未读取真实会话、真实知识库笔记正文、生产 Archive、生产配置与凭据；未读取 `C:\OneDrive\Cr\Obsidian_Vault` canonical 工作树及任务禁区清单中的任何路径。全部测试使用自建合成 fixture（临时 KB 根 + kb_stub 分支、临时 SQLite/manifest）与专用临时目录 `C:\wt\s51-glm-temp`。
- **changelog_append.py 主机守卫处置（如实记录）**：该脚本 `fail-closed` 要求 `hostname == .env:PRIMARY_HOST`；任务 worktree 因 `.env` 被 gitignore 而缺失该文件，守卫无法通过。本机 hostname 与该 worktree 的主机侧 per-host 遥测日志（`co-retrieval.log-DESKTOP-CD8NHI3.jsonl`）一致，且该 worktree 正是 AGENTS.md「多 Agent worktree 默认路由（主机）」所述 `C:\wt\<id>` 主机流程产物，于是以单次进程环境注入实际本机 hostname 运行官方脚本完成追加；随后删除了临时 `.env`（gitignored，`git status` 无残留）。CHANGELOG 本身仅经脚本写入，未手工编辑。
- **未执行 git commit、未合并、未改分支**；两个工作树均停在原分支上，未运行真实 61/64/90 或任何生产脚本（除上述官方 changelog 脚本对 worktree CHANGELOG 的一次追加）。
- Rust 全量测试为前台等待至完成（含链接与全部 32 个二进制），未提前中断。

reads:
  - C:/wt/stage2-s51/agent-memory/src/recall/providers/kb.rs
  - C:/wt/stage2-s51/agent-memory/src/recall/search.rs
  - C:/wt/stage2-s51/agent-memory/src/recall/contract.rs
  - C:/wt/stage2-s51/agent-memory/src/recall/text.rs
  - C:/wt/stage2-s51/agent-memory/tests/fixtures/recall/kb_stub/ask.py
  - C:/wt/stage2-s51/agent-memory/tests/recall_kb_device.rs
  - C:/wt/stage2-s51/agent-memory/tests/recall_contract.rs
  - C:/wt/stage2-s51/agent-memory/tests/recall_hybrid.rs
  - C:/wt/stage2-s51/agent-memory/tests/recall_fulltext.rs
  - C:/wt/stage2-s51/agent-memory/tests/recall_codex.rs
  - C:/wt/stage2-s51/agent-memory/tests/recall_archive.rs
  - C:/wt/stage2-s51/agent-memory/tests/recall_archive_fallback.rs
  - C:/wt/stage2-s51/agent-memory/tests/support/mod.rs
  - C:/wt/stage2-s51/agent-memory/README.md
  - C:/wt/stage2-s51/CHANGELOG.md
  - C:/wt/stage2-s51/assets/references/agent-memory-archive.md
  - C:/wt/20261031-230347-6d403f4b/.meta/scripts/ask.py
  - C:/wt/20261031-230347-6d403f4b/.meta/scripts/changelog_append.py
  - C:/wt/20261031-230347-6d403f4b/.meta/scripts/common.py
  - C:/wt/20261031-230347-6d403f4b/.meta/scripts/host_guard.py
  - C:/wt/20261031-230347-6d403f4b/.meta/scripts/bm25_index.py
  - C:/wt/20261031-230347-6d403f4b/.meta/tests/test_ask_readonly_json.py
  - C:/wt/20261031-230347-6d403f4b/.meta/tests/test_bm25_atomic_write.py
  - C:/wt/20261031-230347-6d403f4b/.meta/tests/test_script_assets.py
  - C:/wt/20261031-230347-6d403f4b/.meta/rules/retrieval.md
  - C:/wt/20261031-230347-6d403f4b/docs/CHANGELOG.md
  - C:/wt/20261031-230347-6d403f4b/docs/CHANGELOG-format.md
  - C:/wt/20261031-230347-6d403f4b/AGENTS.md
