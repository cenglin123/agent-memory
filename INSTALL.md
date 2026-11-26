# agent-memory 单组件安装指南

本目录是 agent-memory 的**单组件根**：CLI/MCP 源码、预编译分发物与最小安装脚本。
它不是完整 agent 套件——统一回忆接入（64）、完工审计（90）、宿主开场简报插件、
语义/知识库的 Python 依赖都在套件侧，不在本仓库。

## 前置

- Windows x64，PowerShell 5.1 或 7；目标机不需要 Rust/Python/Node（只消费预编译二进制）。
- `assets/dist/agent-memory/agent-memory.exe` 必须本地就位。**exe 不入 Git**
  （.gitignore 排除），fresh clone 只有 build-manifest.json、LICENSE、NOTICE。
  获取方式二选一：
  1. GitHub Release v0.1.1 资产下载 agent-memory.exe，SHA-256 必须为
     `84712fb8f219bb8c6c522a97d6a4f58dceeab8fbe8babf3303051603e1de8ea8`（15,236,608 字节）；
  2. 从完整套件 `assets/dist/agent-memory/` 复制（哈希须与 build-manifest.json 一致）。

## 安装与校验

```powershell
powershell -ExecutionPolicy Bypass -File assets/scripts/61-install-agent-memory.ps1 -CheckOnly
powershell -ExecutionPolicy Bypass -File assets/scripts/61-install-agent-memory.ps1
```

- 安装目标 `%USERPROFILE%\.agent-memory\bin\agent-memory.exe` 与语义辅助脚本
  `recall_embedding.py`；空运行时根 `%USERPROFILE%\.session-evidence`；学习底座
  `.agent-memory\learning`。不采集、不绑定知识库、不改 PATH。
- `-CheckOnly` 是**安装后校验**：未安装或哈希失配的 UserRoot 返回 1 属预期，
  不是根目录错误；`-UserRoot <临时绝对路径>` 可指向隔离根。
- `-SuiteRoot` 缺省即本仓库根；本文件（INSTALL.md）是 61 要求的根目录契约标记。
- 替换被宿主进程占用的二进制时安装器拒绝并列出占用者；退出宿主后重跑，不强杀。

## 与完整套件的边界

- 生产机推荐用**完整套件**的 61 安装：套件安装器带版本/构建身份判定
  （同哈希幂等；降级与同版本异哈希默认拒绝，显式 -AllowDowngrade 才覆盖），
  并把分发物清单原件持久化到 `.agent-memory/build-manifest.json`。
  本仓库 assets/scripts/ 是 vendored 快照，可能滞后于套件；同步走套件
  sync-agent-memory.ps1，属另行任务。
- recall.json 生成、宿主 MCP/简报接线、语义侧车与知识库依赖：运行套件 64/15/90。
- 62 离线验收依赖 90 与宿主模块，只在完整套件根运行；本根不运行 62。
- 目标机不现场构建 Rust；安装期不采集真实会话（采集须另行授权）。
