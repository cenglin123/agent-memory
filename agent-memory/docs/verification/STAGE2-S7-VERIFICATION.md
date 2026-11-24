# Stage 2 S7 独立验收

verdict: 通过

本条 verdict 只覆盖本报告「独立执行记录」与「合同矩阵」中标为 `independently-reproduced` 的部分。凡标为 `orchestrator-reported` 的生产侧数字（真实 KB、真实会话、生产运行时根、真实指标时延），本审查未读取、未复算，也不作为独立证据计入。

---

## 1. 目标提交与审查边界

| 项目 | 期望 | 实测 |
| --- | --- | --- |
| Suite HEAD | `721aea0e2c10e7940dcdc2620b1aaf9a07576034` | 一致（detached，`## HEAD (no branch)`） |
| KB HEAD | `db9a16ca9b3cb953d99020222e463acec36907db` | 一致（detached，`## HEAD (no branch)`） |
| Suite 初始状态 | 干净 | `git status --porcelain -uall` 输出 0 字节 |
| KB 初始状态 | 干净 | `git status --porcelain -uall` 输出 0 字节 |

工作树完整性基线（审查前后各取一次，逐字节相同）：

| 工作树 | `git ls-files -s` 的 SHA-256 | 审查后 status |
| --- | --- | --- |
| `<suite-worktree>` | `7438e43b4fd5258c04ef60e8c5cc0d643ffdc3e6f5c8a346caaac5ecf3f76b2c` | 0 字节 |
| `<kb-worktree>` | `7ba07612187215b7be1de2e901c2a5b385057d47f8065d1c45f4927d4686f9f6` | 0 字节 |

允许读取范围：`<suite-worktree>`、`<kb-worktree>`、脱敏指标摘要，三者均为只读。两份工作树都是 detached checkout，且 `.git` 是指向既有共享对象库的链接文件；`git rev-parse` / `git ls-files` 等 plumbing 因此会解析该对象库中的套件源码对象（与工作树内容同一份源码），未读取任何会话、证据、笔记或凭据对象。

未读取（本轮全程）：`.session-evidence`、生产 `recall.json`、canonical KB、真实会话目录、agent JSONL、真实 KB search/read stdout、任何笔记正文、任何凭据值。未联网：`cargo test` 带 `CARGO_NET_OFFLINE=true`；其余构建/脚本只使用本机已缓存 crate 与已钉定工具链，未触发下载。

一处需披露的自我边界偏差：审查开始时在 `C:\wt` 上执行过一次目录列举（仅目录名，用于定位两个输入工作树），随后即收紧到允许范围；该次列举未打开任何文件、未读取任何证据内容。

---

## 2. 独立执行记录（命令、退出码、计数）

### 2.1 HEAD 与初始状态

```
git -C <suite-worktree> rev-parse HEAD             -> 721aea0e2c10e7940dcdc2620b1aaf9a07576034
git -C <kb-worktree>    rev-parse HEAD             -> db9a16ca9b3cb953d99020222e463acec36907db
git -C <suite-worktree> status --porcelain -uall   -> 0 字节
git -C <kb-worktree>    status --porcelain -uall   -> 0 字节
```

### 2.2 Suite Rust 测试

```
cd <suite-worktree>/agent-memory
CARGO_TARGET_DIR=<review-temp>\cargo-target CARGO_NET_OFFLINE=true cargo test --locked
```

- 退出码：**0**
- 统计：**32 个测试二进制 / 719 passed / 0 failed / 3 ignored**（含 doc-tests 5 项）
- 构建产物只写入 `<review-temp>\cargo-target`，未写入输入工作树。

### 2.3 KB 合成测试

```
cd <kb-worktree>
TEMP=<review-temp>\tmp TMP=<review-temp>\tmp \
python -I -B -m pytest .meta/tests/test_ask_readonly_json.py .meta/tests/test_script_assets.py -q -p no:cacheprovider
```

- 退出码：**0**
- 统计：**37 passed, 1 warning, 9 subtests passed**（`test_ask_readonly_json.py` 32 项，`test_script_assets.py` 5 项）
- 唯一 warning：`.meta\scripts\governance_check.py:22: SyntaxWarning: invalid escape sequence '\p'`（非阻断，见 §5 N3）
- 附加参数 `-p no:cacheprovider` 仅用于阻止 pytest 在输入工作树内创建 `.pytest_cache`；`-B` 阻止 `__pycache__`。未调用真实 `ask.py` 查询，全部为合成/离线用例。

### 2.4 分发物从源码的可复现重建（关键独立证据）

在审查者自有沙箱 `<review-temp>\s63`（内容为 `<suite-worktree>` 在目标提交的全部构建输入副本）中执行：

```
powershell.exe -NoProfile -ExecutionPolicy Bypass -File <sandbox>\agent-memory\build\build-release.ps1 \
  -SuiteRoot <sandbox> -DistDir <sandbox>\assets\dist\agent-memory -SkipReproCheck
```

- 退出码：**0**
- 重建产物 SHA-256：`8c4a1c890f69ccb027d64a0a3ae1d87d268134892a33bad52fdd4db9c3198a2b`，大小 10933248 字节
- 该哈希与套件内已提交清单 `assets/dist/agent-memory/build-manifest.json` 的 `binarySha256` **完全一致**，也与脱敏摘要声明的分发物/生产二进制哈希一致。
- 用共享实现 `Get-AgentMemorySourceTreeHash` 独立计算目标提交的构建输入树：`83947119b75613844e2b36901b0b806203c954464ab40e82cd82cdb9154b56e7`，`treeFiles=80`，与清单 `source.treeSha256`/`treeFiles` 及脱敏摘要的「distribution tree hash」一致。

结论：脱敏摘要里的「分发物哈希 = 生产哈希」在本审查中获得了**上游可复现性**支撑 —— 被审查的提交内容可以逐字节重建出该二进制。这也是后续 62/63 能在隔离环境跑通的前提。

### 2.5 62 离线验收脚本

**A. 直接在 `<suite-worktree>` 上执行（记录原始失败，未做任何修补掩盖）**

```
powershell.exe -NoProfile -ExecutionPolicy Bypass -File <suite-worktree>\assets\scripts\62-verify-agent-memory-offline.ps1
```

- 退出码：**1**
- 执行条目：28（预期 43），结果 **3 PASS / 25 FAIL**
- 首个失败原样记录：`预编译分发物完整（清单 + 二进制 + 许可 + 无 GUI 依赖）  FAIL  预编译二进制缺失：<review-temp>\tmp\agent-memory-offline-...`
- 根因（可判定，非产品缺陷）：`assets/dist/agent-memory/agent-memory.exe` 被 `.gitignore` 的 `*.exe` 规则排除，`git ls-files assets/dist` 只有 `LICENSE`、`NOTICE`、`build-manifest.json`。纯 git checkout 不含该二进制，脚本登记处 `Assert-AgentMemoryDistIntact` 直接失败，并因后续流程抛错提前终止（末项 `验收流程未跑完`）。
- 换言之：**只拿到 git checkout 的独立审查者无法按字面复跑 62**，这是交付形态问题，见 §5 N1。

**B. 在审查者沙箱中复跑（唯一增加物 = 上一步逐字节重建的分发物）**

```
powershell.exe -NoProfile -ExecutionPolicy Bypass -File <sandbox>\assets\scripts\62-verify-agent-memory-offline.ps1 -SuiteRoot <sandbox>
```

- 退出码：**0**
- 结果：**43/43 PASS**，条目数与 orchestrator 报告一致。
- 覆盖要点（全部由本次执行独立产出）：离线安装 + 空运行时根、辅助脚本哈希、CLI help/version、90 空根审计、采集/覆盖/回读哈希、zstd 对象与 compact 迁移幂等、status 存储分组、压缩对象凭据扫描（仅计数不含原值）、全文索引建索引、`resolved_mode=hybrid` 缺省、`exact` 命中、删除缓存后回落 `legacy`、重复安装幂等、`-CheckOnly`、10 项反向用例（缺二进制/缺清单/篡改分发物/源码漂移/各类凭据与篡改）。
- 唯一被判为“空过”的条目：`全程未触碰真实运行时根` 显示 `根不存在，前后一致`。因为本审查把 `USERPROFILE` 重定向到审查者沙箱，脚本的真实运行时根指纹对象不存在。**因此生产运行时根未被改写这一点仍属 orchestrator-reported，不是我的独立结论**（见 §5 N4）。

### 2.6 63 谱系验收脚本

**A. 直接在 `<suite-worktree>` 上执行**

```
powershell.exe -NoProfile -ExecutionPolicy Bypass -File <suite-worktree>\assets\scripts\63-verify-agent-memory-lineage.ps1
```

- 退出码：**1**，结果 **16/19 PASS**，3 项失败（`端到端：HEAD~1 谱系下安装 + 90 审计通过（PS 5.1）`、对应 PS 7 项、`端到端：HEAD~1 谱系下 build -Verify 通过`），均因分发物二进制缺失。

**B. 沙箱复跑（含重建二进制）**

- 第一次：退出码 **1**，**18/19 PASS**。唯一失败项原样记录：
  `端到端：HEAD~1 谱系下 build -Verify 通过  FAIL  沙箱重建失败：来源：HEAD ...；构建输入 80 个文件... rustc 不可用：error: rustup could not choose a version of rustc to run, because one wasn't specified explicitly, and no default is configured.`
- 该失败的归因经隔离实验确认为**审查侧环境人造因素**，不是产品缺陷：本次运行我重定向了 `USERPROFILE`，而 `build-release.ps1` 通过 `cmd /c "rustc -V"` 取版本，rustup 的默认工具链设置存放于 `%USERPROFILE%\.rustup`，重定向后变空，故报“无默认工具链”。在未重定向 `USERPROFILE` 的同一沙箱中，同一步骤通过（`VERIFY OK`）。
- 第二次（仅取消 `USERPROFILE` 重定向；63 本身不引用 `$env:USERPROFILE`，不影响其边界）：退出码 **0**，**19/19 PASS**，含 `端到端：HEAD~1 谱系下 build -Verify 通过  PASS  VERIFY OK（HEAD~1 谱系）`。

### 2.7 输入工作树最终状态

```
git -C <suite-worktree> status --porcelain -uall -> 0 字节；ls-files 摘要不变；HEAD 不变
git -C <kb-worktree>    status --porcelain -uall -> 0 字节；ls-files 摘要不变；HEAD 不变
```

需披露：某一轮 PowerShell 在以 `<suite-worktree>` 为当前目录、且 `LOCALAPPDATA` 未传入的进程环境里启动，PS 5.1 把模块分析缓存写成了工作树内的未跟踪文件 `Microsoft/Windows/PowerShell/ModuleAnalysisCache`（80489 字节，创建时间与本轮运行一致）。该文件已删除，工作树恢复为初始状态，`ls-files` 摘要与审查前逐字节相同。除此之外未对两个输入工作树做任何写入、清理、提交或检出操作。

---

## 3. 合同矩阵

标签含义：`independently-reproduced` = 本次在隔离环境亲自执行/亲自核对代码得出；`orchestrator-reported` = 仅来自脱敏摘要，本审查未读取生产证据、不予背书。

### S5

| 合同项 | 结论 | 标签 |
| --- | --- | --- |
| 22 正例 + 7 负例；exact recall@5 = 0.864；exact∪hybrid = 1.000；hybrid = 0.727 | 需真实 KB 语料，未复算 | orchestrator-reported |
| 负例空结果率 exact 0.571 / hybrid 0.571 / union 0.286（含 hybrid 负例行为观察） | 同上 | orchestrator-reported |
| 独立 CLI 时延不与 MCP 常驻时延混算的口径声明 | 口径自洽，无可复算数据 | orchestrator-reported |
| 检索模式合同：`mode` 取值 exact/hybrid/legacy/auto；`source_status[].detail.resolved_mode` 冻结值集合 | 代码核对一致（`search.rs` 模式解析、`contract.rs` 常量） | independently-reproduced |
| 全文索引行为：建索引、缺省 hybrid、exact 命中、带 since、删缓存回落 legacy | 62 沙箱执行全部 PASS（合成 Codex JSONL） | independently-reproduced |
| Rust 侧检索/索引/契约回归 | `cargo test --locked` 719 passed / 0 failed | independently-reproduced |

### S5.1

| 合同项 | 结论 | 标签 |
| --- | --- | --- |
| exact 路由实现：KB 且无 `session_id` 的显式 exact 走 `kb_lexical` | 核对 `search.rs` 显式 exact 分支：`source.kind == Kb && session_id.is_none()` → `annotate_exact_kb_lexical` + `resolved=RESOLVED_MODE_KB_LEXICAL` | independently-reproduced |
| `resolved_mode=kb_lexical` 是“解析路径”而非请求 mode 值 | `contract.rs` 常量与注释一致；`detail.exact.mode` 同步标注 | independently-reproduced |
| `score_kind=rank_basis=native_bm25` | `kb.rs` 由子进程 `detail.mode` 单点派生；`contract.rs` 中 `KB_MODE_BM25 → native_bm25`，且 `rank_basis_for_mode` 复用同一生产者 | independently-reproduced |
| 子进程调用加 `--bm25`，Rust 侧做整词过滤（不重复实现 BM25） | `kb.rs` `exact_terms` 分支：行级 `row_carries_all_terms` 过滤，未命中行按“过滤决策”丢弃 | independently-reproduced |
| `ask.py --json --bm25` 单信封、`detail.mode="bm25"` | 代码核对 + KB 测试 5 项 bm25 用例（含无命中、索引异常降级、非法组合、单信封）全部通过 | independently-reproduced |
| `CONTENT_UNRESOLVED` → PARTIAL → 退出码 10（“有结果 + 有降级源”） | `contract.rs` `status_for_code`、`DEGRADED_STATUSES` 含 PARTIAL、`search_exit_code` 返回 10；`kb.rs` 仅在存在未解析行时置 PARTIAL/`CONTENT_UNRESOLVED` | independently-reproduced |
| 生产实跑：CJK 1.679 s、ASCII 1.801 s/exit 10、default/legacy/hybrid/legacy 各 5 命中、回读成功 | 需生产 KB，未执行（本任务亦禁止） | orchestrator-reported |
| 非 exact 请求保持 `native_hybrid`、不进入 `kb_lexical` | 代码路径核对支持该断言（`kb_lexical` 仅在显式 exact 成功路径写入） | independently-reproduced（代码层） |
| 真实 KB 候选存在 `CONTENT_UNRESOLVED` 歧义；维护期重叠曾出现 `SIDE_EFFECT_VIOLATION` | 未复现 | orchestrator-reported |

### S6

| 合同项 | 结论 | 标签 |
| --- | --- | --- |
| Archive 删除探针：仅删隔离副本，生产源/记录态/manifest 计数不变 | 未执行 | orchestrator-reported |
| 初始索引 30.314 s/230 行；删后 0.527 s/仍 230 行；exact 0.008 s、hybrid 0.007 s 命中同会话、`origin=archive`；回读 0.805 s、三段、range SHA 匹配 | 未执行 | orchestrator-reported |
| `origin=live/archive` 合同 | `contract.rs` 冻结 `ORIGIN_LIVE`/`ORIGIN_ARCHIVE`，与摘要字段一致 | independently-reproduced（代码层） |
| 引用/回读冻结键集（ref 8 键、read 9 键） | `REF_KEYS` 8 项、`READ_RESPONSE_KEYS` 9 项，核对一致 | independently-reproduced |
| MCP 常驻时延 119 次热查询：p50 182.4 ms / p95 820.8 ms / max 1500.1 ms；exact p95 711.3 ms / hybrid 820.8 ms；OpenCode exact p95 1291.5 ms 作为尾部观察 | 未执行（需真实 MCP 常驻与真实会话） | orchestrator-reported |
| fresh-context agent：30.421 s、4 search + 3 read、无辅助工具、首读 8.645 s | 未执行 | orchestrator-reported |
| 62 中 archive 相关合成合同（采集/回读/对象哈希/压缩） | 43 项中相关条目全部 PASS | independently-reproduced（合成数据） |

### S7

| 合同项 | 结论 | 标签 |
| --- | --- | --- |
| 分发物 SHA-256 `8c4a1c89…`、tree `83947119…`、字节数 10933248 | 由被审查提交的构建输入重建，三值全部命中 | independently-reproduced |
| 61 `-CheckOnly` 4/4 PASS | 未执行；仅核对脚本确有且仅有 4 个检查项（分发物完整/已安装二进制/语义辅助脚本/运行时根已初始化），条目数与 4/4 自洽 | orchestrator-reported（结构自洽） |
| 62 离线验收 43/43 PASS | 沙箱复跑 43/43 PASS（直接 checkout 复跑为 25 FAIL，原因已定位为 gitignored 二进制） | independently-reproduced（沙箱，含重建二进制） |
| 63 谱系验收 19/19 PASS（含 PS 5.1 与 PS 7） | 沙箱复跑 19/19 PASS | independently-reproduced（沙箱，含重建二进制） |
| 64 `-CheckOnly`：recall 配置、MCP 注册、提示词路由、KB harness、embedding/Ollama、计划任务、采集 | 未执行（会读取生产配置，超出允许范围） | orchestrator-reported |
| 90 生产 `-AgentMemoryOnly`：10 PASS + 1 预期 WARN，exit 0 | 未执行；仅核对代码中该模式下共 11 个条目（9 个固定 Check + 凭据扫描项 + 全文索引项 + 周期刷新项），与 10+1 自洽 | orchestrator-reported（结构自洽） |
| 90 WARN：148/5041 证据对象匹配凭据模式，仅计数；交付文件与运行时配置凭据检查 PASS | 未执行 | orchestrator-reported |

### 摘要标注核对（任务第 6 项）

- 62/63 在摘要中位于 `## Final orchestrator checks`，摘要同时明确 62 是「offline isolated acceptance」、63 是「lineage acceptance」，与「生产侧 vs 隔离侧」区分一致。
- 90 明确写为「90 production AgentMemoryOnly audit」，标注为生产侧，符合要求。
- 61 与 64 的条目（「distribution, installed binary, helper and runtime root」「recall config, MCP registration, prompt route…」）描述的是已安装/生产环境的检查，但**没有像 90 那样显式写出 “production”**。属于标注精度问题，不影响结论方向（它们本就不属于隔离脚本）。见 §5 N5。

---

## 4. 阻断项

无。

本审查未发现可实现层面的缺陷：目标提交与声明一致；Rust 与 KB 测试全绿；S5.1 exact 路由、`kb_lexical`/`native_bm25` 派生、整词过滤与 exit 10 语义与公开合同一致；分发物可由被审查源码逐字节重建；62/63 在具备该分发物的隔离环境中分别 43/43、19/19 全通过（与 orchestrator 报告条目数一致）。

---

## 5. 非阻断观察项

**N1（流程，建议记录而非修复）** 62/63 无法在纯 git checkout 上运行，因为 `assets/dist/agent-memory/agent-memory.exe` 被 `.gitignore` 的 `*.exe` 排除。本次通过「从同一提交重建并被验证哈希一致」补齐，闭环可行，但这条路径没有写在验收说明里；若把 62/63 作为交给外部审查者的独立验收项，需同时给出「先重建分发物」的前置步骤，否则复跑必然失败。

**N2（工具链健壮性）** `build-release.ps1` 用 `cmd /c "rustc -V"` 取版本，依赖环境中的 rustup 默认工具链（`%USERPROFILE%\.rustup`）或当前目录向上找到的 `rust-toolchain.toml`。在重定向 `USERPROFILE`（或 `RUSTUP_HOME` 与工作目录均不含钉定信息）时，会以 `rustup could not choose a version of rustc` 失败，且失败信息不含“环境缺失”的提示。本次 18/19 的那次运行即由此引起。建议（未来）在脚本内显式设置 `RUSTUP_TOOLCHAIN` 或先 `cd` 到 crate 根，减少隔离审查的假阴性。

**N3（KB 静态告警）** `python -I -B -m pytest` 期间出现 `.meta\scripts\governance_check.py:22: SyntaxWarning: invalid escape sequence '\p'`（文档字符串中的 Windows 路径）。不影响 37 项测试结果，但会污染以后的告警基线。

**N4（独立证据边界）** 62 的「全程未触碰真实运行时根」在本次执行中为**空过**：为避免读取 `.session-evidence`，我把 `USERPROFILE` 指向审查者沙箱，脚本因此报“根不存在”。生产运行时根未被本次验收改写的结论，仍只来自 orchestrator，本审查未独立验证。

**N5（标注精度）** 见 §3 末段：61/64 未像 90 一样显式标注为生产侧结果，虽然其范围明显属于生产/已安装环境。

**N6（审查侧污染，已消除）** 见 §2.7：一次 PowerShell 运行在 `<suite-worktree>` 根目录留下 `Microsoft/Windows/PowerShell/ModuleAnalysisCache`，已删除并复核工作树恢复原状。该现象同时提示：以工作树为 cwd 运行 PowerShell 前应确保 `LOCALAPPDATA` 已传入，否则缓存会落进仓库。

**N7（未复算口径）** 摘要中 S5 的 recall 数值、S6 的时延与删除探针、S7 的 61/64/90 生产审计结果均需生产环境与真实数据，本审查按任务边界不做补验；这些数字在本报告中一律不构成独立证据。

---

## 6. reads: 审计列表

以下为本轮实际读取的全部位置（按工作树归类），除此之外未打开任何文件。

`<suite-worktree>`（只读）

- 目录列举：仓库根、`assets/scripts`、`agent-memory`、`assets/dist/agent-memory`
- `.gitignore`、`agent-memory/rust-toolchain.toml`、`agent-memory/Cargo.toml`（grep 片段）
- `assets/dist/agent-memory/build-manifest.json`
- `assets/scripts/62-verify-agent-memory-offline.ps1`（全文）
- `assets/scripts/63-verify-agent-memory-lineage.ps1`（全文）
- `assets/scripts/61-install-agent-memory.ps1`（`-CheckOnly` 段与相关行）
- `assets/scripts/90-audit.ps1`（param 行、检查项枚举、agent-memory 段与退出段）
- `assets/scripts/agent-memory-common.ps1`（树哈希/进程调用/运行时根校验相关函数）
- `agent-memory/build/build-release.ps1`（全文关键段）
- `agent-memory/src/recall/search.rs`（模式解析、exact/hybrid 分支、标注函数）
- `agent-memory/src/recall/contract.rs`（KB 模式、score_kind/rank_basis、ref/read 键集、状态与退出码）
- `agent-memory/src/recall/providers/kb.rs`（子进程调用、整词过滤、CONTENT_UNRESOLVED）
- `agent-memory/src/recall/providers/kb.rs`、`mcp.rs` 中与 `resolved_mode` 描述相关的行

`<kb-worktree>`（只读）

- 目录列举：仓库根、`.meta`、`.meta/tests`、`.meta/scripts`
- `.meta/tests/test_ask_readonly_json.py`、`.meta/tests/test_script_assets.py`（grep 片段）
- `.meta/scripts/ask.py`（`--json`/`--bm25` 相关段）
- 测试输出中出现的 `.meta/scripts/governance_check.py:22` 告警行（仅 pytest 回显，未打开该文件正文）

脱敏输入

- `<user-home>\.ocsr-work\s7-verification-input.md`（全文，用于矩阵归类，不作独立证据）

审查者自建产物（`<review-temp>` 与 `<review-output>`）

- `<review-temp>\cargo-target`、`<review-temp>\s63`（重建沙箱）、`<review-temp>\tmp`（脚本 TEMP）、各步骤日志与 `*-before.txt` 基线文件
- `<review-output>\STAGE2-S7-VERIFICATION.md`（本报告）

**未读取**：`.session-evidence`（任何文件）、生产 `recall.json`、canonical KB（任何路径）、真实会话目录、`.session-evidence` 指纹、agent JSONL、任意笔记正文、任意凭据值、canonical suite 安装目录、生产 MCP 配置。**未联网**。
