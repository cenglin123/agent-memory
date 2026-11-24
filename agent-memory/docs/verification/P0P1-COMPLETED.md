# P0/P1 完工报告 — agent-memory 会话原始证据归档技术验证

完成时间：2026-10-25（本机；原写作 2026-10-26 为笔误，实现提交时间为 2026-10-25T02:38+08:00）。工作区：`C:\wt\agent-memory-p0p1-20261025`（套件独立 worktree，基线 HEAD 75e014d）。未执行 git commit（按任务要求）。

## 基线

- 上游：cc-switch，tag `v4.0.20`，commit `85ad4969b3399500baaea9d46b1e08f87aba80e1`。
- 选择理由：P0 启动时上游最新发布 tag；相对调查笔记的核验点（326a9ef8，v4.0.4 后特性分支）已包含 Codex 压缩 rollout 读取（8641cd1d）与 OpenCode SQLite V2 支持，覆盖首批三框架的现行默认写入格式。以 `git archive v4.0.20` 原始字节抽取，只读使用该仓。
- MIT 许可：`LICENSE` 逐字节复制，来源与本地修改见 `NOTICE`。

## 实现文件清单

| 路径 | 来源/职责 |
|---|---|
| `agent-memory/Cargo.toml` / `Cargo.lock` | crate 定义（edition 2021, rust-version 1.85），无 Tauri |
| `agent-memory/LICENSE` | cc-switch MIT 许可证（逐字节复制） |
| `agent-memory/NOTICE` | 基线 commit、移植清单、本地修改与剥离项 |
| `agent-memory/README.md` | 构建/测试、CLI 契约、支持矩阵、原子性契约、权限模型 |
| `agent-memory/src/lib.rs` | `SessionMeta`、三 provider 分发、`format_support_matrix()` |
| `agent-memory/src/model.rs` | 移植：统一 SessionMessage/SessionBlock/ContentRef/ImageRef + `project_content` + fixture 往返测试 |
| `agent-memory/src/providers/{utils,blocks,claude,codex,codex_items,opencode,opencode_blocks}.rs` | 移植：三 provider 扫描/解析/ContentRef 逻辑与全部自带测试 |
| `agent-memory/src/codex_rollout_file.rs` | 移植：Codex 未压缩/压缩 rollout 透明读写 |
| `agent-memory/src/codex_state_db.rs` | 移植：Codex state db 定位与 WSL 锁判定 |
| `agent-memory/src/config.rs` | 新写：headless 配置垫片（test home override、provider 根、atomic_write、不响应 OPENCODE_DB） |
| `agent-memory/src/error.rs` | 新写：替代 AppError 的最小错误类型 |
| `agent-memory/src/archive/mod.rs` | 新写：内容寻址对象库、采集清单、水位/记录索引、schema 版本、对账、单写者锁、fail-closed 回读 |
| `agent-memory/src/capture.rs` | 新写：单来源全量采集、指纹跳过、记录级去重、压缩 rollout 解码对象、解析探针、故障注入点 |
| `agent-memory/src/bin/agent-memory.rs` | 新写：CLI（init/capture/read/status/reconcile），JSON 输出 + 稳定退出码 |
| `agent-memory/tests/p0_formats.rs` | 新写：支持矩阵 + Codex 两种 rollout / Claude injected+compact 的真实解析验证（OpenCode 三代际与 Claude sidecar 的解析测试实际位于各 provider 单元测试，见该文件头注释的测试位置对照） |
| `agent-memory/tests/p1_archive.rs` | 新写：8 项 P1 验收测试（含 CLI 契约与故障注入） |
| `agent-memory/tests/fixtures/sessions/*.messages.json` | 复制：cc-switch 前后端共用契约 fixture（脱敏合成数据） |
| `agent-memory/.gitignore` | 新写：忽略 target/ 等生成物 |
| `agent-memory/docs/verification/P0P1-COMPLETED.md` | 新写：本完工报告 |
| `agent-memory/tests/boundary_no_default_roots.rs` | 后补（边界修复，见 P0P1-BOUNDARY-FIXED.md）：默认真实根通道的反向测试 |

## 实际复用的 cc-switch 模块（v4.0.20）

- `session_manager/model.rs` 全量（含 fixture 往返测试，`include_str!` 路径适配）。
- `session_manager/providers/`：`utils.rs`（LineSpans/JsonlSpan/指纹读头尾）、`blocks.rs`（预览阈值/标题/归一化）、`claude.rs`、`codex.rs`、`codex_items.rs`、`opencode.rs`、`opencode_blocks.rs` 全量移植（仅 import 适配 + 删除源会话函数及其测试剥离）。
- `codex_rollout_file.rs` / `codex_state_db.rs` 全量（config/error 指向本 crate 垫片）。
- `tests/fixtures/sessions/` 六个契约 fixture 原样。
- `SessionMeta` 定义自 `session_manager/mod.rs`。
- 剥离的桌面耦合（详见 NOTICE）：Tauri 命令/Channel/终端/揭示/导出/删除命令；`cache.rs`（交互式 TranscriptCache）；`content.rs`（源侧 validate/resolve/图片解码，归档回读改走对象引用）；agy/gemini/grokbuild/hermes/kimi/mcode/omp/openclaw/pi 九个第二批 provider；app_store/settings/database/proxy/MCP 等其余子系统。

## 直接依赖

serde、serde_json(preserve_order)、log、chrono、toml_edit、regex、zstd、rusqlite(bundled+backup)、sha2；dev：tempfile、zstd。`cargo tree`（125 行）无 tauri/wry/webkit2gtk/gtk。（边界修复后移除了未使用的 serial_test 与 dirs——dirs 正是被删掉的 home 兜底通道；`cargo tree` 现为 92 行。）

## 测试命令与退出码

- `cargo test` → **exit 0**：lib 108 passed（全部移植测试，含压缩 rollout、SQLite V1/V2、注入/compact、fixture 往返）；p0_formats 4 passed；p1_archive 8 passed；bin 0。
- `cargo tree` → 无 Tauri/GUI 依赖（已核验）。
- 8 项必做测试映射：幂等/未知字段（test 2）`capture_recapture_idempotent_and_unknown_fields_readable`；>32MB 字节一致（test 3）`large_record_above_read_budget_archives_byte_exact`；故障注入恢复+孤儿报告（test 4）`crash_before_manifest_commit_recovers_and_reports_orphans`；源删除后回读（test 5）`source_deleted_after_capture_readback_still_complete`；并发锁拒绝（test 6）`second_concurrent_capture_is_refused` + CLI 退出码 4；权限 fail-closed（test 7）`readback_is_fail_closed_on_missing_or_foreign_access_mapping`；对账计数与失败显式（test 8）`status_tracks_counters_and_failures_are_explicit`；CLI JSON 契约与退出码 `cli_json_contract_and_exit_codes`。

## 格式代际支持矩阵（P0 契约）

| Provider | 代际 | 支持 |
|---|---|---|
| codex | rollout `*.jsonl`（现行默认） | ✅ 支持 |
| codex | rollout `*.jsonl.zst`（zstd 单帧） | ✅ 支持（透明解码 + 解码对象入档） |
| claude | `projects/**/*.jsonl`（现行默认） | ✅ 支持 |
| claude | 注入（system-reminder / task-notification 等） | ✅ 支持（`injected` 标记、原文保留、派生视图排除） |
| claude | sidecar `tool-results/` | ✅ 模型/ContentRef 语义保留（附件归档本身是 P4，未做） |
| claude | compact | ✅ 支持（EventKind::Compaction） |
| opencode | 旧 JSON storage | ✅ 支持 |
| opencode | SQLite V1（session/message/part） | ✅ 支持 |
| opencode | SQLite V2（session_v2/session_message，现行默认） | ✅ 支持 |

无"伪装完成"项：本切片中所有契约代际均为真实支持并由测试钉住；第二批框架与上述之外的能力未实现也未宣称。

## 已知限制

1. **P2 未做**：JSONL 增量续采（水位按整文件指纹，不分字节偏移续采）、不完整尾记录、mtime 不前进、active/archived 迁移、指纹不连续的强制重捕/降级路径。
2. **P3 未做**：OpenCode SQLite 的"基线快照 + 行级差分"组合证据；SQLite 源目前仅整体对象入档，无行级记录/重放。
3. **P4 未做**：图片/附件/持久化工具输出的 ContentRef 映射归档。
4. **P5 未做**：覆盖报告的框架侧基准对账（session_index.jsonl 等）、观测盲区呈现、派生重放检查点。
5. **P6 未做**：套装离线分发、安装脚本、审计接入；`.gitignore` 未登记 `agent-memory/target/`（本目录自带 `.gitignore` 兜底）。
6. 采集为单来源整文件级；`read` 仅支持对象/记录 SHA-256 引用；权限策略仅采集时点标签 + 最严交集，无授权策略版本化（§9.7 未锁定，未实现任何放宽方向）。
7. 孤儿对象只报告不删除（按 §4.3 口径）；scrub 周期校验未实现（P5）。
8. Windows AV/索引器对刚写入文件的短暂占用以重试（20×10ms）处理；上游 ReplaceFileW 重试循环未移植（NOTICE 已记录）。

## 是否读取真实数据

**本切片测试数据全部为合成 fixture 与临时根**。诚实披露（历史违例，保留原文口径）：移植首轮联调时，一次失败的 opencode 用例曾随上游默认行为只读扫描过本机真实 OpenCode 库（表现为断言 2324≠2），随即通过移除 `OPENCODE_DB` 响应修复并复测全绿。

**对"最终代码路径不存在指向真实会话目录的通道"一句的更正**：该结论下得过早，已被 fresh verification 探针证伪——完工时刻的代码仍存在默认真实根通道（codex 标题加载经 home 兜底读真实 `<home>\.codex` 的 config.toml / session_index.jsonl / state_5.sqlite、OpenCode base-dir 绕过统一隔离解析、`lib.rs::scan_sessions()` 无参扫描三框架真实根）。这些通道已在边界修复中删除/封闭（fail-closed + 显式注入，反向测试钉住），修复事实与验收证据见 `P0P1-BOUNDARY-FIXED.md`。除此之外未读取任何真实 Agent 会话内容：上述历史违例与验证探针只触达会话元数据层面的只读扫描，未读取 000【备忘录】或任何凭据。
