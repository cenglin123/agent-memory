# STAGE2-S2 完成记录：多源采集驱动 capture-all 与定时采集接入

- 状态：**完成**（2026-10-30，工作区 `C:\wt\stage2-s2`，基于 HEAD=71c9a1d；未 git commit，由 orchestrator 提交）
- **真实数据 = 否**：全部测试与端到端验证使用合成 fixture、临时目录与注入的假计划任务执行器；未读取任何真实会话/数据库/生产 Archive/真实配置（`<user-home>/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory`、`~/.config/opencode` 全程未读）；未注册真实计划任务；未运行真实 61/64/90；未重建 `assets/dist`；未修改 `src/capture_sqlite.rs`。

## 1. 实现

| 文件 | 变更 |
|---|---|
| `agent-memory/src/capture_all.rs` | 新增：`capture-all` 驱动（枚举、台账、报告、退出码），约 780 行含文档注释 |
| `agent-memory/src/lib.rs` | 注册 `pub mod capture_all` |
| `agent-memory/src/bin/agent-memory.rs` | 接线 `capture-all` 子命令；usage 文本与文件头退出码说明更新 |
| `agent-memory/tests/capture_all.rs` | 新增：11 条 CLI 级验收用例（多源合成 fixture：Claude 含 subagents/tool-results、Codex `.jsonl`+`.zst`+archived_sessions、OpenCode V1 SQLite + legacy storage） |
| `assets/scripts/recall_onboarding.py` | `_launcher_ps1(capture)` 参数化包装脚本；`ensure_index_schedule(..., capture)`；`install(..., capture_schedule)`；`capture_readiness_report` + `_last_capture_summary`；`check_report` 追加 `archive-capture` 项；`main` 增加 `--capture-schedule` |
| `assets/scripts/test_recall_onboarding.py` | 新增 3 条用例（31/32/33） |
| `assets/scripts/64-install-recall.ps1` | 新增 `-CaptureSchedule` 开关与互斥校验，透传 `--capture-schedule` |
| `assets/scripts/90-audit.ps1` | 统一回忆段新增定时采集检查（只读元数据，WARN 级） |
| `agent-memory/README.md` | 新增「Multi-source capture driver」一节；CLI 块与运行时根布局更新 |
| `assets/references/agent-memory-archive.md` | 新增 4.6「生产采集：开启方式与排错」；运行时根布局补 `state/capture-all.json` |
| `INSTALL.md` / `UPGRADE.md` | 生产采集开启步骤（强调真实采集须用户授权）；5.7 行与第 7 节更新 |
| `CHANGELOG.md` | 顶部新条目 |

## 2. 设计决定

1. **锁 = check-then-act + 逐文件既有锁**：驱动启动时检查 `locks/capture.lock`（同 `scrub` 的模式），`--skip-if-locked` 被占用 → 退出 0 报 `skipped=locked`，否则退出 4；每个文件经公开入口 `capture_source` 自持锁采集。不在驱动层长持锁的原因：`capture_source`（含 `capture_jsonl`/`capture_sqlite` 内部分支）自带加锁，驱动持锁会自锁；逐文件锁保持「同一时刻运行时根内一个写者」的既有契约，运行中出现的竞争写者只把冲突文件记 `errors[].code=lock_held` 并使整体退出 7，其余文件不受影响。
2. **台账只记成功**：`state/capture-all.json`（v1）逐源逐文件记 `size`/`mtime_ms`/`capture_id`/`captured_at_ms`；失败的文件不写条目（下次必然重试），消失的文件被清理。损坏（不可读/解析失败/版本≠1）→ `ledger=corrupt`，本次全量重扫并重建——重扫永远正确，跳过才需要证明。台账是驱动簿记，不是证据。
3. **SQLite 源每次观测**：台账对 `opencode.db` 只记录不跳过（成本由 S1 水位决定）；`--session-since` 对 SQLite 不做 mtime 过滤（源间透传），逐源报 `unsupported_option=["session_since"]`，不算错误。
4. **枚举规则与 recall 侧对齐**：Codex 只取 `rollout-*` 前缀并复用 `codex_rollout_file::dedupe_siblings`（普通与 `.zst` 并存只采普通形态，Codex 正在写它）；OpenCode legacy 只扫 `storage/{session,message,part}` 固定布局（与 `json-legacy-v1` reader 一致，storage 下其余内容不采）；`db`/`storage` 显式键优先于约定路径。符号链接一律不跟随；枚举根缺失 → 逐源 `errors[].code=source_missing`。
5. **报告字段 snake_case**：与 `index` 命令报告风格一致（serde 默认字段名），顶层与逐源字段见 README/CHANGELOG；`files_captured` 统计「调用 capture 且成功」的文件数（含 archive 端确认 `skipped_unchanged` 的情形）。
6. **`--plan` 零写入**：不打开 Archive、不读台账、不写台账（测试用运行时根前后树哈希相等 + 裸根保持为空证明）；未初始化的运行时根也可 `--plan`。
7. **`-CaptureSchedule` 的 FAIL 语义**：缺启用的 archive 源时 `capture-schedule` 项 FAIL（提示先 `-ArchiveRoot`），任务仍按仅索引注册——`-IndexSchedule` 的既有语义不被吞掉；`-CaptureSchedule` 单独给出时隐含注册任务。
8. **包装脚本 capture 段逐字段白名单**：只挑 `ok/skipped/exit_code/ledger/finished_at` 与逐源 `source_id/files_*/records_added/bytes_added/duration_ms/errors`（errors 只保留 `code` 字符串、不含 path），解析报告从不整体内嵌——「日志只含元数据」由生成器结构保证并有静态断言（用例 32）。
9. **CheckOnly 可写性用 `os.access` 启发式**：CheckOnly 承诺不写任何文件，故不落探测文件（Windows 上 `os.access(W_OK)` 是弱检查，见限制）。

## 3. 测试

Rust `cargo test --locked --no-fail-fast`：**628 passed / 2 ignored / 0 failed**（基线 617/2 + 新增 11）。

`tests/capture_all.rs` 用例表（全部 CLI 级、合成 fixture）：

| # | 用例 | 覆盖 |
|---|---|---|
| 1 | `first_run_captures_every_enumerated_file` | 三源 10 文件全采；`notes.txt`/非 `rollout-` 前缀被忽略；台账生成；哨兵正文不在报告 |
| 2 | `second_run_skips_unchanged_files_but_still_observes_sqlite` | JSONL/legacy 全跳过；SQLite 每次观测；零新 manifest；`ledger=ok` |
| 3 | `only_the_changed_file_is_recaptured` | 追加 1 行 → 只采该文件，`records_added=1` |
| 4 | `new_files_are_picked_up` | 新增会话文件被枚举并采集 |
| 5 | `corrupt_ledger_forces_full_rescan` | 台账损坏 → `ledger=corrupt`、不跳过、重建后可解析 |
| 6 | `single_file_failure_is_isolated` | 坏 `.zst` → 该源 `errors[0].code=io`、整体 exit 7；其他源/文件不受影响；修复后重试干净 |
| 7 | `held_lock_is_reported_and_skip_if_locked_exits_zero` | 锁占用：默认 exit 4；`--skip-if-locked` exit 0 且 `skipped=locked` |
| 8 | `plan_writes_nothing_and_reports_planned_bytes` | `--plan` 运行时根树哈希不变；裸根保持空（不初始化） |
| 9 | `session_since_filters_old_files_and_sqlite_reports_unsupported` | 旧 mtime 文件被计 `files_skipped_since`；SQLite 报 `unsupported_option` 不报错；坏时间戳 exit 2 |
| 10 | `no_archive_source_and_bad_source_selection_exit_two` | 无 archive 源且无 `--root` → exit 2 带提示；`--source` 未知/非采集源/禁用 → exit 2；合法 `--source` 单源报告；`--root` 优先于 archive 源 |
| 11 | `deleted_files_are_pruned_from_the_ledger` | 删除源文件后台账条目被清理 |

Python `pytest test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`：**71 passed**（onboarding 35 → 38，新增 31/32/33）：

| # | 用例 | 覆盖 |
|---|---|---|
| 31 | `test_capture_schedule_wrapper_and_archive_precondition` | 无 archive 源 → `capture-schedule` FAIL 带提示、任务仍注册且包装脚本无 capture-all；有 archive 源 → PASS、`capture-all` 先于 `index`；`ensure_index_schedule(capture=False/True)` 内容差异 |
| 32 | `test_launcher_capture_section_is_metadata_only` | capture 段只选白名单键；errors 只留 code；报告从不整体内嵌 |
| 33 | `test_check_reports_capture_readiness` | 未配置 SKIP；配置后 PASS（writable、lastCapture=None）；根缺失 WARN；`index-last.json` capture 段摘要只含元数据键（哨兵正文不在 detail）；回落台账摘要 |

PowerShell：`64-install-recall.ps1`、`90-audit.ps1` 及生成的两种包装脚本（仅索引 / capture 模式）经 PS 5.1 与 PS 7 `Parser::ParseFile/ParseInput` 均 **0 错误**。capture 模式包装脚本在合成 fixture（临时目录 + 工作区 debug `agent-memory.exe`）上端到端验证两轮：首轮 `capture.ok=true`、`files_captured=1`；次轮 `files_skipped_unchanged=1`、`ledger=ok`；`index-last.json` 顶层键含 `capture`。

文档治理：`assets/scripts/check-docs.ps1` 通过（6 项、72 个 Markdown）；`95-test-docs-governance.ps1` 通过（67 用例）。`git diff --check` 干净；新增 Rust 文件 UTF-8 无 BOM、LF。

## 4. 命令与退出码

```
agent-memory capture-all --config <recall.json> [--root <runtime-root>] [--domain <d>]
                          [--source <id>] [--session-since <rfc3339>] [--plan] [--skip-if-locked]
```

| 退出码 | 条件 |
|---|---|
| 0 | 全部成功；或 `--skip-if-locked` 且锁被占用（`skipped=locked`） |
| 2 | 用法/配置：缺 `--config`、配置不可读、无 archive 源且无 `--root`（提示配置 archive 源）、`--source` 未知/禁用/非采集源、`--session-since` 非 RFC3339 |
| 3 | io/state：运行时根未初始化、台账不可写 |
| 4 | 锁被占用且未给 `--skip-if-locked` |
| 7 | 任一源有错误（报告仍在 stdout；单文件错误隔离在 `sources[].errors[{path, code}]`） |

错误码取值：`lock_held`/`io`/`access_denied`/`not_found`/`integrity`/`corrupt`/`limit_exceeded`/`embedding`/`not_initialized`（ArchiveError 映射）、`source_missing`/`enum_io`/`stat_io`（枚举/统计失败），以及 capture 引擎返回的 `status`（如 `failed`/`degraded`）。

## 5. 限制

- 跳过判定是 size+mtime：同大小原地改写且 mtime 不动的文件会被跳过（规格如此；Archive 端 fingerprint 校验作为被调用时的兜底）。时钟回拨到「上次成功采集之前」同样按规格跳过。
- 一次性 `capture-all` 不做并发限流：文件逐个串行采集。台账让稳态成本与增量成正比，但首次全量（本机约 900 文件 / 1.5 GB+）耗时会由 S3/S4 实测记录。
- `-CaptureSchedule` 的包装脚本是注册时生成的静态文本：改间隔用 `-IndexIntervalMinutes` 重跑即可，但「从仅索引升级为先采集后索引」需要重跑带 `-CaptureSchedule` 的 64（同一任务名原地更新，UPGRADE.md 第 7 节已写）。
- 90-audit 对 `capture.finished_at` 用 InvariantCulture 解析包装脚本写入的 RFC3339 字符串；对台账用 `DateTimeOffset::FromUnixTimeMilliseconds`。解析失败按「无时间戳」路径 WARN，不 FAIL。

## 6. 未做（按边界与分工）

- 未实现 `--session-since` 的 SQLite 透传（S1；现为 `unsupported_option` 标注）。
- 未改 `src/capture_sqlite.rs`（S1 并行重写中）；驱动只经 `capture_source` 公开入口调用，S1 签名增参由 orchestrator 衔接（驱动侧无需变更）。
- 未重建 `assets/dist`、未刷新分发物谱系（维护机职责）；本次不改二进制分发物，61/62/63 安装与验收脚本未动。
- 未注册真实计划任务、未运行真实 61/64/90；未采集任何真实数据（S3/S4 需用户授权）。
- 未 git commit（orchestrator 执行）；工作区为 `C:\wt\stage2-s2`（HEAD=71c9a1d）。

## 7. 与 S1 的衔接点

1. `capture_source` 公开签名不变即可工作；S1 若给 SQLite 路径增加增量参数（`--session-since`/`--rebaseline`/`--verify`），由 orchestrator 在 `capture.rs`→`capture_sqlite.rs` 边界衔接，`capture_all.rs` 不需要改（它只调用 `capture_source`）。
2. `session_since` 透传：`run_source` 已按「SQLite 例外」组织（mtime 过滤与台账跳过都不作用于 db 文件），S1 增参后把 `since_ms` 传入即可，报告中 `unsupported_option` 随之消失。
3. 台账对 SQLite 只记 `size/mtime/capture_id/captured_at_ms` 事实，不参与跳过——S1 的水位与 `--rebaseline` 语义不受台账影响。
4. `capture_sqlite::is_sqlite_file/looks_like_sqlite_path` 被 `capture_all.rs` 复用做「db 文件」识别；这两个 pub fn 若在 S1 中更名/移动，需同步 `capture_all.rs` 一处调用。

## 8. 观察与不确定项

- [UNCERTAIN] CheckOnly 的「可写」判定用 `os.access(root, W_OK)`：POSIX 语义准确，Windows 只能排除只读属性目录，属弱启发式（代价是 CheckOnly 零写入承诺——不落探测文件）。
- [UNCERTAIN] 环境观察（与交付物无关，如实记录）：在本代理会话的部分临时脚本运行期间，控制台出现过本机真实知识库/会话图摘要（`019e…` 会话 UUID、计划文档名、`total groups 29`）。逐项隔离验证（python 裸脚本、仅 `import recall_onboarding`、debug exe `init`、包装脚本各自单独运行均各自干净）确认该输出不是 S2 任何代码路径产生；S2 全部测试与端到端均在合成 fixture 上完成，报告不含正文的哨兵断言全部通过。疑似本机既有自动化（OpenCode 侧外壳/钩子）对本代理 shell 输出管道的行为，未深究，不影响本工作区文件与测试结论。
