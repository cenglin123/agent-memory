# P2/P3 验收修复记录

对象：`<temp>/opencode/recall-p2p3-verification.md`（verdict=FAIL）中 4 个必修项 + 2 个同时处理项。
结论：**4 个必修项全部修复并有入库测试；两个同时处理项已完成；既有 257 个套件测试与 22 个 vault 测试断言未删、全部保持绿（现为 266 + 26）。**
未执行任何 `git commit`；未读取真实笔记、真实 OpenCode 库、真实索引；未推进 Archive 或真实数据验证。

| 项 | 级别 | 状态 | 证据 |
|---|---|---|---|
| F1 `--strict-snapshot` OpenCode 失效 | 阻断 | **已修复** | 5 个新用例（SQLite/legacy JSON 的 search+read、CLI、device read）+ 临时目录清理用例 |
| F3 `-` 开头 query 误判 | 阻断 | **已修复** | recall 传 `--` 终止符 + stub 断言用例 + ask 侧用例 |
| F2 device 空正文 ref 不可回读 | 中 | **已修复（剔除口径）** | 0 字节文件用例 |
| F4 KB CRLF 两类无测试 | 中 | **已修复** | Rust 两类用例 + vault 侧哈希/`chunk_text` 用例 |
| F9 `contract_version` 缺 required/observed | 低 | **已处理** | `reason` 编码两值 + 断言 |
| F10 严格快照失败残留临时目录 | 低 | **已修复** | 失败清理 + 左证用例 |
| F5 GBK 用例无非 ASCII、F6 co-retrieval 无断言 | 低 | **已处理** | 两处用例加固 |
| F7/F8/F11/F13 | 低 | **部分处理 / 已登记** | 见 §5 |

---

## 1. F1 · `--strict-snapshot` 对 OpenCode（SQLite / legacy JSON）不可用

**根因**：`port::reader::resolve()` 用 `root_str` 推导 `rel_path` 与根内包含关系；严格模式把 `root`/`db`/`storage` 换成了副本路径，但 `root_str` 仍是配置原根，`rel_from_root(配置根, 副本路径)` 必然为 `None` → `Io("… outside the configured source root")`。

**修复**（与验收建议一致，`read_root` 自证）：

- `src/recall/providers/opencode.rs`：`resolve(read_root, read_root_str, ...)`；新增 `read_root_str = read_root.to_string_lossy()`，`remap_configured()` 仍以**配置根**为基准把 `db`/`storage` 映射进副本（配置路径 → 配置根相对路径 → 副本内同一相对位置）。`root_sha256` 继续取配置字符串，因此 ref/hit_id 在两种模式下完全相同。
- `src/recall/read.rs`：`read_opencode()` 同样改用 `read_root` 派生 `rel_path`/包含判定。
- `src/recall/port/reader.rs`：`resolve()` 文档明确 `root_str` 语义为"子路径所相对的树（配置根或严格快照副本）"，仅用于包含判定与 `rel_path`，不参与任何哈希。

**测试**（`tests/recall_kb_device.rs`、`tests/recall_cli_mcp.rs`）：

- `strict_snapshot_serves_sqlite_search_and_read`：同配置下正常/严格 `source_status=ok`、hit_id 集合相等、`side_effects=[]`、`memory_read --strict-snapshot` 回读与普通模式逐字节相等。
- `strict_snapshot_serves_legacy_json_search_and_read`：legacy JSON 源 status 与普通模式一致（fixture 含一个坏 part，故为 `partial`，关键是**未**降级为 `unavailable`），严格回读命中 `needlelegacy`。
- `strict_snapshot_flag_serves_an_opencode_source_through_the_cli`：复刻验收报告 §4.2 的 CLI 复现路径，`memory_search --strict-snapshot` 与 `memory_read --strict-snapshot` 均退出 0。
- `memory_read_strict_snapshot_reads_device_without_touching_the_root`：`memory_read` 层面的严格模式（验收缺口）。

**同时**：`snapshot.rs` 的 `StrictSnapshot::create()` 在 `create_dir_all` 或 `copy_tree` 失败时删除已创建的临时目录（F10），并有 `strict_snapshot_failure_removes_its_temporary_directory` 用缺失源根验证 `%TEMP%` 无 `agent-memory-strict-*` 残留。

---

## 2. F3 · 以 `-` 开头的合法 query

**修复**：

- `src/recall/providers/kb.rs` `build_args()`：脚本 + `--json --no-log --top-k <n>` 之后插入 **`--`**，再放 query。即 `python <args...> <ask.py> --json --no-log --top-k N -- -foo`。
- `.meta/scripts/ask.py`：argparse 本身在 `--` 之后把 `-foo` 当位置参数；另把 `--json` 下**可达**的参数校验失败（`--provider` 非法、`--hybrid-weights` 非法）改为输出单信封（`INVALID_REQUEST`，退出 2），使"所有可达失败路径单信封"成立；argparse 层未知选项仍走退出 2 + 空 stdout（recall 不会产生该 argv）。

**测试**：

- `kb.rs` 单元：`build_args_terminates_options_before_a_dash_query`（`--` 紧邻 query，且为倒数第二个参数）。
- `recall_kb_device.rs::kb_query_starting_with_a_dash_reaches_the_child_positional`：`kb_stub` 的 `dash-query` 分支**只有** query 恰为 `-foo` 才返回 `ok`，否则回 `INVALID_REQUEST`；用例断言 `kb=ok`、退出 0、1 条结果——证明 `--` 穿透子进程。
- vault：`test_dash_query_is_a_positional_after_the_option_terminator`（断言 `semantic_search` 实际收到 `-foo` 且信封 ok）、`test_json_parameter_errors_still_emit_one_envelope`（provider/权重错误仍是单信封）。
- `kb_stub` 的参数解析改为 argparse 语义（遇 `--` 后下一 token 即 query），与真实脚本一致。

---

## 3. F2 · device 空正文 + 仅文件名命中

**修复**（采用验收建议 ①，与 KB"无 ref 不进 results"一致）：`providers/device.rs` 计算投影后若 `byte_start == byte_end`，**剔除该候选**并计 `CONTENT_UNRESOLVED`；源状态 `partial`、`code=CONTENT_UNRESOLVED`（若同时有跳过则 `RECORD_SKIPPED` 优先，与 OpenCode 口径一致），`detail.CONTENT_UNRESOLVED` 计数；不再产出 `[0,0)` 的不可回读 ref。

**测试**：`device_empty_body_filename_hit_is_dropped_not_emitted_unreadable`（0 字节 `2026-10-25_calendar.md` + 查询 `calendar`）→ `results=[]`、`partial`/`CONTENT_UNRESOLVED`、`detail.CONTENT_UNRESOLVED=1`、退出 4、`sources_incomplete=["device"]`、`next_offset=null`；`status`/`detail` 形状仍过 `assert_status_shape`（冻结键集）。

---

## 4. F4 · KB CRLF 两类（入库证据）

**实现未改**（行为原本正确），补齐两仓测试：

- `tests/recall_kb_device.rs::kb_newline_chunk_cannot_match_a_crlf_file_but_single_line_chunk_can`
  - CRLF 文件 `line1\r\nneedle\r\n` + `chunk_text="line1\nneedle"`（含换行）→ `results=[]`、`partial`/`CONTENT_UNRESOLVED=1`、退出 4、`next_offset=null`；
  - 同一 CRLF 文件 + `chunk_text="needle"`（单行）→ `ok`、`text_range=[7,13)`、行 `[1,2)`、`memory_read` 回读 `"needle"`。
  - 由 `kb_stub` 新增的 `chunk-crlf-multiline` / `chunk-crlf-single` 分支驱动。
- vault `.meta/tests/test_ask_readonly_json.py::test_crlf_note_hash_matches_and_chunk_text_is_lf`：CRLF 笔记经 `\r\n→\n` 归一后 `embed_full_sha256` 与活动代 `content_hash` 相等、`_hit_chunk_text()` 照常产出 LF chunk、且该 chunk 在文件原始字节中**不存在**（即 recall 侧整段匹配必落空）；对应计划 P2.2 的"CRLF 笔记哈希仍相等、`chunk_text` 照常产出"样本。

---

## 5. 同时处理项与其余发现

- **F9（required/observed）**：`detail` 冻结键集不含 `required`/`observed`，故保留 `kb_contract_version`（observed，缺失为 `null`）并把两个数值编码进 `reason`：`contract_version_required_1_observed_missing` / `contract_version_required_1_observed_2`。`kb_contract_violations_are_unsupported_or_failed` 精确断言；README 同步说明。
- **F10**：见 §1 末尾（失败清理 + 用例）。
- **F5（GBK 用例）**：改为非 ASCII **结果路径**（`notes/中文标题.md`）进入信封，断言 UTF-8 单文档且含中文；不再只是"reconfigure 不报错"。
- **F6（co-retrieval 门控）**：新增 `test_co_retrieval_write_is_gated_by_no_log`，用注入的 `co_retrieval` 模块断言 `--no-log` 时 `record` 未被调用、默认时被调用一次（此前仅有结构性论证）。
- **F7（记录/流程）**：本报告补齐 repo/commit/路径与测试证据；`P2P3-RECALL-COMPLETED.md` 顶部加"状态更新"注记，说明其"未 commit"一句是生成时事实（随后两仓由 orchestrator 提交为 `6e824c1` / `810523a0`）。**vault `CHANGELOG` 追加、`maintain_trigger.py --pause/resume` 留痕仍未做**（见 §6）。
- **F8（device 首块 `[0,1)`）**：保持 `utf8_floor` 的"至少一个字符"语义；该区间可回读，不再产生不可回读 ref；空正文场景已由 F2 剔除规则覆盖。计划字面 `[0,0)` 只在"正文恰为空"时出现，而该情形现在不产出结果。
- **F11（symlink 计 skip）**：保持"文件型 symlink 跳过并计 `RECORD_SKIPPED`"（fail-closed 且显式）；未新增 junction fixture。
- **F13（terminate→kill）**：Windows 上 `Child::kill()` 即 `TerminateProcess`，无优雅终止语义；计划 §3.4 第 5 条与 §2.3 的"kill 后记 0 条"按后者实现，未改为两段式（避免给 KB 引入不可控的等待窗口）。

---

## 6. 测试命令与实测退出码

| 命令（工作目录） | 结果 | 退出码 |
|---|---|---|
| `cargo test`（`<suite-worktree>/agent-memory`） | **266 passed / 0 failed / 1 ignored**（含 5 个 compile-fail doctest） | **0** |
| `cargo test --doc`（同上） | 5 passed | **0** |
| `python .meta/tests/test_ask_readonly_json.py`（`<vault-worktree>`） | **Ran 26 tests … OK** | **0** |
| `cargo tree -p agent-memory --depth 1`（同上） | 直接依赖未变、无 `tauri` | **0** |

分布（套件）：lib 157（+1 `build_args_terminates_options_before_a_dash_query`）、`recall_kb_device` 25（+7）、`recall_cli_mcp` 7（+1）、其余与修复前一致（doctest 5、boundary 5+6、p0 4、p1 8、contract 9、opencode 22+1 ignored、read 18）。
vault：22 → 26（+dash query、+参数错误信封、+CRLF 哈希样本、+co-retrieval 门控）。

---

## 7. 基础信息与改动面

| 项 | 套件仓 | vault 仓 |
|---|---|---|
| 路径 | `<suite-worktree>` | `<vault-worktree>` |
| 修复基线 HEAD | `6e824c1` | `810523a0` |
| 本次提交 | **无**（工作区留改动） | **无** |
| 改动文件 | `agent-memory/README.md`、`src/recall/providers/{kb,device,opencode}.rs`、`src/recall/read.rs`、`src/recall/snapshot.rs`、`src/recall/port/reader.rs`、`tests/fixtures/recall/kb_stub/ask.py`、`tests/recall_kb_device.rs`、`tests/recall_cli_mcp.rs`、`agent-memory/docs/verification/P2P3-FIX-COMPLETED.md`（本文件） | `.meta/scripts/ask.py`、`.meta/tests/test_ask_readonly_json.py` |
| `Cargo.toml`/`Cargo.lock` | 未改 | — |
| Archive / 安装分发 / 真实数据 | 未触碰 | 未触碰 |

---

## 8. 剩余限制（未在本任务范围）

1. **F7 的流程留痕**：vault `docs/CHANGELOG*.md` 未追加本次条目（需 `changelog_append.py`）、`maintain_trigger.py --pause/resume` 无可核实载体；`P2P3-RECALL-COMPLETED.md` 以注记而非重写方式更正 commit 说法。
2. **F11**：device 的文件型 symlink 仍计 `RECORD_SKIPPED`（→ `partial`），无 junction fixture；行为 fail-closed、可解释。
3. **F13**：KB 硬超时仍为 `kill`（Windows `TerminateProcess`），未实现"terminate→等待→kill"两段式。
4. **严格快照的成本与上限**：整棵源根复制（512 MiB / 200 000 项上限），大库直接 `DB_UNAVAILABLE`；每次调用重新复制；WAL 库在副本中连同 `-wal`/`-shm` 一起复制。
5. **真实数据验证**：真实 vault 的 KB 端到端、真实 OpenCode 会话、`kb_python_baseline.json` 导出仍未执行（需单独授权，`integrate` 之后）。
6. **既有失败**：`python .meta/tests/test_ask_rerank.py` 2 个用例失败（只依赖 `common.py`，本次未改，属既有问题）。
7. `python -m unittest .meta/tests/…` 在 `.meta` 以点开头的目录上不可用（非合法模块路径），故以脚本直跑；未安装 pytest。
8. `import ask` 的测试会在 `.meta/scripts/__pycache__` 生成 `.pyc`（git 已忽略）；生产路径由 `interpreter.kb` 的 `-B`/`PYTHONDONTWRITEBYTECODE=1` 消除。
