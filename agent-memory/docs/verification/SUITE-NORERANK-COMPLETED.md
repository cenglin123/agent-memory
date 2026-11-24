# 套件移除知识库精排依赖探测与默认开关 — 完工记录

工作区：`C:\wt\hybrid-retrieval-20261028`（HEAD `cb7cb6b`，未提交；提交由 orchestrator 执行）
依据：知识库 `a3188790`「移除检索精排，候选相关性由调用方 agent 判断」。`ask.py` 不再精排，`--rerank` 仅兼容保留，`ASK_AUTO_RERANK` 不再被读取；套件侧的探测、报告与默认开关因此失去意义。

范围：只改本工作区；不读真实会话/知识库/生产 Archive，不重建 `assets/dist`，不运行 61/62/90 的真实安装，不改 Rust 源码，不改 `AGENTS.md`。

## 1. 修改文件

| 文件 | 改动 |
|---|---|
| `assets/scripts/kb_rerank_status.py` | 删除（整文件，66 行） |
| `assets/scripts/00-precheck.ps1` | 必备文件清单删去 `assets\scripts\kb_rerank_status.py` |
| `assets/scripts/90-audit.ps1` | 删除「知识库本地精排依赖（可选）」`CheckOptional` 检查块及其前置注释；`CheckOptional` 已无调用方，连同其定义与注释删除；头部「可选能力缺失记 WARN」说明删除；汇总段删除 `$warnCount` 及其输出（不再可能产生 WARN） |
| `assets/scripts/15-install-kb-deps.ps1` | 删除文件头「可选：知识库本地精排依赖…」两行说明；删除精排探测块（`kb_rerank_status.py` 调用、`[INFO]／[WARN]` 输出与 `pip` 手工安装提示） |
| `assets/scripts/recall_onboarding.py` | 删除写入 `interpreter.kb.env.ASK_AUTO_RERANK` 的 `env.setdefault(...)` 及说明注释；离线 env 三键（`PYTHONDONTWRITEBYTECODE`/`PYTHONUTF8`/`HF_HUB_OFFLINE`）不变，用户既有键不主动删除 |
| `assets/scripts/test_recall_onboarding.py` | 删除 `ASK_AUTO_RERANK == '0'` 断言与 21a 用例 `test_rerank_default_is_written_and_an_explicit_value_is_kept` |
| `INSTALL.md` | 流程表 1.5 行删去「同时报告可选精排依赖（torch / transformers）状态」；删除「**1.5 的可选组件：知识库本地精排依赖**」整节（含 `pip` 示例代码块） |
| `assets/references/conventions.md` | 「套件脚本不复制进 dist」清单删去 `kb_rerank_status.py`；删除「可选能力只报告不安装」整条（指向 `INSTALL.md` 1.5 的交叉引用随条目消失） |
| `assets/references/agent-memory-archive.md` | 「知识库不精排…`ASK_AUTO_RERANK=0`」条目改为原则句「知识库检索不做精排：返回带摘要的候选，由调用方 agent 判断相关性。」；审计表删除「知识库本地精排依赖（**WARN**）」行 |
| `agent-memory/README.md` | KB 信封段：`detail.mode` 生产者改为 `dense|hybrid`，`rerank` 与 `detail.rerank.skipped` 改为「兼容旧版知识库信封」口径；删除 `ASK_AUTO_RERANK=0` 段落，改为「onboarding 只写离线 env，KB 检索不做精排」的原则句 |
| `CHANGELOG.md` | 顶部（`## 2026-10-30` 下）新增「知识库检索不做精排：移除套件中的精排依赖探测与默认开关」条目，列出上述改动与依据 |
| `agent-memory/docs/verification/SUITE-NORERANK-COMPLETED.md` | 本 sentinel（新增） |

未改动：Rust 源码（`src/recall/contract.rs`、`src/recall/providers/kb.rs` 的旧信封兼容解析保留）、`agent-memory/tests/fixtures/recall/kb_stub/ask.py`、`agent-memory/tests/recall_kb_device.rs`、`AGENTS.md`、`assets/dist`。

## 2. 每处改动的判定依据

1. **删除而非保留开关**：`ask.py` 已不读取 `ASK_AUTO_RERANK`，套件继续写入只会留下无效配置键；但已存在的用户键不做删除或改写，保持「不改用户显式值」。
2. **`CheckOptional` 一并删除**：`90-audit.ps1` 中该函数唯一调用方就是被删的精排检查（`grep -n CheckOptional` 仅见定义与被删块），删除后 `WARN` 级不再可达，`$warnCount` 汇总同步移除，避免留下永不触发的分支。
3. **15 不再探测**：探测脚本已删，保留探测块只会每次安装都报「探测脚本缺失」的伪 WARN。
4. **文档改为最终态**：删除「可选精排依赖」的安装指引与审计条目，原则句只陈述「不做精排、由调用方判断」，不保留日期、弃用对比或历史叙述；历史事实由 CHANGELOG 与既有 `agent-memory/docs/verification/` 记录承担。
5. **Rust 兼容解析保留**：README 描述改为「兼容旧版知识库信封」——当前 `ask.py` 不产出 `mode=rerank`／`detail.rerank.skipped`，但旧版知识库信封仍能被解析为 `score_kind`/`rank_basis` 与 `warnings[]`，因此解析代码与 fixture/用例不动。

## 3. 剩余命中与理由

命令：

```
grep -rn -i "kb_rerank|ASK_AUTO_RERANK|精排|rerank" --exclude-dir=target --exclude-dir=.git --exclude-dir=dist .
```

| 位置 | 内容性质 | 保留理由 |
|---|---|---|
| `CHANGELOG.md:5-10` | 本次新增条目 | 本次改动的正式记录 |
| `CHANGELOG.md:17,20` | L5b 历史条目（`ASK_AUTO_RERANK=0` 与 rerank 用例） | 历史事实，CHANGELOG 不改写既往记录 |
| `CHANGELOG.md:37,38` | L1 历史条目（90/15 曾新增探测） | 同上 |
| `agent-memory/docs/verification/FULLTEXT-L1-COMPLETED.md:31,34,37,39,52,60,74,75,104` | 当时验收记录 | 历史验收证据，非现行行为说明 |
| `agent-memory/docs/verification/FULLTEXT-L5B-COMPLETED.md:164,174,181` | 当时验收记录 | 同上 |
| `agent-memory/docs/verification/P2P3-RECALL-COMPLETED.md:23,37,50,51,59,61,90`、`P2P3-FIX-COMPLETED.md:124` | 当时验收记录（KB 适配器与 ask.py 历史行为） | 同上 |
| `agent-memory/README.md:772-774` | 「`rerank` 仅为兼容旧版信封而接受」 | 现行实现的准确描述，非精排探测/开关 |
| `agent-memory/README.md:795` | 原则句「KB recall is not reranked…」 | 任务要求的原则表述 |
| `assets/references/agent-memory-archive.md:243` | 原则句「知识库检索不做精排…」 | 任务要求的原则表述 |
| `agent-memory/src/recall/contract.rs:122,123,129,130,302,305,312,361` | 旧信封兼容：`KB_MODE_RERANK`、`WARN_RERANK_*`、`SCORE_KIND_NATIVE_RERANK` | 任务明确「Rust 源码不改」 |
| `agent-memory/src/recall/providers/kb.rs:16,17,322,335,340,341,349,350` | 旧信封兼容解析（mode 映射、`detail.rerank.skipped` → warning） | 同上 |
| `agent-memory/tests/fixtures/recall/kb_stub/ask.py:16,17,114,117` | 旧信封 fixture 的 `rerank-timeout` 分支 | 任务明确保留 |
| `agent-memory/tests/recall_kb_device.rs:219,220` | 旧信封告警用例 | 任务明确保留 |
| `assets/scripts/.pytest_cache/v/cache/nodeids:57` | pytest 生成的用例缓存（含已删除用例名） | 生成物、`.pytest_cache/.gitignore` 自忽略；任务声明该缓存目录不用管 |

除此之外无 `kb_rerank_status.py`／`ASK_AUTO_RERANK` 的活引用。

## 4. 验证命令与结果

| 命令 | 结果 |
|---|---|
| `python -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`（cwd=`assets/scripts`） | **59 passed**（1.84s；删除 21a 后由 60 降为 59） |
| `[System.Management.Automation.Language.Parser]::ParseFile(...)` 对 `00-precheck.ps1`、`15-install-kb-deps.ps1`、`90-audit.ps1` | 三者均 `errors=0` |
| `powershell -NoProfile -ExecutionPolicy Bypass -File assets/scripts/check-docs.ps1` | `[PASS] 文档治理检查通过：6 项、67 个 Markdown（完整）`，exit 0 |
| `powershell -NoProfile -ExecutionPolicy Bypass -File assets/scripts/check-docs.ps1 -Quick -Quiet`（`.githooks/pre-commit` 同参数） | `[PASS] …（快速）`，exit 0 |
| `powershell -NoProfile -ExecutionPolicy Bypass -File assets/scripts/95-test-docs-governance.ps1` | 基础 28 + 新增 39 = **67/67 通过**，临时目录已清理 |
| `git diff --check` | 无输出，exit 0 |
| 首字节检查（三个 `.ps1`） | 均为 `efbbbf`，BOM 保留 |
| `git ls-files --debug`（行尾核对） | `CHANGELOG.md`、`agent-memory/README.md`、`agent-memory-archive.md` 索引尺寸=blob 尺寸，改动前后同为 LF；`INSTALL.md`、`conventions.md` 与 `.ps1`/`.py` 保持 CRLF |

未执行（按任务约束）：`cargo test`、`61`/`62`/`90` 真实安装、`assets/dist` 重建、真实知识库/会话读取、`git commit`。

## 5. 备注

- 未运行 `62-verify-agent-memory-offline.ps1`；已确认该脚本与其兄弟脚本（61/63/64）不含精排探测或 `WARN` 审计相关用例，无对应用例需要删除。
- `90-audit.ps1` 现在只产出 PASS/FAIL；`INSTALL.md` 流程表 7 行「无 FAIL/待接入项」的表述仍然成立。
- [UNCERTAIN] `agent-memory/README.md` 原则句按该文件英文语境写作英文（`KB recall is not reranked: returns candidates with summaries and the calling agent judges relevance`）；如要求逐字中文，可再调整。
