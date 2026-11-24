# 统一回忆安装接入与目标环境升级分流 — 完成记录

- 日期：2026-10-28
- 范围：套件 Windows 安装脚本与文档
- 真实数据使用：**否**（全程合成 HOME、合成 cc-switch 数据库、损坏的假 OpenCode DB；未读取真实会话、真实配置或真实数据库，未执行 Archive 采集，未重建分发物或修改 manifest）

## 1. 实现

| 文件 | 内容 |
|---|---|
| `assets/scripts/64-install-recall.ps1` | PS 入口：必选 `-VaultPath`；`-OpenCodeRoot`/`-CodexRoot` 由显式 `-UserRoot` 推导标准路径后作为参数传入；`-SkipOpenCode`/`-SkipCodex`、`-CheckOnly`、`-Smoke`、`-PythonExe`；`CheckOnly` 与 `Smoke` 互斥并以非零退出 |
| `assets/scripts/recall_onboarding.py` | 标准库实现：recall.json 生成/合并、JSONC 解析与 MCP 合并、cc-switch 提示词同步、CheckOnly 检查、显式 Smoke |
| `assets/scripts/test_recall_onboarding.py` | 20 项隔离回归用例 |
| `assets/scripts/00-precheck.ps1` | 必需文件加入 64 与辅助 Python 文件 |
| `UPGRADE.md` | 区分 80 资产升级与目标环境升级/迁移；两条路径、保留边界、备份与回滚 |
| `INSTALL.md`、`README.md`、`AGENTS.md`、`docs/STRUCTURE.md`、`INSTALL-macos.md` | 61 仅 CLI、64 完成发现；导航与术语同步；macOS 明确不冒充全量 |
| `assets/references/agent-memory-archive.md`、`conventions.md` | 64 行为、边界、排障与维护约定 |

行为要点：

- `recall.json`：`version = 1`；`source_order` 精确覆盖 enabled 源；只更新 `kb`/`opencode`/`codex` 固定 `source_id`，保留 `device` 与自定义源；解释器 `-B` + `PYTHONDONTWRITEBYTECODE=1`、`PYTHONUTF8=1`、`HF_HUB_OFFLINE=1`；既有 `interpreter.kb.command` 存在时保留，只有显式 `-PythonExe` 才覆盖并报告变更；`db`/`storage` 只写源根内。
- `opencode.json(c)`：只合并 `mcp.servers.agent-memory`，保留 provider/model/agents/其他 MCP。自检按 `type`/`command`/`disabled` 必需键子集匹配，`environment`/`timeout` 等额外键原样保留。语义未变时不改写（注释保留）；必须改写时先备份并明确报告 JSONC 注释未保留；`opencode.json` 与 `opencode.jsonc` 并存时拒绝猜测。
- 提示词：cc-switch 存在时只改每个宿主唯一启用的来源，来源与导出必须逐字一致（仅允许换行差异），改库前做 SQLite 一致性备份；零条跳过、多条或发散立即停止。无 cc-switch 时只向已存在的全局提示词追加，不创建文件；已有未标记的 `memory_search`+`memory_read`+`session_id` 等价规则不重复追加并报告。
- Skip 语义：`-SkipOpenCode`/`-SkipCodex` 只跳过本次安装与校验，`recall.json` 中遗留的启用源保留并报 WARN「保留但未校验」。
- 只读边界：安装与 CheckOnly 不打开 OpenCode/Codex 会话数据；Smoke 仅在显式请求时运行一次 `memory_search`，输出只含 `source_status`、结果数、错误码与警告数，不含正文；无 Archive 采集。`-CheckOnly -Smoke` 以干净错误行 exit 1；PS 调 Python 用 `cmd /c "… 2>&1"` 合并 stderr。

## 2. 测试与验证

| 验证 | 结果 |
|---|---|
| `python assets/scripts/test_recall_onboarding.py` | 20/20 通过 |
| 覆盖：新装、重复安装幂等（无新备份）、已有自定义源、其他 MCP 保留、MCP 额外键保留并收敛、JSONC 注释备份与告知、json/jsonc 并存拒绝、cc-switch 来源/导出同步、无 cc-switch 仅追加既有文件、等价规则去重、来源发散拒绝且零写入、多启用来源拒绝、缺根拒绝、Skip 遗留源 WARN、解释器保留/显式覆盖、CheckOnly 零会话读取（损坏 DB + mock 连接）、Smoke 仅显式且不泄露正文、CRLF 保持、KB-only | 通过 |
| PowerShell 5.1 与 PowerShell 7 实跑（合成 HOME） | install / `-CheckOnly` exit 0；`-CheckOnly -Smoke` exit 1 且 stderr 只有脚本提示、无 `CategoryInfo`/`FullyQualifiedErrorId`；MCP 额外键、解释器保留/显式覆盖、Skip WARN 均实跑核对 |
| `Parser::ParseFile`（PS 5.1 + PS 7） | 64 与 00-precheck 均无语法错误；64/00-precheck 保持 UTF-8 BOM |
| `check-docs.ps1`（完整） | 6 项全部 PASS；规划、死链、STRUCTURE 覆盖与 references 分层通过 |
| `git diff --check` | 无空白错误 |

说明：本工作区缺少套件二进制分发物（`assets/packages/*`、`assets/dist/agent-memory/agent-memory.exe` 等，均不入 Git），因此 `00-precheck -FilesOnly` 会按既有设计报告这些文件缺失；新增的 64、Python 辅助与测试文件不在缺失列表中。真实 `agent-memory.exe` 的 Smoke 未在本工作区执行，Smoke 逻辑以模拟返回值验证。

## 3. 限制

- CheckOnly 证明配置、MCP 注册与提示词来源/导出一致，不证明知识库检索质量或真实运行时可检索；需按目标库制度另行验收。
- 知识库缺 `.meta/scripts/ask.py` 时 64 报 WARN 并完成安装，KB 源在运行时会降级；不自动初始化知识库。
- JSONC 注释不是可往返保留的内容：仅在必须合并 MCP 时重写，原文件有备份并在输出中明确提示；配置键与语义保持不变。
- cc-switch 的 `prompts` 表按 `app_type` / `enabled` / `content` 使用，其余表只读不写；数据库结构不兼容时停止，不做迁移猜测。
- 提示词来源更新与导出文件写入不是跨介质事务，cc-switch 多个宿主也不是跨行事务；中途失败须按备份恢复后重跑。
- `64` 是 Windows 脚本；macOS 最小集不包含 agent-memory 与统一回忆接入，不能据此宣称完整环境。
- 未重建 `assets/dist/agent-memory/`，未修改构建谱系 manifest。
- 本记录已按 `RECALL-ONBOARDING-FIXED.md` 的 B1/B2/F3/F5–F9 修复更新；旧版 13 项测试与 MCP 整对象比较不再代表当前实现。
- 计划文件在复审通过前保持 `in_progress`，不提前归档。

## 4. 结论

新增能力可由其他 Agent 按 `INSTALL.md` 的 61 → 64 → 只读验收路径落地；旧环境可按 `UPGRADE.md` 选择“只新增统一回忆能力”或“全量迁移”，两者都不重跑 bootstrap、不删除旧数据、不覆盖自定义配置。
