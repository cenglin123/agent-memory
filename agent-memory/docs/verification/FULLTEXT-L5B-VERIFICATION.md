# FULLTEXT-L5B 独立验收报告（对抗式 Verifier）

- **verdict：通过**
- 验收对象：`543dbcf^..810e415` 共 6 个提交（隔离检出 `C:\wt\l5b-verify`，detached HEAD = 810e415）
- Verifier：全新上下文独立会话，与实现者不同模型族；全程只读仓库，未修改任何被检文件，未执行 git commit；未读取任何真实会话/知识库/生产 Archive（`~/.agent-memory*`、`~/.local/share/opencode`、`~/.codex`、`~/.claude/projects` 均未触碰）。全部动态验证使用仓库外合成 fixture 与 fake embedding 后端。
- 唯一写入：本文件。探针与中间产物均在 `C:\Users\chenr\AppData\Local\Temp\opencode\l5b-verify\`（仓库外临时目录）。

---

## 1. 结论摘要

6 个提交的行为与实现者完成记录（FULLTEXT-L5B-COMPLETED.md、SUITE-NORERANK-COMPLETED.md）一致，与两份规范性文档（agent-memory/README.md、assets/references/agent-memory-archive.md）一致。两条测试线全绿（617 passed / 2 ignored / 0 failed；pytest 59 passed），Verifier 独立构造的 102 项 CLI/MCP 端到端对抗探针全部通过。未发现阻断问题。

发现 3 项 **non_blocking**：CHANGELOG 两处陈旧/失实陈述（ctx-v1、`strict_snapshot_unsupported_for_live_index`），以及一条历史偶发失败测试的结构性根因判断（测试设计问题，非产品缺陷，本次未复现）。

## 2. 已执行命令与退出码

| # | 命令 | cwd | 退出码 | 结果 |
|---|---|---|---|---|
| 1 | `cargo test --locked --no-fail-fast` | `C:/wt/l5b-verify/agent-memory` | 0 | **617 passed / 2 ignored / 0 failed**（26 个测试二进制逐一核对；target 目录在检出内新建） |
| 2 | `python -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py` | `C:/wt/l5b-verify/assets/scripts` | 0 | 59 passed（与完成记录 §263 的 59 一致；babfc8c 删除 rerank 用例后从 60→59） |
| 3 | 偶发候选复现：`cargo test --locked --test recall_archive` ×20 循环 | 同上 | 0（20/20） | 每次 16 passed / 0 failed；连同第 1 条全量共 21 次执行该测试，均通过 |
| 4 | 独立探针驱动（自建 V2 fixture + CLI/MCP）：`gen_fixture.py` + `driver.py`（调用 `target/debug/agent-memory.exe` 的 `index`/`memory_search`/`memory_read`/`mcp`） | 仓库外 probe 目录 | 0 | **102 pass / 0 fail**（明细见 `probe/probe-report.json`） |
| 5 | PowerShell 语法解析：`Parser::ParseFile` 遍历 `assets/scripts/*.ps1`（22 个文件） | — | 0 | 全部 `parse_errors=0` |
| 6 | `git diff 543dbcf^..810e415 -- . \| grep "^+" \| grep "C:\\\\Users\\\\\|C:/Users/"` | 检出根 | 1（无匹配） | 6 提交新增行中无真实用户路径 |
| 7 | 悬空引用扫描 `grep -rn "kb_rerank_status\|CheckOptional"`（限 ps1/py/rs/json/md，排除 verification 文档） | 检出根 | 1（仅 CHANGELOG 历史条目命中） | 活代码与脚本无悬空引用 |

2 个 `#[ignore]` 均为显式规模/时延用例（`tests/recall_fulltext.rs:1641` 20 万行时延、`tests/recall_opencode.rs:758` 千会话规模），与预期"2 ignored"一致。

## 3. blocking

无。

## 4. non_blocking

### N-1 CHANGELOG 两处 `ctx-v1` 陈旧，且未记录 8000 字符截断（实现已是 ctx-v2）

- **位置**：`CHANGELOG.md:14`（向量缓存 bullet，两处 ctx-v1）、`CHANGELOG.md:16`（projector bullet，一处 ctx-v1）
- **证据**：实现为 `EMBED_INPUT_RULE = "ctx-v2"`（`src/recall/fulltext/vectors.rs:50`）与 `EMBED_INPUT_MAX_CHARS = 8000`（vectors.rs:55）；`git show dac91b9 --stat` 显示该提交只改了 README/archive.md/vectors/mod，未改 CHANGELOG。README.md:1063–1074 与 assets/references/agent-memory-archive.md:238 均已是 ctx-v2 + 8000 字符口径，无问题。
- **为何不阻断**：仅同一 stage 叙事条目内的历史状态残留，规范性文档与代码、以及运行时生成键（探针实测 vectors 头 `…;ctx=ctx-v2`）一致；不影响任何行为或合同。
- **建议**：把 CHANGELOG L5b 条目中三处 `ctx-v1` 更新为 `ctx-v2`，并在向量缓存 bullet 补一句"整条嵌入输入截断至 8000 字符"。

### N-2 CHANGELOG 声称存在 `strict_snapshot_unsupported_for_live_index`，该标记从未存在于任何代码

- **位置**：`CHANGELOG.md:15`（混合检索 bullet 末句："`--strict-snapshot` 明确标 `strict_snapshot_unsupported_for_live_index`"）
- **证据**：`git grep strict_snapshot_unsupported_for_live_index f2156a7 -- agent-memory` 无匹配、HEAD 全树仅 CHANGELOG.md 命中 1 次；实际行为相反——索引按配置身份应答，仅扫描回退读快照拷贝（`src/recall/search.rs:826-829` 注释；README.md:1174–1177；测试 `recall_hybrid::strict_snapshot_answers_from_the_index_like_exact_mode` 在套件中通过）。codex/claude_code/archive 在 strict 模式被拒的标记是另一组字符串（`strict_snapshot_unsupported_for_codex_home|claude_code_root|archive_root`），与 live 索引无关。
- **为何不阻断**：纯文档失实，README（规范口径）与实现、测试一致；不改变行为。
- **建议**：将该句改为"`--strict-snapshot` 只改变扫描回退读取的副本，不改变回答查询的缓存（按源身份定位）"。

### N-3 偶发失败测试（`recall_archive::retrieval_index_carries_generator_identity_and_rebuilds_identically`）根因判断：测试设计的环境竞态，非产品缺陷

- **位置**：`tests/recall_archive.rs:573`（测试本体）；`tests/support/mod.rs:14-19`（env_lock）、`43-83`（TestEnv 移除/重写全部环境变量，含 TMP/TEMP）、`148-157`（Drop 恢复环境并删除自己的 TempDir 根）
- **根因链**：该测试是文件中唯一不构造 `support::TestEnv` 的测试，因而不持有全局 env 锁，但其第一步 `fixture()`（recall_archive.rs:37-46）调用 `tempfile::tempdir()`，结果路径取决于进程级 `TMP/TEMP`。全量并行时若恰逢另一 TestEnv 测试的环境窗口，tempdir 会落在**对方测试的临时根内**；对方测试结束时 TestEnv Drop 递归删除该根，本测试的 fixture 随后被删，后续 `std::fs::…unwrap()` 以 ENOENT 失败。窗口为微秒级，故"全量并行出现过一次"且难复现。
- **复现尝试**：21 次执行（1 次全量 + 20 次 recall_archive 二进制循环）0 失败——与上述极小窗口一致；未能复现不推翻代码结构层面的因果链（`ReplayIdentity::current()` 不读环境变量、`replay_with_plan` 无 env 依赖，均已排除其他机制）。
- **为何不阻断**：Archive 派生检索骨架测试，与本 6 提交的行为无交集；全量与循环重跑均通过。
- **建议**：在该测试首行加 `let _env = support::TestEnv::new();`（先取 env 锁再建临时目录，与同文件其他测试一致），或在 `fixture()` 内用显式 Builder 根，杜绝继承并发改写的 TMP/TEMP。

## 5. 必查项逐条核验

| 必查项 | 结论 | 关键证据 |
|---|---|---|
| 1. cargo 全量 | ✅ 617 passed / 2 ignored / 0 failed，exit 0 | 命令 #1；无失败故无需 3 次重跑；偶发候选单独按 N-3 处置 |
| 2. pytest 三文件 | ✅ 59 passed，exit 0 | 命令 #2 |
| 3a. 缺省+过滤→hybrid、过滤在候选生成前 | ✅ | 探针：roles+since 只剩 assistant 命中、kinds 过滤两通道各验、session_id/session_kinds 过滤后全部命中满足过滤；语义支路 SQL 级过滤（`hybrid.rs:69-114` filter_sql，`semantic_branch` 同用）；既有测试 `hybrid_applies_filters_before_semantic_candidates` 通过 |
| 3b. 无索引源→仅该源 INDEX_REQUIRED、整请求不失败 | ✅ | 探针：oc2 `degraded` + `detail.reason=INDEX_REQUIRED` + `detail.hybrid{reason,hint}`，oc 正常，errors=[]，exit 4/10（非 2/5）；代码 `search.rs:892-928`、`effective_mode search.rs:948-987` |
| 3c. 缺省+order/group_by_session→exact 与显式一致 | ✅ | 探针：resolved_mode=exact、hit_id 逐一相等、exit 相等；既有测试对**整个信封做字节相等断言**（`recall_hybrid.rs:1092-1127`）并通过 |
| 3d. 显式 legacy+过滤→INVALID_REQUEST；显式 hybrid+order→拒绝 | ✅ | 探针 exit 2 + INVALID_REQUEST + 消息点名键；机器原因 `exact_only`/`hybrid_fixed_order` 在解析层 detail 中，由单元测试 `explicit_legacy_and_hybrid_keep_rejecting_unsupported_keys` 钉住（冻结 ErrorEntry 仅 {code,message,source_id}，信封不携带 detail，属既有冻结形状） |
| 3e. MCP 只读、查询期不写缓存 | ✅ | 探针对整个 cache 树逐文件 SHA-256 前后比对（search+read 后不变）；代码层 `open_for_query` SQLITE_OPEN_READ_ONLY（`fulltext/mod.rs:548`）、`VectorCache::load` 只读、`cache_store::write_atomic` 唯一写入方且仅 index 路径；boundary 套件通过 |
| 3f. CLI 与 MCP 同合同；工具 description 一致 | ✅ | 探针：同一请求 CLI stdout 与 MCP `structuredContent` 全等（search+read）；description 含 resolved_mode/INDEX_REQUIRED/coverage/stale_since/legacy/exact/group_by_session/memory_read，readOnlyHint=true（`mcp.rs:89-163`） |
| 4a. 身份任一变化→整源重建/重嵌入 | ✅ | `open_for_query` 校验 schema_version/source_id/source_root/source_kind/root_sha256/projector_version（`fulltext/mod.rs:572-590`）；向量头校验 generation_key(含 ctx)/projector/quantization/dimension（`vectors.rs:261-272`）；探针篡改 projector_version → 显式 hybrid `degraded`+cause=projector_version_changed，index 报 rebuild reason，默认模式按设计回退扫描（resolved_mode=legacy） |
| 4b. 同身份按嵌入输入哈希复用 | ✅ | 探针追加 1 行后重建：`reused>0, embedded==1, status=updated`；既有测试 `vectors_are_reused_incrementally_and_reembedded_on_model_change` 通过 |
| 4c. 删缓存重建字节一致（fake） | ✅ | 探针：vectors.bin 删除→重建 SHA-256 相等；整缓存目录删除→重建后 hybrid hit_id 列表相等；既有测试 `deleting_the_vector_cache_and_rebuilding_gives_identical_bytes`、`deleting_the_index_and_rebuilding_produces_identical_hits` 通过 |
| 4d. 模型不可用→全文仍建、向量 degraded、exit 0、无半成品 | ✅ | 既有测试 `an_unavailable_model_degrades_only_the_vector_report` 逐项断言（built、vectors degraded+unavailable、**vectors.bin 不存在**、CLI exit 0、ok=true）且套件通过；代码 `build.rs:174-199` 向量步骤只进 detail |
| 4e. 8000 截断→≤65536 字节；截断不改全文/snippet/回读 | ✅ | `truncate_chars`（vectors.rs:87-92）按字符截断，8000×4B=32000<65536；单元测试断言 30000 字输入 `chars==8000 && len≤65536`（vectors.rs:682-691）；截断只作用于 `embedding_input`，Unit.text/range_sha256 不经该路径（build.rs 与 vectors.rs 分离）；探针：30000 字 CJK 单元可被 exact 命中、hybrid 语义支路 available、vectors 头身份完好 |
| 5. CJK 切分与 FTS5 转义 | ✅ | 判定范围/重叠三字片段/<3 字不入 MATCH/引号字面/去重/48 上限：`hybrid.rs:128-212` + 单元测试（hybrid.rs:654-720，套件通过）；**自建对抗查询实测**：含 `"`、`*`、`NEAR`/`AND`/`OR`/`NOT` 字面、反斜杠、CJK+引号、不配对引号共 13 条 ×{exact,hybrid,legacy} 39 次无一 SQL 错误/INDEX_CORRUPT；`fts_phrase` 双引号加倍（mod.rs:707-711）且 MATCH 是唯一内插字符串，其余全部参数绑定；无词项查询（如 `*`）按合同以 `no_lexical_terms` INVALID_REQUEST 拒绝 |
| 6. coverage | ✅ | `stale_since` 仅在源领先时出现（coverage.rs:96-203；探针：当前→无、追加行→有、重建→消失）；coverage 失败只省略（探针移除源库：块仍在、无 stale_since、status/结果不变）；字段与 README:1143-1146、archive.md:241 一致 |
| 7. 543dbcf | ✅ | V1 字节不变：`tool_part_name` 先读 `tool`、`tool_part_output` 先读 `state.output`，投影拼装未动（543dbcf diff）+ 单元测试 `v1_tool_projection_is_unchanged` 钉住 `"bash\n{\"command\":\"ls\"}\na.txt"`；V2 `name`+`state.content`（字符串/文本块数组）：探针 exact 命中 tool_output 通道、legacy 扫描命中、memory_read 回读含块文本、tool_input 通道命中 |
| 8. babfc8c | ✅ | 22 个 ps1 语法解析 0 错误；`kb_rerank_status`/`CheckOptional` 在活脚本/代码中零残留（仅 CHANGELOG 历史条目）；`15-install-kb-deps.ps1`/`recall_onboarding.py`/`90-audit.ps1` 无 torch/transformers/ASK_AUTO_RERANK；INSTALL.md、conventions.md 无精排条目；Rust 旧信封兼容解析原样（`providers/kb.rs:244-276`，babfc8c 未触碰 agent-memory/src，commit stat 佐证） |
| 9. 文档与路径隐私 | ✅（含 2 项 non_blocking） | README 模式表（:883）、resolved_mode（:888）、projector（:986-988）、ctx-v2/8000（:1063-1074）、CJK（:1109-1127）、coverage（:1143-1146）、INDEX_REQUIRED（:1160）与实现一致；archive.md:235-241 同；CHANGELOG 模式判定/resolved_mode/INDEX_REQUIRED/CJK/coverage 条目一致，唯 ctx-v1（N-1）与 strict_snapshot 标记（N-2）两处失实；6 提交新增行 0 处 `C:\Users\`/`C:/Users/` |

## 6. 证据文件

- 探针 fixture/驱动/报告：`C:\Users\chenr\AppData\Local\Temp\opencode\l5b-verify\probe\`（`gen_fixture.py`、`driver.py`、`probe-report.json`）
- cargo 全量日志：`C:\Users\chenr\AppData\Local\Temp\opencode\l5b-verify\cargo-test.log`
- 偶发复现循环日志：`C:\Users\chenr\AppData\Local\Temp\opencode\l5b-verify\flaky-loop.log`
- 被检检出保持干净：`git -C C:/wt/l5b-verify status --short` 为空（target/ 被 ignore，属允许的构建产物）
