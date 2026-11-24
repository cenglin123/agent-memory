# 全文检索优先 L1：部署与体检（完成记录）

状态：**L1 完成**（本文件是 L1 唯一 sentinel）。

边界：未读取任何真实会话、真实知识库内容或生产 Archive；未运行 61 真实安装；未重建 `assets/dist/agent-memory/`；未执行 git commit。所有安装/审计用例都在 `%TEMP%` 沙箱与临时 `UserRoot` 内运行。对机器状态的唯一读取是：90 / 15 的 `-PythonExe` 依赖探测（只做 `import`，不读数据）与 62 既有的真实运行时根只读指纹（`state/`+`archive/` 的相对路径、大小、SHA256）。

## 1. 实现

### 1.1 61：安装语义辅助脚本（`assets/scripts/61-install-agent-memory.ps1`）

- 新增第 2.5 段：把套件脚本 `assets\scripts\recall_embedding.py` 复制到 `<user-home>/.agent-memory/scripts/recall_embedding.py`。该位置正是 `src/recall/semantic/embed.rs::resolve_script` 的首选候选（exe 目录及其祖先的 `scripts\recall_embedding.py`，次选 `<ancestor>\assets\scripts\`）：exe 在 `.agent-memory\bin\`，其祖先 `.agent-memory\` 恰好命中。此前生产安装下配置 `embedding` 的 Archive 语义检索恒为 `script_unavailable`。
- 幂等：已安装副本哈希与套件源一致即跳过复制；不一致或缺失则重新复制。复制走 `*.new-<guid>` 暂存 → 校验暂存副本 SHA256 → `Move-Item` 原子替换，失败时清理暂存文件。
- 哈希校验：期望哈希 = 套件源文件 SHA256（单一事实源）。复制后立刻回验，不符即抛错终止安装。
- `-CheckOnly` 新增一行 `语义辅助脚本`（存在性 + 哈希 + 期望值），任一不符 exit 1。
- `install.json` 新增 `helperScript{path, sha256, source}`；幂等判据扩展到 `helperScript.sha256`（旧记录缺该字段时会被重写一次）。
- 卸载逻辑不变（删除整个程序目录，因此 `scripts\` 一并移除），运行时根一律保留；62 新增断言覆盖。
- 顺序：辅助脚本安装仍在 `Assert-AgentMemoryDistIntact` 之后，分发物缺失/被篡改时先失败，不会留下半装状态。

### 1.2 共享实现（`assets/scripts/agent-memory-common.ps1`）

新增单一实现点，61 与 90 共用：

- `$script:AgentMemoryHelperScriptName` / `$script:AgentMemoryHelperScriptRelPath`（`assets\scripts\recall_embedding.py`）；
- `Get-AgentMemoryHelperScriptSourcePath -SuiteRoot`；
- `Get-AgentMemoryHelperScriptInstallPath -UserRoot`；
- `Get-AgentMemoryHelperScriptExpectation -SuiteRoot` → `{Path, Sha256}`，源文件缺失即抛错（显式失败，不静默跳过）。

### 1.3 90：两项新审计（`assets/scripts/90-audit.ps1`）

- **「agent-memory 语义辅助脚本已安装且与分发物一致」（FAIL 级）**：已安装文件缺失 / 哈希与套件源不符 / `install.json` 未记录该哈希 → FAIL。Detail 给出 SHA256 前缀与核对依据。放在「已安装且与分发物一致」之后，`-AgentMemoryOnly` 下同样执行。
- **「知识库本地精排依赖（可选）」（WARN 级）**：用 `-PythonExe`（默认沿用原默认 `C:\Program Files\Python313\python.exe`）调用 `assets\scripts\kb_rerank_status.py`，探测 `transformers`、`torch`，`torch.cuda.is_available()` 为真时再探测 `accelerate`。缺失 → WARN（不阻断 exit 0），Detail 写明解释器路径与缺失包名；解释器不存在同样记 WARN。仅在 `-MemoryOnly`/全量审计中运行（`-AgentMemoryOnly` 跳过，避免 62 沙箱依赖 Python 环境）。
- 新增 `CheckOptional` 帮助函数与 WARN 计数：`Result` 支持 `WARN`，`exit 1` 只由 `FAIL` 触发，末尾打印 WARN 汇总行。

### 1.4 15：报告可选精排依赖（`assets/scripts/15-install-kb-deps.ps1`）

- 在版本校验之后、安装动作之前插入可选依赖报告段：`-CheckOnly` 与实装路径都会执行，**只报告不安装**。
- 就绪：`[INFO] 本地精排依赖就绪（可选）：torch / transformers 可导入（CUDA 可用|不可用）`。
- 未就绪：`[WARN] … 解释器 <path>；缺 torch, transformers` + 三条手工安装命令（torch 用 cu126 index、transformers、accelerate 注明仅 CUDA 需要）。异常一律吞掉并降级为 WARN，不改变 15 的失败语义（必需依赖缺失仍抛错）。
- 探测实现为新增脚本 `assets/scripts/kb_rerank_status.py`（标准库、离线、只读、不写文件，stdout 单行 JSON `{python, version, cuda, missing}`，退出码恒 0）。

### 1.5 62：离线验收正例与反向用例（`assets/scripts/62-verify-agent-memory-offline.ps1`）

- 沙箱额外复制 `assets\scripts\recall_embedding.py`（61 从它安装，90 用它核对）。
- 新增正例：`语义辅助脚本装到 .agent-memory\scripts 且与套件源哈希一致`（存在性、哈希、`install.json` 记录、布局说明指向 `resolve_script`）。
- 新增正例：`审计 -AgentMemoryOnly 在空白运行时根全 PASS` 追加断言，审计输出必须含 `语义辅助脚本已安装且与分发物一致`。
- 新增反向用例：删除已安装脚本 → 审计 FAIL（断言失败信息提到「语义辅助脚本」或 `script_unavailable`）；篡改已安装脚本（改成仍合法的 Python）→ 审计 FAIL（断言「哈希不符|SHA256」）；复原后审计重新 PASS。
- 卸载用例追加断言：`.agent-memory\scripts\recall_embedding.py` 不存在。
- 头部覆盖说明同步为十类反向用例 + 恢复用例。

### 1.6 其它

- `assets/scripts/00-precheck.ps1`：必备文件清单加入 `assets\scripts\kb_rerank_status.py`（`recall_embedding.py` 原已列入）。
- 文档：`assets/references/agent-memory-archive.md`（程序目录内容、61 行为 2 条、审计表新增两行、故障排查两行、62 覆盖说明、`backend=python` 启动路径）、`assets/references/conventions.md`（分发形式中「套件脚本不复制进 dist」、快照重建清单中「语义辅助脚本不参与分发物谱系」、脚本维护要点中「可选能力只报告不安装」）、`INSTALL.md`（1.5 可选组件说明与 `pip` 示例、5.6 行）、`CHANGELOG.md`（顶部「全文检索优先 L1：部署与体检」）。未改 `AGENTS.md`。

## 2. 设计决定

- **D1 辅助脚本随套件分发，不进 `assets/dist/agent-memory/`，清单不新增 `helperScripts` 字段。** 任务书的条件是「如需随分发物携带」，而 `recall_embedding.py` 与 `61`/`90`/`agent-memory-common.ps1` 同属套件脚本：它们随套件目录复制、由 Git 与 `00-precheck` 保证存在，dist 只承载必须由钉定 Rust 工具链构建的二进制。由此：61 从套件源安装，哈希写入 `install.json`，90 按套件源核对已安装副本——三处引用同一个哈希来源，不存在第二个真值。
  - **[UNCERTAIN] 与「在 build-manifest 谱系中体现其哈希」的取舍**：把哈希写进 `build-manifest.json` 需要改 `agent-memory/build/build-release.ps1`，而该文件属于 `source.treeSha256` 的构建输入。改动它会立刻让现有 dist 的「构建谱系与源码一致」项 FAIL，直到重建分发物（本任务禁止重建），62 就不可能全绿。故本轮选择不改 `build-release.ps1`、不改清单。若 orchestrator 重建时希望清单钉定，需要的改动是：清单新增 `helperScripts.recallEmbedding{name,source,sha256,bytes}`、`Format-Manifest` 与发布/`-Verify`/`-RefreshLineage` 三处写入并核对，再让 61/90 在字段存在时优先用清单哈希（缺字段时退回套件源）。这属于编排决定，未擅自实施。
- **D2 WARN 是结果级状态，不是降级通道。** 90 的 `Check` 只产出 PASS/FAIL；新增 `CheckOptional` 产出 PASS/WARN，退出码只由 FAIL 决定，避免「可选组件缺失」把整套安装判定为失败，同时保证缺失可见（表格行 + 汇总计数 + Detail 含解释器路径与包名）。
- **D3 探测逻辑单点。** 15 与 90 不做两份内联 Python 探测，统一调用 `kb_rerank_status.py`；避免两处判断漂移，也让「CUDA 可用才要求 accelerate」这一条件只写一次。
- **D4 62 只断言布局与哈希，不伪造语义可用。** `detail.semantic.status=available` 需要真实 Archive 派生代际 + embedding 配置 + `replay`，超出「不读真实 Archive / 不重建分发物」的边界。62 断言的是 `resolve_script` 依赖的不变式：exe 位于 `.agent-memory\bin\`、脚本位于 `.agent-memory\scripts\`、哈希与套件源一致。真实语义可用由 orchestrator 在本机安装后以 `memory_search` 复核。
- **D5 安装顺序与失败姿态。** 辅助脚本安装排在分发物完整性校验之后、`init` 之前：分发物有问题时先失败，不留下只装了脚本的程序目录；脚本安装失败即 exit 1，不静默降级为「语义不可用但安装成功」。

## 3. 测试与命令退出码

| # | 命令 | 退出码 | 结果 |
|---|---|---|---|
| 1 | `powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1` | 0 | 全部 32 项通过（含新增正例、三类辅助脚本反向/恢复用例） |
| 2 | `pwsh -NoProfile -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1` | 0 | 全部 32 项通过（与 PS 5.1 等价） |
| 3 | Parser 语法检查（PS 5.1）：61/62/90/15/agent-memory-common/00-precheck/build-release | 0 | 7 个文件全部 OK |
| 4 | Parser 语法检查（pwsh 7.6.9）：同上 | 0 | 7 个文件全部 OK |
| 5 | `powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\check-docs.ps1` | 0 | 6 项全 PASS，64 个 Markdown |
| 6 | `git diff --check` | 0 | 无空白/冲突标记问题 |
| 7 | 90 WARN 语义（临时套件 + 桩 60，`-MemoryOnly -UserRoot <临时>`） | 0 | 精排依赖就绪 → `PASS`，exit 0；`-PythonExe` 指向不存在文件 → `WARN`（Detail 含该路径），**exit 0**（WARN 不阻断） |
| 8 | `python -B assets\scripts\kb_rerank_status.py`（系统 3.13.17） | 0 | `{"cuda": true, "missing": []}` |
| 9 | 同上（全新 venv，无 torch/transformers） | 0 | `{"cuda": false, "missing": ["torch","transformers"]}` |
| 10 | 15 可选依赖段隔离运行（系统解释器 / venv 解释器） | 0 / 0 | `[INFO] …（CUDA 可用）` / `[WARN] … 缺 torch, transformers` + 三条 `pip` 命令 |
| 11 | `powershell -File assets\scripts\15-install-kb-deps.ps1 -CheckOnly`（本工作树） | 1 | 套件缺 `assets\packages\kb-harness\wheels`（工作树不含大体积包，属既有条件），未走到可选依赖段 |

## 4. 限制

- **未证明真实安装下 `detail.semantic.status=available`**：需要真实 Archive 派生层与 `replay`，超出本轮边界。本轮给的是布局不变式 + 哈希一致性 + 90 的机械失败项。
- **15 未在完整套件上跑通**：工作树没有 `assets/packages/kb-harness/`（wheel 与模型缓存不入工作树），因此只做了「可选依赖报告段」的隔离验证（第 10 项）。orchestrator 应在完整检出上跑一次 `15 -CheckOnly` 复核 `[INFO]/[WARN]` 输出位置。
- **90 全量审计未运行**：全量会读真实用户环境（记忆、库、CLI 状态），且任务禁止读取真实知识库。WARN 语义用 `-MemoryOnly` + 桩 60 验证；`-AgentMemoryOnly` 的 agent-memory 相关项由 62 的两次完整运行覆盖。
- **已安装旧版 61 的机器**：在新 61 运行前，90 会明确 FAIL（脚本未安装），这是预期检测而非回归；重跑 61 即修复，`install.json` 会被重写一次以补 `helperScript`。
- **62 未单列「套件源脚本缺失」反例**：该场景在 61 安装与 90 审计都会显式失败（`Get-AgentMemoryHelperScriptExpectation` 抛「来源缺失」），未额外构造用例。
- **哈希依据是套件源而非构建清单**（见 D1）：若有人只替换已安装副本，90 会 FAIL；若有人同时替换套件源与已安装副本，90 会 PASS——这与「套件脚本」的信任模型一致（同 61/90 自身），不是二进制级的构建书。

## 5. 未做

- 未改 `AGENTS.md`；未改 `agent-memory/build/build-release.ps1`；未改 `assets/dist/agent-memory/**`、未重建分发物。
- 未在 `build-manifest.json` 新增 `helperScripts` 字段（理由见 D1 的 [UNCERTAIN]）。
- 未运行 61 真实安装、未运行 15 实装、未运行 63（谱系验收，逻辑未改动）、未运行 90 全量审计。
- 未执行 git commit（由 orchestrator 执行）。

## 6. 改动文件

- `assets/scripts/61-install-agent-memory.ps1`
- `assets/scripts/62-verify-agent-memory-offline.ps1`
- `assets/scripts/90-audit.ps1`
- `assets/scripts/15-install-kb-deps.ps1`
- `assets/scripts/agent-memory-common.ps1`
- `assets/scripts/00-precheck.ps1`
- `assets/scripts/kb_rerank_status.py`（新增）
- `assets/references/agent-memory-archive.md`
- `assets/references/conventions.md`
- `INSTALL.md`
- `CHANGELOG.md`
- `agent-memory/docs/verification/FULLTEXT-L1-COMPLETED.md`（本文件）
