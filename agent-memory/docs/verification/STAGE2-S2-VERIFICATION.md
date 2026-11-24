# STAGE2-S2 独立验收报告

- 被验：`C:\wt\s2-verify`（detached 26c9f1b，父 71c9a1d）；验收者为独立 Verifier，只读，仅用合成 fixture 与临时目录，未读真实会话/数据库/配置，未注册任务，未运行真实 61/64/90。
- **verdict：通过**
- **blocking：无**

## 已执行命令与结果

| 命令 | 结果 |
|---|---|
| `cargo test --locked --no-fail-fast`（agent-memory） | 退出 0；汇总 628 passed / 0 failed / 2 ignored |
| `python -I -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py` | 71 passed |
| `git diff 71c9a1d 26c9f1b -- agent-memory/src/capture_sqlite.rs` | 空（未修改） |
| 新增行扫描 `chenr` / `C:\Users` | 无命中 |
| PS Parser 解析 64 / 90 | 各 0 错误 |
| 验收后 `git status` | 干净 |

## 端到端（自建合成 fixture：Claude 含 subagents、Codex rollout、OpenCode legacy storage；路径含中文与空格；archive 根已 `init`）

- 首次全采：7 文件全部 captured，`ledger=missing`，exit 0。
- 二次：7 个全部 `skipped_unchanged`，`ledger=ok`，records_added=0。
- 追加一行到一个文件：只采该文件（captured=1，其余 skipped）。
- 删除一个已采源文件：下次运行台账条目被清理；`agent-memory read --sha256 <对象> --domain default` 仍能回读完整正文，Archive 保留生效。
- `--plan` 前后 `arch` 根逐文件 sha256 汇总一致，未写任何文件。
- 报告 JSON、台账文件、包装脚本日志 `index-last.json` 中哨兵字符串出现次数均为 0（报告仅含路径、计数、错误码）。
- 锁占用（放置 `locks/capture.lock`）：`--skip-if-locked` 退出 0 且 `skipped=locked`；不带该参数退出 4；`--plan` 不受影响。
- 双进程并发（`--skip-if-locked`，同时启动 6 轮及错开启动 5 轮）：每轮一个完成全采（exit 0），另一个 `skipped=locked`，无 lock_held 错误。
- 台账写坏（截断 JSON、空文件、非 UTF-8 字节）：均不 panic，`ledger=corrupt`，全量重扫并重建，下一轮恢复 `ledger=ok`。
- 退出码：无 archive 源且无 `--root` → 2；`--root ""` → 2；`--source` 未知 → 2；`--session-since` 非法 → 2；缺 config 文件 → 2。
- `--session-since 2099-...`：全部文件 `skipped_since`，exit 0。
- 符号链接与联接点：在 Claude 根内创建目录联接点（junction）、目录符号链接、文件符号链接，指向根外含 jsonl 的目录。`--plan` 与真实运行均未枚举到根外文件（file_total 不变，台账无根外路径）。
- 约 120 字符 x 2 级深目录下的 jsonl：被正常采集，无报错。
- 同大小、同 mtime 的原地改写：确认被跳过（规格声明的限制）。
- 64 包装脚本（用假执行器调用 `ensure_index_schedule` 于含中文/空格的临时 UserRoot，只检查生成内容）：
  - 有/无 capture 的差异仅为：标题行、`capture-all --config $cfg --skip-if-locked` 阶段、各 payload 末尾 `capture = $capture`。
  - capture 段为字段白名单（ok、skipped、exit_code、ledger、finished_at、逐源计数、errors 仅含 code 字符串、固定文案 error）。
  - 带 UTF-8 BOM，vbs 为 UTF-16 并对路径加双引号。
  - 在合成目录实际运行该包装脚本：capture 先于 index 执行，capture 段正确写入日志，且不含哨兵（index 因 fixture 无 cache root 失败，属预期）。

## non_blocking

1. **`agent-memory/README.md`、`assets/references/agent-memory-archive.md` 未写明「size+mtime 同值的原地改写会被跳过」的限制**；仅在 `agent-memory/docs/verification/STAGE2-S2-COMPLETED.md` §5 有声明。已实测该限制存在。建议在 README Skip ledger 段加一句（同时注明时钟回拨同理）。
2. **锁语义为启动时 check-then-act**（`capture_all.rs` 中 `archive.lock_held()` 为 `locks/capture.lock` 存在性检查，每文件另持既有锁）。两进程在检查间隙同时越过检查时，理论上后者的文件会记 `lock_held` 并以退出 7 结束。本次 11 轮并发均未复现；README 已如实说明。另：该锁是文件存在性检查，崩溃遗留的陈旧 `capture.lock` 会使 `--skip-if-locked` 永久静默跳过（预存语义，非本次引入，建议文档提示排错）。
3. **报告字段小瑕疵**：`--plan` 与 `skipped=locked` 报告里 `ledger` 固定显示 `missing`，不反映实际台账状态，可能误导；建议 skipped/plan 时输出 null 或 `not_read`。
4. `--plan` 对已有台账不做 skipped_unchanged 预估（只报 `files_total`/`bytes_total`），与「计划」预期略有出入，但与文档声明一致。
5. 规格要求核对的「日志加 `capture` 段」在早退分支（already_running）中 `capture` 为 null；90 的审计对此按「无时间戳」处理，行为合理。

## 代码审查结论

- 退出码与 `bin/agent-memory.rs` 头注释表一致（0/2/3/4/7）；`run_source` 失败逐文件隔离，失败文件不写台账，下次重试。
- 报告、台账、日志路径：只含路径、计数、错误码；`capture_source` 的错误以 `error_code` 映射成固定码，未带错误文本；无会话正文进入这些输出。
- 枚举只用配置的显式根；`walk_dir` 对 `is_symlink`（Windows 上含 junction）一律跳过，已实测。
- `capture_sqlite.rs` 零改动；SQLite 每次都调用、`--session-since` 报 `unsupported_option`，与规格一致。
- 文档与实现一致（除上述 non_blocking 1）。
