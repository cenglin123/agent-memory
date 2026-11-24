# P6 · 谱系自指校验修复（完成记录）

修复独立复核阻断项 F-1：`manifest.source.commit == HEAD` 对任何**已提交**清单都不可满足（写入清单后必须再提交，HEAD 必然变化），导致 `-Verify` 与 `90-audit` 在最终 commit 上必然失败。改为语义判据并在两种 PowerShell 宿主下补齐正向/反向测试。

## 1. 修改

| 文件 | 修改 |
|---|---|
| `assets\scripts\agent-memory-common.ps1` | 新增 `Get-AgentMemoryBuildScopes`、`Test-AgentMemoryCommitAncestry`（存在性 + `merge-base --is-ancestor`）、`Test-AgentMemoryCommitContainsInputs`（`git diff --quiet <commit> -- <scope>`）、`Assert-AgentMemoryLineage`（唯一判据实现，90 与 `-Verify` 共用）。`Read-AgentMemoryDistManifest` 取消 `commit == baseCommit` 要求（`commit` 可为 HEAD 祖先），保留「treeDirty=true ⇒ commit=null」。 |
| `assets\scripts\90-audit.ps1` | 「构建谱系与源码一致」改用 `Assert-AgentMemoryLineage`；保留 treeSha256 / 工具链 / target 校验与既有输出。 |
| `agent-memory\build\build-release.ps1` | `-Verify` 改用同一 `Assert-AgentMemoryLineage`；`-RefreshLineage` 语义不变（提交后按 git 现状写入当时 HEAD，此后成为后续 HEAD 的祖先）。 |
| `assets\scripts\63-verify-agent-memory-lineage.ps1` | 新增谱系语义验收：临时 git 仓库（`git init` + `commit-tree`/`update-ref`），19 项场景 × PS 5.1/PS 7。 |
| `assets\scripts\00-precheck.ps1` | required 清单 + `63-verify-agent-memory-lineage.ps1`。 |
| 文档 | `agent-memory-archive.md`（判据表 + 谱系语义）、`conventions.md`、`AGENTS.md`（祖先判据 + 63 入口）、`CHANGELOG.md`、`CURRENT.md`、`P6-FIX-COMPLETED.md`（谱系/树哈希/时间同步为当前值）。 |

判据（不满足即 FAIL）：

1. `treeDirty=false` ⇒ 工作区构建输入改动数必须为 0（`git status --porcelain -- <scope>`）；
2. `source.commit` 必须存在（`cat-file -e`），且是 HEAD 的祖先（`merge-base --is-ancestor`，与 HEAD 相等也算）；
3. 该提交的构建输入内容必须与当前工作区逐字节一致（`git diff --quiet <commit> -- <scope>`），等价于清单 `treeSha256` 所代表的内容；
4. `treeDirty=true` ⇒ 工作区必须确有构建输入改动（commit 为 `null`），否则报「谱系过期」。

非 git 副本（目标机整目录拷贝）跳过第 2–3 项并明确说明；treeSha256 与二进制哈希校验始终生效。

## 2. 测试

### 2.1 谱系语义（`63-verify-agent-memory-lineage.ps1`，临时仓库）

| 场景 | 期望 | PS 5.1 | PS 7 |
|---|---|---|---|
| commit=HEAD，构建输入一致 | 通过 | PASS | PASS |
| **commit=HEAD~1**（后续提交只改非构建输入），构建输入一致 | 通过 | PASS | PASS |
| commit 不存在（40×0） | 失败（「在仓库中不存在」） | PASS | PASS |
| commit 存在但非 HEAD 祖先（独立根提交） | 失败（「不是 HEAD 的祖先」） | PASS | PASS |
| 祖先提交但构建输入内容不同（改了 Cargo.lock 未重发） | 失败（「构建输入与当前工作区不一致」） | PASS | PASS |
| treeDirty=false 但工作区有构建输入改动 | 失败（「工作区有 1 处构建输入改动」） | PASS | PASS |
| treeDirty=true 但工作区干净（提交后未刷新） | 失败（「清单谱系过期」） | PASS | PASS |
| 端到端：HEAD~1 谱系下 61 安装 + 90 审计 | 通过（7 项） | PASS | PASS |
| 端到端：非祖先 commit 下 90 审计 | 失败（exit 1） | PASS | PASS |
| 端到端：HEAD~1 谱系下 `build -Verify` | 通过（`VERIFY OK`） | PASS | — |

结果：**19/19 PASS**（`-Verify` 端到端只在 PS 5.1 下跑：跨目录构建的字节不保证相同，沙箱内先按当前路径重建再校验；PS 7 的 `-Verify` 在真实仓库覆盖，见 2.3）。

### 2.2 真实仓库（当前状态：构建输入 1 处未提交 → `commit=null`、`treeDirty=true`）

```powershell
powershell -File agent-memory\build\build-release.ps1 -Verify        # exit 0，VERIFY OK（静态 CRT，commit null，base c54edbee…）
pwsh       -File agent-memory\build\build-release.ps1 -Verify        # exit 0
powershell -File assets\scripts\62-verify-agent-memory-offline.ps1   # 28/28，exit 0
pwsh       -File assets\scripts\62-verify-agent-memory-offline.ps1   # 28/28，exit 0
powershell -File assets\scripts\90-audit.ps1 -AgentMemoryOnly -UserRoot <临时根>   # 7/7，exit 0
pwsh       -File assets\scripts\90-audit.ps1 -AgentMemoryOnly -UserRoot <临时根>   # 7/7，exit 0
```

`cargo test --locked`：368 passed / 0 failed / 1 ignored。

### 2.3 复核报告原失败形态

复核报告中的失败形态（清单 `commit=26047ac`、HEAD `c54edbe`、工作区干净、树一致）属于本轮判据的**正向前提**：`26047ac` 是 HEAD 祖先，且其构建输入与工作区一致 → 新判据通过。沙箱以 `commit=HEAD~1` 复现同一形态并验证 `-Verify` 与 90 均通过（2.1）。

## 3. 分发物

| 项 | 值 |
|---|---|
| 二进制 SHA256 | `931192d3228d990cfd5996a19535b02aa7aa953817478ffb470081cb51038784`（8 184 320 字节，静态 CRT，未变） |
| 源码树哈希 | `4ec6e51eb29e05dd4a7eb4e4b99166ca6388e29e1de1048856619f48c9868671`（53 个构建输入） |
| 构建时间（UTC） | `2026-10-25T18:47:06Z` |
| 谱系 | `commit=null`、`baseCommit=c54edbee33515f7c47fd123ee146b2e555f1dc56`、`treeDirty=true`（构建脚本改动未提交）；双构建 SHA256 一致 |

## 4. 剩余限制

- **提交后仍需刷新谱系**：本批改动未提交，清单为 `commit=null`/`treeDirty=true`。提交后运行 `agent-memory\build\build-release.ps1 -RefreshLineage` 得到 `commit=<该提交>`、`treeDirty=false`；此后 HEAD 再前进也保持有效（祖先判据）。90 与 `-Verify` 会强制该步（谱系过期即 FAIL）。
- **跨目录构建不可字节复现**：编译器会嵌入路径，同一源码在不同目录构建的 SHA256 不同。`-Verify` 必须与分发物同路径运行（维护机原位）；沙箱端到端因此先在沙箱内重建再校验。这不是判据缺陷，已写入 63 的实现注释。
- **谱系判据依赖 git**：目标机为整目录拷贝（无 `.git`）时跳过祖先/内容核对，仅校验 `treeSha256` 与二进制哈希；这不降低目标机的分发物完整性校验强度。
- **未做生产安装与真实试采**；`<user-home>/.agent-memory/` 与 `<user-home>/.session-evidence/` 未创建。
