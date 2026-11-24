# 全文检索优先 L6：安装接入与周期刷新 — 完成记录

- **状态**：完成（代码、测试、文档均落在此工作区；未提交、未重建分发物、未做生产安装）
- **真实数据 = 否**：全部用例使用临时目录中的合成 fixture 与 `backend=fake`（Python 计划任务用假执行器）；未读取任何真实会话、真实知识库内容或生产 Archive，未读取生产 `recall.json`，未注册真实计划任务。
- **工作区**：`C:\wt\hybrid-retrieval-20261028`（起点 HEAD `d66ddfc`）
- **执行日期**：2026-10-30
- **承担范围**：L6（64 安装接入、90 体检、62 离线验收用例、文档收口）。

## 1. 实现

### 1.1 修改/新增文件

| 文件 | 内容 |
|---|---|
| `assets/scripts/64-install-recall.ps1` | 新增 `-EmbeddingModel`/`-EmbeddingDimension`/`-EmbeddingBackend`/`-SkipEmbedding`、`-BuildIndex`、`-IndexSchedule`/`-RemoveIndexSchedule`/`-IndexIntervalMinutes`；`-VaultPath` 仅在 `-RemoveIndexSchedule` 时可省略；显式嵌入参数与 `-SkipEmbedding` 互斥；参数只在 `$PSBoundParameters` 含该键时传给 Python（既有块不被默认值覆盖）；`-CheckOnly` 同时报告嵌入/Ollama/计划任务 |
| `assets/scripts/recall_onboarding.py` | `build_config` 写入/保留/显式覆盖 `interpreter.embedding`（默认 `qwen3-embedding:4b`/512/l2/ollama）；`cache_root` 缺省写入 `<UserRoot>/.agent-memory/cache` 并保留既有值；新增 `index_build_report`（只汇总 status/rows/vectors/duration）、`WindowsTaskExecutor`（`schtasks` 注册/删除 + PowerShell 查询）、`ensure_index_schedule`（写隐藏启动器 + 注册）、`remove_index_schedule`、`schedule_report`、`probe_ollama`（仅 `GET 127.0.0.1:11434/api/tags`）、`embedding_report`；新增 `schedule`/`unschedule` 动作 |
| `assets/scripts/test_recall_onboarding.py` | 新增 `FakeTaskExecutor`、离线 Ollama 桩；7 条新用例（默认/保留/显式覆盖、`-SkipEmbedding`、Ollama 报告、`-BuildIndex` 摘要、计划任务注册参数/幂等/删除/CheckOnly、互斥校验） |
| `assets/scripts/90-audit.ps1` | 最小方式恢复通用 `Warn` 记录与汇总（WARN 不改变退出码）；对 `recall.json` 的每个启用活源只读检查 `fts.sqlite`/`vectors.bin`；检查 `agent-memory-index` 计划任务 |
| `assets/scripts/62-verify-agent-memory-offline.ps1` | 新增 5 条离线用例：合成 Codex JSONL `index`、缺省 `resolved_mode=hybrid`、`exact` 命中、带 `since` 的缺省模式、删除缓存后回落 `legacy`；新增合成 recall 配置辅助函数 |
| `README.md`、`INSTALL.md`、`UPGRADE.md` | 统一回忆安装步骤加入 `-BuildIndex`/`-IndexSchedule`、Ollama 前置（`ollama pull qwen3-embedding:4b`）与离线 `-SkipEmbedding`；路径 A 说明首次索引、计划任务与 `projector_version_changed` 自动整源重建 |
| `assets/references/agent-memory-archive.md` | 2.2 增补 64 的新参数与只读边界；新增 4.5：用法指引（exact/缺省/两路都调）、刷新机制与 `stale_since` 读法、排错表；6/7 节补 WARN 审计项与排错行 |
| `agent-memory/README.md` | 新增 **Index refresh**：首次索引、计划任务（隐藏窗口/单实例/日志）、`-CheckOnly` 回环探测、嵌入块写入/保留规则 |
| `CHANGELOG.md`、`CURRENT.md`、`agent-memory/docs/verification/README.md` | L6 变更记录、任务状态与最近完工摘要、验收索引更新 |

### 1.2 关键行为

- **嵌入配置**：`recall.json` 无 `interpreter.embedding` → 写默认块（`normalization=l2`）；已有块 → 原样保留；只有显式 `-EmbeddingModel`/`-EmbeddingDimension`/`-EmbeddingBackend` 才覆盖，并在 `recall-config.detail` 逐项报告 `旧值 → 新值`。`-SkipEmbedding` 不写、也不会删除已有块；与显式参数同用直接 exit 1。
- **`cache_root`**：未配置时写 `<UserRoot>/.agent-memory/cache`；已有自定义值保留（v1 不写，需 `-MigrateV1`）。
- **`-BuildIndex`**：写完配置后运行一次 `agent-memory index --config <recall.json>`；`index-build` 只含 `exitCode`/`ok`/`total_rows`/每源 `source_id`/`status`/`rows`/`vectors`/`duration_ms`，不含正文；非 0 退出或 `ok=false` → FAIL。
- **`-CheckOnly`**：`recall-embedding` 对 `backend=ollama` 只发一次回环 `GET /api/tags`，报告可达与模型是否已拉取；`backend=python` 检查 `model_path`；无嵌入块为 SKIP。`index-schedule` 报告任务存在性、下次运行、上次结果与 `index-last.json` 摘要；未注册为 WARN（可选能力）。
- **90**：`recall.json` 缺失也记 WARN；每个启用的活源（opencode/codex/claude_code/device），缺 `fts.sqlite` → WARN「未建全文索引（运行 64 -BuildIndex 或 agent-memory index）」；配置了 embedding 但缺 `vectors.bin` → WARN；`agent-memory-index` 不存在 → WARN。只读文件元数据，不打开会话、不解析正文。
- **62**：索引建在临时 `cache_root`；`memory_search` 分别验证缺省 hybrid、exact 命中、带 `since` 不报错、删缓存后 legacy 回退；结束由既有 `finally` 清理临时目录。

## 2. 设计决定

1. **计划任务无窗口方案**：任务动作是 `wscript.exe //B //NoLogo <UserRoot>/.agent-memory/scripts/index-refresh.vbs`。VBS 用 `WScript.Shell.Run(cmd, 0, False)` 以窗口状态 0 启动 `powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File index-refresh.ps1`，两层都声明隐藏，避免控制台闪现；`wscript.exe` 属 GUI 子系统，不创建控制台。相比直接让任务运行 `powershell.exe -WindowStyle Hidden`，多一层 VBS 可避免部分环境下 PowerShell 先分配控制台再隐藏的闪烁。
2. **单实例**：任务 XML 写 `MultipleInstancesPolicy=IgnoreNew`（上一次未结束则本次不启动，不排队）；包装器另持 `Local\agent-memory-index` 命名互斥量，手工运行与计划任务重叠时也只会有一个写者，被跳过的一次在日志写 `skipped=already_running`。没有引入跨进程锁文件，避免残留锁导致永久停摆。
3. **计划任务适配器可注入**：Python 侧 `WindowsTaskExecutor` 封装 `schtasks /Create /XML /F`、`schtasks /Delete /F` 与 PowerShell `Get-ScheduledTask*` 查询；`ensure_index_schedule`/`schedule_report`/`remove_index_schedule` 接受 `executor`。测试只传 `FakeTaskExecutor`，断言注册名、间隔、动作、幂等、删除与报告，绝不触碰真实任务。
4. **隐藏启动器落盘位置**：`<UserRoot>/.agent-memory/scripts/index-refresh.ps1`（UTF-8 BOM，PS 5.1 读中文路径安全）与 `index-refresh.vbs`（UTF-16 LE BOM，`wscript` 不受 ANSI 代码页影响）；属于程序目录，卸载 61 时一并移除，不写系统目录。
5. **日志只含报告**：包装器解析 `index` 的 stdout，只落 `ok`/`exit_code`/`total_rows`/每源 `source_id`/`status`/`rows`/`vectors`/`duration_ms`；不落 `path`、`snippet`、`sources[].detail` 或 stderr 正文。覆盖写，避免日志无限增长。
6. **注册幂等**：`schtasks /Create /F` 对同名任务覆盖更新，重复 `-IndexSchedule` 不产生第二个任务；`-RemoveIndexSchedule` 对不存在任务返回 SKIP（退出码 0），便于脚本重复执行。
7. **嵌入体检只走回环**：URL 固定 `http://127.0.0.1:11434/api/tags`，1.5 s 超时；绝不调用 pull、绝不访问外网。模型名按 `:` 前的基础名比较，兼容 `qwen3-embedding:4b` 与 `:latest` 标注。
8. **WARN 与 FAIL 分离**：全文索引、向量缓存与周期刷新都是可选增强；90 只在 FAIL 时退出 1，WARN 仅汇总提示，避免未启用语义召回的机器被审计判失败。

## 3. 测试用例表

### 3.1 Python（`assets/scripts`，`python -m pytest -q`）

| 文件 | 数量 | 覆盖 |
|---|---|---|
| `test_recall_onboarding.py` | 33 | 新增 7：`test_embedding_default_written_preserved_and_overridden`、`test_existing_embedding_is_preserved_without_explicit_parameters`、`test_skip_embedding_does_not_write_but_keeps_cache_root`、`test_check_reports_ollama_reachability`、`test_build_index_reports_sources_without_bodies`、`test_index_schedule_registration_idempotency_and_removal`、`test_skip_embedding_rejects_explicit_embedding_options` |
| `test_recall_embedding.py` | 21 | 既有 |
| `test_recall_eval.py` | 12 | 既有 |
| **合计** | **66 passed** | 既有隔离约束（临时 HOME、不碰真实配置/会话）保持 |

计划任务用例断言：名称 `agent-memory-index`、间隔 15/45 更新且任务数恒为 1、`command=wscript.exe`、参数 `//B //NoLogo <vbs>`、PS1/VBS 的 BOM 与隐藏参数、`CheckOnly` 的 PASS（下次运行/上次结果/日志摘要）与删除后的 WARN/SKIP。

### 3.2 离线验收（`62-verify-agent-memory-offline.ps1`）

| # | 用例 | 覆盖点 |
|---|---|---|
| 1 | `全文索引：分发物对合成 Codex JSONL 建索引` | `index` exit 0、`ok=true`、源状态 `built`、`rows≥1`、生成 `live/codex/fts.sqlite` |
| 2 | `全文索引：缺省模式解析为 hybrid` | 缺省请求 → `detail.resolved_mode=hybrid`、有命中 |
| 3 | `全文索引：exact 模式命中合成文本` | `mode:"exact"` → 命中、`resolved_mode=exact` |
| 4 | `全文索引：缺省模式带 since 不报错` | 缺省 + `since` → exit 0、`errors=[]`、仍 hybrid |
| 5 | `全文索引：删除缓存后缺省模式回落旧路径` | 删除 `cache_root` → exit 0、`errors=[]`、`legacy`、旧扫描仍命中 |

合计 37 项（原 32 + 新 5），PS 5.1 与 PS 7 各 37/37。

### 3.3 手工端到端（临时 HOME）

| 用例 | 结果 |
|---|---|
| `64` 安装（KB + OpenCode，默认嵌入） | exit 0；`recall-config` 写入默认嵌入块；`recall-embedding` PASS（本机 Ollama 可达）；`index-schedule` WARN（未注册） |
| `64 -CheckOnly` | exit 0；报告嵌入与计划任务 |
| `64 -RemoveIndexSchedule`（无 `-VaultPath`） | exit 0；任务不存在 → SKIP |
| `64 -SkipEmbedding -BuildIndex`（KB + 合成 Codex） | exit 0；`index-build` PASS，`codex built rows=1 vectors=skipped`；生成 `fts.sqlite` |
| `90 -AgentMemoryOnly`（同一临时 HOME） | 新项 PASS「全部活源已建立：codex」；`index-schedule` WARN；WARN 未改变退出码 |

## 4. 命令与退出码

| 命令 | 结果 |
|---|---|
| `python -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`（`assets/scripts`） | **66 passed**，exit 0 |
| `cargo test --locked --no-fail-fast`（`agent-memory`） | **617 passed / 2 ignored / 0 failed**，exit 0（基线不变；本轮未改 Rust） |
| `cargo build --release --locked` | exit 0 |
| `RUSTFLAGS=-C target-feature=+crt-static cargo build --release --locked --target x86_64-pc-windows-msvc` | exit 0（供 62 使用的静态 CRT 构建） |
| `62-verify-agent-memory-offline.ps1`（PS 5.1，临时套件根） | **37/37 PASS**，exit 0 |
| `62-verify-agent-memory-offline.ps1`（pwsh 7.6.9，临时套件根） | **37/37 PASS**，exit 0 |
| 修改的 `.ps1`（64/62/90）× PS 5.1 / PS 7 `Parser::ParseFile` | 6/6，0 错误 |
| `check-docs.ps1` | PASS，exit 0 |
| `95-test-docs-governance.ps1` | 67/67 用例通过，exit 0 |

> 62 的二进制来源：按任务要求先用 `cargo build --release --locked`，再用静态 CRT target 构建；因 `assets/dist` 现存清单的 `source.treeSha256` 早于 HEAD，**未改动 `assets/dist`**，而是把静态 CRT 二进制复制到临时套件副本并同步该副本清单的 `binarySha256`/`binaryBytes`/`source.treeSha256`。因此 62 验证的是本轮代码路径，不是现存分发物；生产前须由 orchestrator 重建并刷新谱系。

## 5. 限制

- **未注册真实计划任务**：所有计划任务用例使用 `FakeTaskExecutor`；`schtasks /Create /XML`、`wscript` 隐藏窗口、登录触发器与 `IgnoreNew` 的实际表现未在本机验证。**[UNCERTAIN]** 受限策略机器上向任务根目录注册可能需要额外权限；脚本按当前用户、`LeastPrivilege`、`InteractiveToken` 设计，不要求管理员。
- **未目视验证无窗口**：隐藏方案依据 `wscript.exe` 无控制台 + `Run(...,0,...)` + `-WindowStyle Hidden` 三重设置，未录制窗口闪现。**[UNCERTAIN]** 极慢机器上首次启动 `powershell.exe` 仍可能有瞬时闪烁。
- **Ollama 探测只验证本机当前状态**：本机 Ollama 正在运行且模型可达，PASS 路径得到验证；未测试模型缺失时的真实 WARN 路径（仅 Python 桩覆盖）。
- **`vectors.bin` WARN 的前提**：只有索引已建且配置了 embedding 时才检查向量文件；索引缺失时同时报全文索引 WARN。
- **嵌入块与 v1**：v1 配置按既有合同逐字保留，不写 `cache_root`/embedding；必须显式 `-MigrateV1` 后再建索引。
- **计划任务日志**：包装器只保留最近一次报告；历史趋势需另接 telemetry/日志轮转，不在本步范围。
- **62 的临时套件**：见 §4 说明，`assets/dist` 未重建；现存分发物与 HEAD 的树哈希差异需生产前处理。

## 6. 未做

- 未 `git commit`（由 orchestrator 执行）。
- 未重建 `assets/dist`、未跑真实的 61/64 安装、未跑真实 `90-audit` 全量。
- 未注册真实计划任务，未读取/写入真实 `recall.json`、真实会话、知识库或生产 Archive。
- 未复跑真实数据评估；L5b/orchestrator 的指标（exact recall@5 0.864、缺省 hybrid 0.682、两路合并 1.000、hybrid 负例空结果 1.000）仅作为文档结论引用。
- 未改 Rust 代码，未新增 MCP schema 字段。
