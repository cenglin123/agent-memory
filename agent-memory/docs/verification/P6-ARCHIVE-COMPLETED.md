# P6 · 套装离线接入（完成记录）

范围：只做离线接入——预编译分发物与构建谱系、安装脚本、审计检查与反向用例、独立临时环境验收、文档接线。**不采集任何真实会话**。

## 1. 构建谱系

| 项 | 值 |
|---|---|
| 组件 / 版本 | `agent-memory` 0.1.0（版本单点：`agent-memory/Cargo.toml`） |
| 产物 | `assets/dist/agent-memory/agent-memory.exe`（8 184 320 字节，静态 CRT） |
| 二进制 SHA256 | `931192d3228d990cfd5996a19535b02aa7aa953817478ffb470081cb51038784` |
| 可复现性 | 同工具链删除产物后重建，SHA256 字节一致（构建脚本内置双构建比对；`-Verify` 复跑通过） |
| 工具链 | `rustc 1.99.0 (b940084d7 2026-09-28)` / `cargo 1.99.0 (5f94df478 2026-08-27)` |
| 钉定 | `agent-memory/rust-toolchain.toml`：channel `1.99.0`、target `x86_64-pc-windows-msvc` |
| target / profile | `x86_64-pc-windows-msvc` / `release`，`--locked`，`RUSTFLAGS=-C target-feature=+crt-static` |
| 导入表 | `api-ms-win-core-synch-l1-2-0.dll`、`bcryptprimitives.dll`、`KERNEL32.dll`、`ntdll.dll`（无 VCRUNTIME/MSVCP/api-ms-win-crt，无 GUI 栈） |
| source commit | `null`（构建时构建输入未提交；`baseCommit` = `e4a4847e1f8f12cb75e6c6d689e03aba22808c51`，`treeDirty: true`）。提交后运行 `-RefreshLineage` 刷新为该提交并置 `treeDirty=false` |
| 源码树哈希 | `f0b3dadfa28a06c1e15936afdc89107924ee9de5a044bde32d6784e75fbdd05c`（构建输入 53 个文件：`Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`build/build-release.ps1`、`src/**`） |
| 构建时间（UTC） | `2026-10-25T18:24:16Z` |
| 构建脚本 | `agent-memory/build/build-release.ps1`（`-Verify` 只比对不写；`-RefreshLineage` 只刷新谱系） |
| 清单 | `assets/dist/agent-memory/build-manifest.json` |
| 许可 | MIT；随行 `NOTICE`（cc-switch v4.0.20 session_manager，抽取来源 commit `85ad4969b3399500baaea9d46b1e08f87aba80e1`）与 `LICENSE` |

谱系身份以 `source.treeSha256` 为准：审计在目标机重算源码树哈希并与清单比对，不依赖 Rust 工具链。`source.commit` 只在提交确实包含构建输入时记录，否则为 `null`（`baseCommit` 记当时 HEAD、`treeDirty=true`）；提交后执行 `-RefreshLineage`，`90-audit` 与 `-Verify` 按 git 现状核对这两个字段。

## 2. 交付物

- `assets/dist/agent-memory/`：`agent-memory.exe`、`build-manifest.json`、`NOTICE`、`LICENSE`
- `assets/scripts/agent-memory-common.ps1`：清单读取/内容校验/源码树哈希/PE 导入表解析（GUI 与动态 CRT）/git 谱系/凭据扫描/空根判定的单一实现点
- `assets/scripts/61-install-agent-memory.ps1`：离线安装（`-CheckOnly`、`-Uninstall`；只消费预编译分发物）
- `assets/scripts/62-verify-agent-memory-offline.ps1`：独立临时环境验收（28 项，含八类反向用例与防误伤）
- `assets/scripts/90-audit.ps1`：7 项 agent-memory 检查（含 git 谱系一致与凭据扫描），新增 `-AgentMemoryOnly` / `-AllowArchivedData`
- `assets/scripts/00-precheck.ps1`：required 清单 +7 项
- `agent-memory/rust-toolchain.toml`、`agent-memory/build/build-release.ps1`（`-Verify` / `-RefreshLineage`）、`agent-memory/tests/p6_cli_surface.rs`、`agent-memory/src/bin/agent-memory.rs`（`--help` / `--version`）
- 文档：`assets/references/agent-memory-archive.md`（新）、`conventions.md`、`README.md`、`INSTALL.md`、`AGENTS.md`、`CHANGELOG.md`、`P6-FIX-COMPLETED.md`（收口修复记录）

## 3. 测试命令与结果

```powershell
# 单元/集成/cargo 契约（仅在套件源码目录，不指向真实会话目录）
cd agent-memory; cargo test --locked
# → 368 passed; 0 failed; 1 ignored（含 tests/p6_cli_surface.rs 4 项）

# 构建 + 可复现校验（PS 5.1 与 PS 7 各跑一次，均 exit 0）
powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1
powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -Verify
pwsh -NoProfile -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -Verify
# → 双构建 SHA256 一致；VERIFY OK（静态 CRT）

# 独立临时环境离线验收（PATH 只留系统目录 → 无 rustc/cargo；代理指向 127.0.0.1:9）
powershell -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1
pwsh -NoProfile -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1
# → 28/28 PASS（两个宿主均全过）

# 审计（临时安装根；PS 5.1 与 PS 7）
powershell -ExecutionPolicy Bypass -File assets\scripts\90-audit.ps1 -AgentMemoryOnly -UserRoot <临时UserRoot>
pwsh -NoProfile -ExecutionPolicy Bypass -File assets\scripts\90-audit.ps1 -AgentMemoryOnly -UserRoot <临时UserRoot>
# → 7/7 PASS；-AllowArchivedData 分支 PASS（含合成采集数据）
```

验收覆盖与结果：

| 项 | 结果 |
|---|---|
| 实际构建 release 二进制并记录 SHA256 | 通过（见 §1） |
| 同工具链重建 SHA256 一致 | 通过（含 `+crt-static`） |
| 静态 CRT：导入表无 VCRUNTIME / api-ms-win-crt | 通过（PE 导入表 + ASCII 双重核对） |
| 临时 UserRoot 实跑安装 | 通过（程序目录 + 空运行时根，仅 `state/schema.json`、`state/device_id`） |
| `--help` / `--version` 可运行 | 通过（help 覆盖全部子命令） |
| 最小 JSON 输出（`init`/`status`/`coverage`/`capture`/`read`） | 通过（合成 codex rollout：2 条记录、覆盖 sources=1、回读内容与源记录逐字一致且 `verified=true`） |
| 重复安装保留运行时 | 通过（记录索引哈希与归档对象数未变） |
| 反向：缺失预编译二进制 → 安装失败 | 通过（exit 1，无落盘） |
| 反向：缺失分发物清单 → 安装失败 | 通过 |
| 反向：篡改分发物 → 安装失败 | 通过（翻转 1 字节后 SHA256 不符） |
| 反向：缺失/篡改/源码漂移/谱系未刷新 → 审计 FAIL | 通过 |
| 反向：运行时配置与 `install.json` 含凭据 → 审计 FAIL | 通过（2 项）；合法裸词不误报（1 项） |
| 卸载边界 | 通过（只移除程序目录，运行时根与记录索引未变） |
| 真实运行时根 | 通过（`state/` + `archive/` 文件清单与 SHA256 指纹前后一致） |
| 修改的 `.ps1`：UTF-8 BOM + `Parser::ParseFile` 语法 | 通过（00/61/62/90/common/build 全部带 BOM、0 parse error、实跑通过） |
| 无 GUI / 无动态 CRT | 通过（导入表解析，非字符串猜测） |

## 4. 真实数据

**真实数据 = 否。**

- 全程只使用 `%TEMP%` 下的沙箱套件副本、临时 UserRoot 与合成 codex rollout（虚构两行）。
- 未读写任何真实会话目录（`~/.claude`、`~/.codex`、OpenCode 数据目录），也不解析任何真实运行时根内容；62 对 `<user-home>/.session-evidence` 只取 `state/`+`archive/` 的文件清单、大小与 SHA256 指纹（完整性指纹，不读取会话语义）。
- 生产路径 `<user-home>/.session-evidence/` 未初始化；真实会话写入需用户单独授权的小范围试采（P6.4），本次未执行。
- Git 跟踪文件只写 `<suite-root>` / `<user-home>` 占位符，不含本机路径、凭据或会话内容。

## 5. 生产边界

- 生产路径的真实安装（在用户主目录跑 61）与 `90-audit -AgentMemoryOnly` 的整机复核尚未执行：本次验收在临时 UserRoot 内完成，避免在未获授权时于生产路径创建运行时根。用户确认后按 `assets/references/agent-memory-archive.md` 执行即可。
