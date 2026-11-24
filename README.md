# agent-memory（记忆底座）

本机跨宿主的历史记忆与知识库检索底座：Rust 实现，提供 `memory_search` / `memory_read` / `memory_recent` / 开场简报（brief），通过 MCP 暴露给 Claude Code、OpenCode、Codex、DSH、Hermes 等宿主，并把索引与运行时留在本机。

本仓是 agent-memory 的**独立源码仓**。原 `20260703-agent智能体使用套装整理` 套件只保留发布产物（`assets/dist/agent-memory`）与安装/校验脚本；需要源码时按 `git pull` 取回。

## 目录

```
agent-memory/            Rust crate（src、tests、build、docs、Cargo.*）
assets/skills/agent-memory/   随项目分发的 Skill（SKILL.md + references）
assets/scripts/          安装、离线校验、谱系校验与共享函数
assets/dist/agent-memory/     发布产物（agent-memory.exe + build-manifest.json；exe 不入库）
```

布局与套件内的相对路径一致，`build/build-release.ps1` 与 `61/62/63` 脚本无需改动即可在本仓根目录运行。

## 构建与测试

```powershell
cd agent-memory
cargo fmt --check
cargo test --offline --tests        # 75 个目标，离线可跑
```

工具链钉定在 `agent-memory/rust-toolchain.toml`（1.99.0 / x86_64-pc-windows-msvc），发布构建使用静态 CRT：

```powershell
powershell -ExecutionPolicy Bypass -File agent-memory/build/build-release.ps1
```

产物：`assets/dist/agent-memory/agent-memory.exe` + `build-manifest.json`（含工具链、源码树哈希、二进制 SHA-256、谱系提交）。

## 安装

```powershell
powershell -ExecutionPolicy Bypass -File assets/scripts/61-install-agent-memory.ps1 -CheckOnly
powershell -ExecutionPolicy Bypass -File assets/scripts/61-install-agent-memory.ps1
```

安装目标：`%USERPROFILE%\.agent-memory\bin\agent-memory.exe`，运行时根 `%USERPROFILE%\.session-evidence`，配置 `%USERPROFILE%\.agent-memory\recall.json`。

替换正在被宿主进程占用的二进制时，安装器会拒绝；先保存并退出宿主，不要强杀。

## 与套件、与两个 SKILL 的边界

- **套件**：只消费发布产物。套件内的 `agent-memory/` 是本仓的检出目录（`git pull` 更新），不参与套件自身的提交。
- **init-kb-harness**：产出知识库与 `.meta/summaries/**`；本仓的 grounding 读取其 frontmatter `source`。
- **init-agent-docs**：定义 `AGENTS.md` / `.meta` / `docs/plans` 等结构；本仓把这些作为可索引的文档面。

这些是**数据契约**，不是代码依赖。契约变更（目录、frontmatter、会话来源格式）必须同时更新本仓文档与两个 SKILL，并按契约版本标注。

## 许可

见 `agent-memory/LICENSE` 与 `agent-memory/NOTICE`（上游 cc-switch v4.0.20 session_manager，MIT）。
