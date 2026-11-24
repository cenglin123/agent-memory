# 统一回忆接入验收修复记录

- 日期：2026-10-28
- 输入：`<user-home>\AppData\Local\Temp\opencode\recall-onboarding-verification.md`（fresh verification）
- 范围：`64-install-recall.ps1`、`recall_onboarding.py`、`test_recall_onboarding.py`、`UPGRADE.md`、`INSTALL.md`、`assets/references/{agent-memory-archive,conventions}.md`、完成记录
- 真实数据使用：**否**（合成 HOME、合成 cc-switch DB、损坏的假 OpenCode DB；未读取真实配置、会话或数据库，未联网，未执行 Archive 采集，未重建分发物或修改 manifest）

## 1. 修复项

### B1 — UPGRADE 路径 B 与保留承诺一致

`UPGRADE.md` §1/§4/§5 改写：

- 明确 `30-write-opencode-config.ps1` **只用于没有既有 `~/.config/opencode/` 配置的新环境**；它不合并既有配置、整体重写固定对象、**不是幂等步骤**。
- 路径 B 拆成 4.1 新环境与 4.2 旧环境：
  - 新环境保留 30，顺序为 `00 → 05 → 10 → 15 → 20 → 30 → 40 → 50 → 60 → 61 → 64 → 70 → 90`。
  - 旧环境**跳过 30**：先备份/保留旧配置，再跑 `00 → 05 → 10 → 15 → 20 → 40 → 50 → 60 → 61`，旧配置就位后运行 64 与 `64 -CheckOnly`，最后 70 → 90。
  - 误跑 30 时从 `opencode.jsonc.bak-*` 恢复或手工回填 provider/model/agents/其他 MCP，并**重跑 64 与 `-CheckOnly`**。
- 删除“每步幂等”的错误表述；`INSTALL.md` 步骤表与提示同步标注 30 仅限新环境、非幂等。

### B2 — MCP 必需键子集匹配

- 新增 `mcp_matches(actual, rule)`：只要求 `type`/`command`/`disabled` 与安装值一致；`environment`/`timeout` 等额外键保留且不参与失败判定。
- `check_report` 与安装后自检都使用该匹配；旧环境带额外键时安装只产生一条 `mcp-registration PASS`，后续 `-CheckOnly` 收敛。

### F3 — 保留既有 KB 解释器

- `build_config` 接收 `python_explicit`：既有 `interpreter.kb.command` 存在且文件存在时保留；只有显式 `-PythonExe` 才覆盖。
- PS 侧记录 `$pythonExplicit` 并传 `--python-exe-explicit`；Python 在 `recall-config` 详情中报告“保留既有解释器 …”或“解释器 X → Y（显式 PythonExe）”。
- 旧命令不可用时改用候选解释器并报告。

### F5 — CheckOnly+Smoke 干净退出

- 守卫改用 `[Console]::Error.WriteLine(...)` + `exit 1`，不再触发 `$ErrorActionPreference='Stop'` 的终止错误对象。

### F6 — Skip 遗留源显式 WARN

- `recall.json` 中仍启用的 `opencode`/`codex` 源在本次 `-Skip*` 时输出独立 `recall-sources` WARN：“保留但未校验：…（本次显式 Skip；源仍在 recall.json 中启用）”。
- 安装与 `-CheckOnly` 同口径；WARN 不改变退出码 0。

### F7 — 等价规则去重

- 无标记文本同时包含 `memory_search`、`memory_read`、`session_id` 时视为已有等价统一回忆路由：不追加、不改写，并在 `prompt-route` 详情报告。
- 有 `<!-- suite-recall:start -->` 标记时仍按标记更新；cc-switch 来源与无 cc-switch 全局提示词使用同一逻辑。`-CheckOnly` 接受带标记或等价规则。

### F8 — PS 5.1 stderr

- 64 调用 Python 改为 `cmd /c "… 2>&1"`（参数逐个加引号，拒绝含双引号的参数），稳定取得退出码与完整 stdout+stderr，避免 PS 5.1 把 Python 输出包装成 `NativeCommandError` 后提前终止。

### F9 — 测试补充

`test_recall_onboarding.py` 从 13 项增至 **20 项**，新增：

- MCP 额外键保留 + 安装/CheckOnly 收敛 + 只出现一条 `mcp-registration`；
- `opencode.json` 与 `opencode.jsonc` 并存拒绝且零写入；
- `-SkipOpenCode` 遗留源 WARN（安装与 CheckOnly）；
- 既有解释器保留 + 显式 `-PythonExe` 覆盖并报告；
- 无 cc-switch 等价规则不重复追加；
- cc-switch 等价规则不重复追加；
- CheckOnly 在 mock `sqlite3.connect` 下不连接任何会话存储。

原有 13 项全部保留并通过。

## 2. 验证证据

| 验证 | 结果 |
|---|---|
| `python assets/scripts/test_recall_onboarding.py` | `Ran 20 tests ... OK`，exit 0 |
| PS 5.1 合成 HOME：install / `-CheckOnly` | exit 0；`mcp-registration` 单条 PASS |
| PS 7 合成 HOME：`-CheckOnly` | exit 0 |
| PS 5.1 `-CheckOnly -Smoke`（分离 stdout/stderr） | exit 1；stderr 仅 `64-install-recall: CheckOnly 与 Smoke 不可同时使用…`，无 `CategoryInfo`/`FullyQualifiedErrorId` |
| MCP 额外键实跑（`environment={KEEP:1}`、`timeout=123`） | 安装 exit 0，额外键保留，`command[1] == mcp`；`-CheckOnly` exit 0 |
| 解释器保留实跑 | 无 `-PythonExe` 时 command 仍为旧路径并输出“保留既有解释器”；显式 `-PythonExe` 时输出“→ …（显式 PythonExe）” |
| Skip 遗留源实跑 | `-CheckOnly -SkipOpenCode` 输出 `recall-sources WARN 保留但未校验：opencode…`，exit 0 |
| `Parser::ParseFile`（PS 5.1 + PS 7） | 64 与 00-precheck 无语法错误；UTF-8 BOM 保持 |
| `check-docs.ps1` | 6 项全 PASS |
| `git diff --check` | 无空白错误 |

## 3. 限制

- 未运行真实 `agent-memory.exe` 的 `-Smoke`：本工作区无分发物二进制，且本机真实 `recall.json` 会触及真实会话；Smoke 解析逻辑用合成 payload 验证。
- 未使用真实 cc-switch 数据库或真实全局提示词；cc-switch 行为只在合成 SQLite 上验证。
- `-CheckOnly` 证明配置、MCP 注册与提示词一致性，不证明知识库检索质量。
- JSONC 注释在必须改写时仍不保留（有备份与明确告警）；配置键与语义保留。
- cc-switch 多宿主更新与导出文件写入不是跨行/跨介质事务，失败按备份恢复后重跑。
- 计划文件在复审通过前保持 `in_progress`，本记录不代替最终复审。
