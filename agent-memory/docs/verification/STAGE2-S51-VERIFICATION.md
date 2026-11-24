# STAGE2-S51 独立验收报告

verdict: 通过

- 套件 worktree：`C:\wt\s51-verify-suite`，detached HEAD = `1026b37dee4f47847b6b80513b9d97cf0dc9a395`（父提交 `5c31aa3e11dd04b79cc7075e1915f69e74a9c6a5`）。
- 知识库 worktree：`C:\wt\s51-verify-kb`，detached HEAD = `e1b54da87fac746f4ea586b24f5a9329a7fcaf2a`（父提交 `880b298c3661aa18c7dfa6e9a8a381c3d21ea964`）。
- 独立性：本报告由全新上下文的 Verifier 完成，未参与实现，与实现者不同模型族；只读两个隔离 worktree、专用临时目录与工具链。未读取 `STAGE2-S51-COMPLETED.md`（实现者自述）、实现者 prompt、其他评审报告或 canonical 工作树；未读取任何禁区路径。所有 fixture 均为合成数据。

---

## 1. 结论摘要

合同 7 条全部满足；未发现阻断项（B 项为空）。Rust 侧仅在显式 `mode:"exact"` 的 KB 源追加 `--bm25` 并做全词项过滤，`resolved_mode=kb_lexical` / `detail.exact={mode:"kb_lexical",reason:"not_indexed"}` / `native_bm25` 均按合同落地；auto/hybrid/legacy、其他源 exact、Archive、session_id corner 与 `memory_read` 回读未见回归。知识库侧 `ask.py --json --bm25` 成功信封 `detail.mode="bm25"`、单信封、退出码、`--json` 隐含 no-log、非法组合 fail-closed 均通过独立合成测试。

非阻断项共 6 条（N1–N6），主要为“子进程自报 mode 与 resolved_mode 不做交叉校验”“失败时仍标注 lexical 路径”等加固建议，以及文档/本地路径类小事，均不构成合同阻断。

---

## 2. 被检变更范围

### 2.1 套件 `git diff HEAD^ HEAD`

| 路径 | 增/删 |
|---|---|
| `CHANGELOG.md` | +9 / -0 |
| `agent-memory/README.md` | +22 / -5 |
| `agent-memory/docs/verification/STAGE2-S51-COMPLETED.md` | +111 / -0（按指令**未读**，不纳入评估） |
| `agent-memory/src/recall/contract.rs` | +15 / -1 |
| `agent-memory/src/recall/providers/kb.rs` | +114 / -3 |
| `agent-memory/src/recall/search.rs` | +51 / -11 |
| `agent-memory/tests/fixtures/recall/kb_stub/ask.py` | +87 / -0 |
| `agent-memory/tests/recall_kb_device.rs` | +208 / -0 |
| `assets/references/agent-memory-archive.md` | +3 / -3 |

### 2.2 知识库 `git diff HEAD^ HEAD`

| 路径 | 增/删 |
|---|---|
| `.meta/scripts/ask.py` | +13 / -7 |
| `.meta/tests/test_ask_readonly_json.py` | +78 / -0 |
| `docs/CHANGELOG.md` | +4 / -0 |

测试文件均为纯新增（0 删除），没有删弱旧断言；现有 `build_args` 单测仅因函数签名新增 `exact: bool` 参数而补传 `false`，断言未改。

---

## 3. 逐条合同判定

| # | 合同条 | 判定 | 证据 |
|---|---|---|---|
| 1 | 仅 KB 源显式 `mode:"exact"`，仍由 `ask.py` 检索，在 `--json --no-log --top-k <n> -- <query>` 上追加 `--bm25` | 通过 | `kb.rs:564-588`（`build_args` 仅在 `exact` 时 push `--bm25`，随后 push `--` 与 query）；`search.rs:1320-1341`（只有 `SourceKind::Kb` 且 `request.mode == Exact` 才传 `exact_terms`）；合成 E2E argv 断言前缀 `--json --no-log --top-k N --bm25 --`，且 `--` 仍紧邻 query |
| 2 | 复用 `tokenize(query)` 与 ASCII-only `fold_ascii`，只留 `chunk_text` 含全部词项的候选；多词 AND / ASCII 大小写 / CJK 整段+bigram / 缺词剔除 / 空结果健康 empty | 通过 | `search.rs:598` 取 `tokenize`；`kb.rs:596-603` 对 `chunk_text` 调 `fold_ascii` 后逐词 `contains`（全词 AND）；`kb.rs:383-390` 在 `build_hit` 前过滤；E2E 39/39 覆盖 `alphabeta gamma`、`ALPHABETA GAMMA`、`foo bar baz`、`统一回忆`、`foo bar zzz`（空）、`-foo`；空结果 status=`empty`、exit 0 |
| 3 | `resolved_mode=kb_lexical`；`detail.exact={mode:"kb_lexical",reason:"not_indexed"}`；BM25 envelope→`native_bm25`；`kb_lexical` 不是请求 mode | 通过 | `search.rs:880-885, 1128-1137`；`contract.rs:323,326,334,387`；E2E 输出 `resolved_mode=kb_lexical`、`score_kind=rank_basis=native_bm25`；基线/空结果均保持该标注；CLI 传 `mode:"kb_lexical"` 返回 exit 2 `INVALID_REQUEST: mode must be ...` |
| 4 | KB 默认/auto、显式 hybrid/legacy 不变；其他源 exact 不变；Archive exact 仍 direct/exact；带 session_id 的 KB corner 不虚标检索路径 | 通过 | `search.rs:865-895` 仅对 `kind==Kb && session_id.is_none()` 标 `kb_lexical`，其余走原 `annotate_exact_direct`；`search.rs:1150-1183` Archive 在 `scan_source` 提前返回、根本不进 KB 分支；E2E：auto/hybrid/legacy 均无 `--bm25`、9 条候选全保留、resolved 分别为 legacy/hybrid/legacy；exact+session_id 无子进程调用、`reason=session_filter_not_applicable`、`exact.mode=direct`、`resolved_mode=exact`；Device exact 探针仍 `exact` 路径、无 `kb_lexical` |
| 5 | `memory_read` 对保留 ref 仍按原唯一 chunk/字节区间回读；过滤不绕过原 fail-closed | 通过 | E2E 对过滤后保留的首条 ref 调 `memory_read`，回读 `ok=true`、`text="AlphaBETA gamma body"`；`kb.rs:596-603` 对无 `chunk_text` 行返回 true，交由 `build_hit` 走 `CONTENT_UNRESOLVED`（未静默丢弃）；`cargo test` 的 `recall_read` 18 条全过 |
| 6 | `ask.py --json --bm25` 成功信封 `detail.mode=bm25`、纯 BM25 排序不改；单信封/退出码、`--json` 隐含 no-log、默认 dense、显式 hybrid 不回归；非法组合 fail-closed | 通过 | `ask.py:1246-1257`（拒收清单去掉 `--bm25`，mode 设为 bm25）；`ask.py:1347-1354` 主守卫同步；`ask.py:1408-1413` JSON 下 hybrid+bm25 单信封 exit 2；独立 ask harness 28/28：成功、空、索引异常退化、7 组非法组合、dense/hybrid 回归、`--check`、排序、no-log、stderr；KB 自带 32 条全过 |
| 7 | 已知候选召回限制（BM25 候选内过滤）可接受且需诚实标注 | 通过 | `README.md:1102-1107` 明确“filter runs on what the child returned … does not guarantee every literal match”；`assets/references/agent-memory-archive.md` 同步该限制；实现确实只在候选内过滤，未把该限制包装成穷尽检索，不构成阻断 |

---

## 4. 独立合成验证

### 4.1 Rust 端到端（`C:\wt\s51-verify-temp\s51-e2e\verify_e2e.py`）

自建 KB 根 + 自建 `ask.py` stub（记录 argv、按 `--bm25` 返回 `detail.mode=bm25`），用编译出的 `agent-memory.exe memory_search/memory_read` 驱动；结果 **39/39 通过，exit 0**。覆盖：

- exact 混合候选只留全词项：`alpha.md` + `case.md`，`beta.md`（缺 gamma）被剔除；`score_kind/rank_basis=native_bm25`。
- argv 精确形状：`["--json","--no-log","--top-k","N","--bm25","--","<query>"]`，`--` 紧邻 query，`-foo` 原样到达。
- `resolved_mode=kb_lexical`、`detail.exact={"mode":"kb_lexical","reason":"not_indexed"}`、status ok、无 warnings。
- 全词缺失 → 健康 `empty`（status empty、无 code、exit 0、pagination 不 incomplete），仍保留 kb_lexical 标注。
- CJK `统一回忆` 仅保留含完整连续段的行；`foo bar baz` 仅保留三词齐全行；大写查询经两侧 ASCII 折叠后正确过滤。
- auto / hybrid / legacy：argv 无 `--bm25`、`--` 仍紧邻 query、9 条候选**不**过滤、无 `exact` 块、resolved 分别为 legacy/hybrid/legacy。
- exact + `session_id`：子进程**零调用**（argv log 无新行）、`session_filter_not_applicable`、保持 `direct`/`resolved_mode=exact`，未虚标 kb_lexical。
- 保留 ref 的 `memory_read` 回读字节区间正确。
- `!!!` / `a` 等无词项查询在 dispatch 前被 `INVALID_REQUEST` 拒绝（exit 2），不会产生“空词项→过滤恒真”的漏洞。

### 4.2 知识库 `ask.py` 独立 harness（`verify_ask_json.py`）

只读 import worktree 的 `ask.py`，用临时 vault + 临时 SQLite manifest + mock 检索函数驱动；**28/28 通过，exit 0**。覆盖：

- `--json --bm25`：stdout 恰一个文档、`ok=true`、`detail.mode=bm25`、`escalated=[]`、行含 `path/score/chunk_index/chunk_text`、无文本模式回显、成功时 stderr 为空。
- 空结果：exit 0、healthy empty、mode bm25。
- `bm25_search` 抛异常：仅 stderr 告警，stdout 仍单信封 empty，exit 0。
- 7 组非法组合（hybrid/deep/save/orphans/backlinks/neighbors/path）：均 exit 2、单信封 `INVALID_REQUEST`，无文本模式输出。
- 默认 dense、显式 hybrid：单信封、mode 分别为 dense/hybrid，`escalated` 保留。
- `--check --bm25`：单信封、mode bm25、exit 0。
- 纯 BM25 排序：给定 raw `[(alpha,4.0),(beta,2.0),(gamma,6.0)]`，JSON 顺序为 gamma→alpha→beta，min-max 归一化分数与既有实现一致（排序未改）。
- no-log：`log_script_run` 未调用，usage/escalation 文件均未创建。

### 4.3 补充边界探针

- `mode:"kb_lexical"` 请求：exit 2，`INVALID_REQUEST`（`mode must be "auto", "exact", "hybrid" or "legacy"`），证明它不是请求 mode。
- Device 源 exact/hybrid/legacy：无 kb_lexical；exact 仍走自身 fallback 语义（INDEX_MISSING，exit 10），说明非 KB 源未受影响。
- 子进程在 `--bm25` 下返回 `detail.mode="dense"`（不合规 stub）：结果为 `score_kind=native_dense` 但 `resolved_mode=kb_lexical`，见 N1。
- 子进程 exit 12：status unavailable、code DB_UNAVAILABLE、exit 4，但仍带 `exact.mode=kb_lexical`，见 N2。

---

## 5. 实际命令、退出码与测试计数

| 命令 | 退出码 | 结果 |
|---|---|---|
| `git rev-parse HEAD` / `git log --oneline -3`（两 worktree） | 0 | SHA 与上文一致；父提交符合预期 |
| `git status --short`（测试前，两 worktree） | 0 | 空（clean） |
| `git diff --name-status HEAD^ HEAD`、`--stat`、`--numstat`（两 worktree） | 0 | 路径与增删见 §2 |
| `git diff --check HEAD^ HEAD`（两 worktree） | 0 | 无 whitespace/冲突标记问题 |
| `cargo test --locked --no-fail-fast`（误在套件根执行） | 101 | `could not find Cargo.toml`；随后在 `agent-memory/` 正确重跑 |
| `cargo test --locked --no-fail-fast`（`agent-memory/`） | 0 | **719 passed / 0 failed / 3 ignored**，32 个 test-result 组（含 doc-tests） |
| `python -I .meta/tests/test_ask_readonly_json.py` | 0 | **32 tests OK**（父提交为 26，新增 6） |
| `python -I .meta/tests/test_script_assets.py` | 0 | **5 tests OK**（仅 `governance_check.py:22` 的既有 SyntaxWarning） |
| `python -I .meta/tests/test_bm25_atomic_write.py` | 1 | 2 tests：**1 failure + 1 error**（存量，归因见 §7.2） |
| `python -I verify_e2e.py`（套件二进制 + 合成 KB） | 0 | **39/39 checks passed** |
| `python -I verify_ask_json.py`（ask.py 临时 vault） | 0 | **28/28 checks passed** |
| CLI 探针：`mode:"kb_lexical"` | 2 | INVALID_REQUEST |
| CLI 探针：Device exact | 10 | 自身 `INDEX_MISSING` 扫描回退，无 kb_lexical |
| CLI 探针：不合规 child（dense on --bm25） | 0 | 见 N1 |
| CLI 探针：child exit 12 | 4 | 见 N2 |
| `git status --short`（测试后，两 worktree） | 0 | 空（tracked 无变化） |
| `git status --short --ignored`（测试后） | 0 | 仅 `agent-memory/target/`、`.meta/scripts/__pycache__/`（忽略项，非 tracked） |

Cargo 关键子集：`recall_kb_device` 32 passed（含新增 7 条）、`recall_read` 18 passed、`recall_contract` 14 passed、doc-tests 5 passed；3 个 ignored 均为既有 scale/latency 用例（新增测试均未被 ignored）。新增测试数 7（集成）+ 2（`kb.rs` 单测）= 9，与 719−710 一致。

---

## 6. 阻断项

**无。** 未发现 B1…；合同 7 条均通过，未发现需要修复才能交付的缺陷。

---

## 7. 非阻断项与归因

### 7.1 非阻断项

**N1（低）— exact 请求未校验子进程自报 mode 必须为 bm25。**
- 位置：`agent-memory/src/recall/providers/kb.rs:314-338`（仅按 envelope `detail.mode` 映射 score_kind）+ `search.rs:880-885`（按请求路径标 kb_lexical）。
- 最小复现：合成 stub 在收到 `--bm25` 时返回 `detail.mode="dense"`；exact 查询 `alphabeta gamma` → `results[0].score_kind="native_dense"`，但 `resolved_mode="kb_lexical"`、`exact.mode="kb_lexical"`。
- 影响：只有不合规/旧版子进程会触发；真实 `ask.py --json --bm25` 恒返回 bm25，故不阻断。
- 修复方向：`exact_terms.is_some()` 时若 envelope mode≠`bm25`，按 KB 契约违规（`KB_QUERY_FAILED`）处理，或不标 kb_lexical。

**N2（低）— 子进程失败时仍标注 kb_lexical。**
- 位置：`search.rs:880-885`；`kb.rs` 失败返回后仍进入同一标注分支。
- 最小复现：stub 输出 `ok:false` + exit 12 → status `unavailable`/`DB_UNAVAILABLE`，但 `detail.exact.mode="kb_lexical"`、`resolved_mode="kb_lexical"`。
- 影响：status/code/exit 正确，标注描述“尝试的路径”而非“成功回答”；与改造前 `direct` 标注在失败时同样保留的语义一致，故不阻断。
- 修复方向：仅对 ok/empty 结果标 kb_lexical，或增加 `attempted_mode` 区分。

**N3（低）— MCP 工具描述未同步 KB 词法路径。**
- 位置：`agent-memory/src/mcp.rs:151` 仍称 exact 使用 “the local trigram index with AND-ed terms”，未提 KB 的 `--bm25` + 全词过滤；README 与 archive 参考已更新。
- 影响：仅工具说明文本，响应字段 `resolved_mode` 仍可辨识。
- 修复方向：更新该 description 一句。

**N4（低）— CHANGELOG 新增了实现者本机绝对路径。**
- 位置：`CHANGELOG.md:9`（`C:\wt\20261031-230347-6d403f4b`）、`CHANGELOG.md:12`（`C:\wt\s51-glm-temp`）；另有测试 fixture 的 `C:/root/.meta/scripts/ask.py`。
- 影响：不含用户 home（`C:\Users\...`）、不含凭据或真实正文；仅暴露本机 worktree/temp 目录名。
- 修复方向：如需严格路径隐私，改用 `<worktree>` / `<temp>` 占位。

**N5（信息）— 单条 BM25 命中分数为 0.0。**
- 位置：`ask.py:1183-1191` 既有 min-max 归一化（`(v-min)/(max-min or 1)`），单条时 min=max → 0.0。
- 影响：S5.1 未改该分支；排序与相对分语义不变，仅提示调用方不要用绝对分数做阈值。

**N6（信息）— `--json --hybrid --bm25` 错误信封 `detail.mode="dense"`。**
- 位置：`ask.py:1408-1413` 在 `_run_json_query` 设置 mode 之前报错；`_json_error` 使用默认 `_JSON_STATE['mode']`。
- 影响：错误信封仍单文档、exit 2、`INVALID_REQUEST` 正确；mode 字段不参与错误判定，属既有错误信封形状。

### 7.2 `test_bm25_atomic_write.py` 存量失败归因（非 S5.1 回归）

- 命令：`python -I .meta/tests/test_bm25_atomic_write.py` → exit 1；`test_reader_sees_old_index_until_new_index_is_complete` FAIL、`test_failed_write_preserves_old_index` ERROR。
- 两个失败均因 `bm25_index._write_index_atomically` 不存在（实现只有 `_write_gzip_atomically`）。
- 归因证据：该测试与 `bm25_index.py` 都不在 `git diff --name-only HEAD^ HEAD` 中；blob 完全相同：
  - `HEAD:.meta/tests/test_bm25_atomic_write.py` = `HEAD^:...` = `06177e30f5e8e22c62c076d6f846f2f8d21d01c5`
  - `HEAD:.meta/scripts/bm25_index.py` = `HEAD^:...` = `f1f5c5f0a86b5f356018e845a574999f68cad4e1`
  - 运行时 `hasattr(bm25_index, "_write_index_atomically") == False`、`_write_gzip_atomically == True`。
- 结论：父提交同样失败，属基线存量测试与实现脱节，与 S5.1 无关；未修复、未扩大范围。

---

## 8. 工作树、隐私与限制

- **测试前后 tracked 状态**：两 worktree 测试前 `git status --short` 均为空；测试后仍为空。仅存在忽略项 `agent-memory/target/`（cargo 构建产物）与 `.meta/scripts/__pycache__/`（预存，mtime 2026-10-31 23:48，早于本次运行；gitignored）。未修改任何 tracked 文件，未 commit/merge/改分支。
- **diff-check**：两 worktree `git diff --check HEAD^ HEAD` 均 exit 0。
- **路径隐私**：新增行扫描未发现 `C:\Users`、`C:\OneDrive`、真实 home 路径、用户名、token/password/api-key 等凭据；未含真实会话或笔记正文。仅见 N4 所列本机 `C:\wt\...` 路径与合成 fixture 的 `C:/root/...`。按指令未读的 `STAGE2-S51-COMPLETED.md`（+111 行）不在本报告的隐私扫描范围内，也不作为任何结论依据。
- **已知限制**：这是 BM25 候选召回后的全词过滤，不保证覆盖全库所有字面匹配；实现与文档均如实声明（`README.md:1102-1107`），按验收准绳不作为阻断。
- **未读数据声明**：未读取 `C:\OneDrive\Cr\Obsidian_Vault`、`C:\Users\chenr\.session-evidence`、`C:\Users\chenr\.agent-memory`、`C:\Users\chenr\.claude`、`C:\Users\chenr\.codex`、`C:\Users\chenr\.local\share\opencode` 及任何真实会话/笔记/生产 Archive/配置/凭据；未读 `STAGE2-S51-COMPLETED.md`。
- **未修改声明**：未修改两个被检 worktree 的源码或 tracked 文件，未执行 commit、merge、分支切换或源码修复；所有 fixture、脚本、日志与临时输出位于 `C:\wt\s51-verify-temp`，唯一持久输出为本报告。

---

## 9. 执行证据（reads）

```yaml
reads:
  # 套件 worktree（源码/测试/文档）
  - C:/wt/s51-verify-suite/CHANGELOG.md
  - C:/wt/s51-verify-suite/CURRENT.md
  - C:/wt/s51-verify-suite/assets/references/agent-memory-archive.md
  - C:/wt/s51-verify-suite/agent-memory/README.md
  - C:/wt/s51-verify-suite/agent-memory/src/recall/contract.rs
  - C:/wt/s51-verify-suite/agent-memory/src/recall/providers/kb.rs
  - C:/wt/s51-verify-suite/agent-memory/src/recall/providers/archive.rs
  - C:/wt/s51-verify-suite/agent-memory/src/recall/search.rs
  - C:/wt/s51-verify-suite/agent-memory/src/recall/text.rs
  - C:/wt/s51-verify-suite/agent-memory/src/mcp.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_kb_device.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/fixtures/recall/kb_stub/ask.py
  - C:/wt/s51-verify-suite/agent-memory/tests/support/mod.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_cli_mcp.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_archive.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_archive_fallback.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_boundary.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_contract.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_hybrid.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_read.rs
  - C:/wt/s51-verify-suite/agent-memory/tests/recall_fulltext.rs
  - C:/wt/s51-verify-suite/agent-memory/docs/verification/P2P3-RECALL-COMPLETED.md
  # 知识库 worktree
  - C:/wt/s51-verify-kb/.meta/scripts/ask.py
  - C:/wt/s51-verify-kb/.meta/scripts/bm25_index.py
  - C:/wt/s51-verify-kb/.meta/tests/test_ask_readonly_json.py
  - C:/wt/s51-verify-kb/.meta/tests/test_bm25_atomic_write.py
  - C:/wt/s51-verify-kb/docs/CHANGELOG.md
  # 专用临时目录（自建 fixture / 日志）
  - C:/wt/s51-verify-temp/logs/cargo-test-full.log
  - C:/wt/s51-verify-temp/s51-e2e/stub_ask.py
  - C:/wt/s51-verify-temp/s51-e2e/verify_e2e.py
  - C:/wt/s51-verify-temp/s51-e2e/verify_ask_json.py
  - C:/wt/s51-verify-temp/s51-e2e/work/argv.log
  - C:/wt/s51-verify-temp/s51-e2e/work/config.json
  - C:/wt/s51-verify-temp/s51-e2e/work/device.json
  - C:/wt/s51-verify-temp/s51-e2e/work/dense_mode.json
  - C:/wt/s51-verify-temp/s51-e2e/work/fail.json
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/alpha.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/beta.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/case.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/cjk_full.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/cjk_part.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/dash.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/foo.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/gamma.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/notes/multi.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb/.meta/scripts/ask.py
  - C:/wt/s51-verify-temp/s51-e2e/work/kb_densemode/.meta/scripts/ask.py
  - C:/wt/s51-verify-temp/s51-e2e/work/kb_densemode/notes/alpha.md
  - C:/wt/s51-verify-temp/s51-e2e/work/kb_fail/.meta/scripts/ask.py
  - C:/wt/s51-verify-temp/s51-e2e/work/kb_fail/notes/a.md
  - C:/wt/s51-verify-temp/s51-e2e/work/device/2026-10-01_probe.md
```