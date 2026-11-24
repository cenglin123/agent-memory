# P6 · 推广前收口（完成记录）

处理独立验收报告 F1–F5 与部署前置条件：静态 CRT 重发分发物、谱系语义与刷新、PowerShell 7 兼容、62 真实根指纹、审计凭据断言、文档同步。**不含生产安装与真实试采**。

## 1. 分发物（重建）

| 项 | 值 |
|---|---|
| 产物 | `assets/dist/agent-memory/agent-memory.exe`（8 184 320 字节） |
| 二进制 SHA256 | `931192d3228d990cfd5996a19535b02aa7aa953817478ffb470081cb51038784` |
| 源码树哈希 | `4ec6e51eb29e05dd4a7eb4e4b99166ca6388e29e1de1048856619f48c9868671`（53 个构建输入；谱系判据修复后重建） |
| 构建时间（UTC） | `2026-10-25T18:47:06Z` |
| 工具链 | `rustc 1.99.0 (b940084d7 2026-09-28)` / `cargo 1.99.0 (5f94df478 2026-08-27)`，target `x86_64-pc-windows-msvc`，`--locked` |
| 构建开关 | `RUSTFLAGS=-C target-feature=+crt-static`（清单 `toolchain.crtStatic=true`、`build.rustflags` 记录） |
| 可复现性 | 删除产物后重建两次，SHA256 三方一致（构建产物 / dist / 清单）；`-Verify` 在 PS 5.1 与 PS 7 均 exit 0 |
| 谱系 | `source.commit=null`、`baseCommit=c54edbee33515f7c47fd123ee146b2e555f1dc56`、`treeDirty=true`（构建时构建输入未提交）；提交后执行 `-RefreshLineage` 得到当时的 HEAD、`treeDirty=false`，树哈希与二进制不变。判据见 `P6-LINEAGE-FIXED.md`（允许 commit 为 HEAD 祖先） |

### 静态 CRT 证据（F2）

| 检查 | 结果 |
|---|---|
| PE 解析（IMAGE_IMPORT_DESCRIPTOR，脚本 `Get-AgentMemoryPeMetadata`） | 仅 `api-ms-win-core-synch-l1-2-0.dll`、`bcryptprimitives.dll`、`KERNEL32.dll`、`ntdll.dll` |
| 旧分发物对照 | 含 `VCRUNTIME140.dll` + 8 个 `api-ms-win-crt-*`（MSVC 动态 CRT）→ 本机有 VC++ 运行库才通过，干净机器会失败 |
| 字节级旁证 | 二进制内 ASCII 检索 `VCRUNTIME` / `MSVCP` / `ucrtbase` / `api-ms-win-crt` 均为 false |
| 构建期防线 | 构建脚本发布前解析导入表，出现动态 CRT 或 GUI 栈即拒发；`Read-AgentMemoryDistManifest` 要求 `crtStatic=true` 且 `rustflags` 含 `crt-static`；安装/审计共用 `Assert-AgentMemoryBinaryHardened` |
| 结论 | 目标机无需 VC++ 2015–2022 运行库即可运行（无动态 CRT 导入） |

## 2. 谱系与 PS 7（F1、F3）

- **F1**：`source.commit` 只在提交确实包含当前构建输入时记录；未提交时写 `null`（`baseCommit` 记 HEAD、`treeDirty=true`），清单不再指向无法重建的提交。新增 `-RefreshLineage`（不重编译，只按 git 现状重写 `commit`/`treeDirty`，已验证树哈希与二进制 SHA256 不变）。`90-audit` 与 `-Verify` 按 git 现状核对，谱系过期即 FAIL——反向用例实测：把清单改成 `commit=HEAD, treeDirty=false` 而工作区仍有 1 处构建输入改动时，审计报「清单谱系过期」并 exit 1。
- **F3**：`builtAtUtc` 改从原始 JSON 文本取值并归一，兼容 PS 5.1 的 `string` 与 PS 7.6 的 `[datetime]`。PS 7 读取「提交后刷新过」的清单形态（`commit` 非空、`treeDirty=false`）实测通过；PS 5.1 同样通过。

## 3. 验收强化（F4、F5）

- **F4**：62 对真实运行时根 `<user-home>/.session-evidence` 的 `state/` 与 `archive/` 逐文件取「相对路径 + 大小 + SHA256」并计算整体指纹，开始/结束比对；根不存在时记录「不存在，前后一致」。只取完整性指纹，不解析会话内容。
- **F5**：90 新增「凭据不入交付物与运行时根」——扫描 P6 交付物、`install.json`、运行时 `state\config.local.json`；规则为「字段名 + 赋值」（含 JSON 引号键）与已知密钥前缀（`sk-`/`ghp_`/`github_pat_`/`xox*-`/`AKIA`/私钥块/JWT）。裸词不误报；凭据样本在测试脚本里运行时拼接，交付物文本本身不含可命中字面量。

## 4. 测试与结果

```powershell
# Rust 契约（无 Rust 侧改动，回归确认）
cd agent-memory; cargo test --locked           # → 368 passed / 0 failed / 1 ignored

# 构建与校验（PS 5.1）
powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1
powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -Verify        # exit 0
powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -RefreshLineage # exit 0，哈希不变
# 构建与校验（PS 7）
pwsh -NoProfile -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -Verify    # exit 0

# 离线验收（PS 5.1 / PS 7）
powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1   # 28/28 PASS
pwsh       -NoProfile -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1   # 28/28 PASS

# 审计（临时安装根，PS 5.1 / PS 7）
powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\90-audit.ps1 -AgentMemoryOnly -UserRoot <临时根>  # 7/7 PASS
pwsh       -NoProfile -ExecutionPolicy Bypass -File assets\scripts\90-audit.ps1 -AgentMemoryOnly -UserRoot <临时根>  # 7/7 PASS
```

| 验收项 | 结果 |
|---|---|
| `cargo test --locked` | 368 passed / 1 ignored |
| `build -Verify` 双构建一致 | 通过（PS 5.1 + PS 7，静态 CRT，`VERIFY OK`） |
| 62 离线验收 | 28/28（PS 5.1 + PS 7 各一轮） |
| 90 审计 | 7/7（PS 5.1 + PS 7），含凭据扫描与 git 谱系一致 |
| PE 导入无 VCRUNTIME | 通过（导入表 + 字节级双重核对） |
| 缺失二进制 / 篡改分发物 / 源码漂移 / 谱系未刷新 | 均显式 FAIL |
| 凭据样本（`install.json`、`config.local.json`） | 均显式 FAIL；合法裸词不误报 |
| 修改的 `.ps1` | UTF-8 BOM=True、`Parser::ParseFile` 0 error、实跑通过 |

## 5. 剩余限制

- **提交后必须刷新谱系**：执行器不提交代码，本次清单为 `commit=null` / `treeDirty=true`（构建输入尚未提交）。提交本批改动后运行 `agent-memory\build\build-release.ps1 -RefreshLineage`，清单变为 `commit=<该提交>`、`treeDirty=false`（树哈希与二进制 SHA256 不变）。`90-audit` 与 `-Verify` 会强制这一步：未刷新即 FAIL。
- **生产路径未安装**：`<user-home>/.agent-memory/` 与 `<user-home>/.session-evidence/` 未创建；真实会话采集（P6.4）需用户单独授权后在限定数据源与时间范围内进行。
- **静态 CRT 的验证边界**：以 PE 导入表（无 `VCRUNTIME*`/`MSVCP*`/`ucrtbase`/`api-ms-win-crt-*`）证明不依赖 VC++ 运行库；未在「全新无 VC++ 运行库的 Windows」上做整机安装实测，推广前可在目标机用同一分发物复核 `--version` 与 `90-audit -AgentMemoryOnly`。
- **凭据扫描是模式匹配**，不能证明「不存在任何形式的秘密」（如自定义格式的密钥）；它保证常见泄露形态与「运行时元数据不应有凭据字段」的回归保护。
