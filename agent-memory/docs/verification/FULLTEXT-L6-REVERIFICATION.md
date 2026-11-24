# 全文检索优先 L6 修复项 — 复验报告（FULLTEXT-L6-REVERIFICATION）

- **Verdict：通过**
- **复验人**：独立 Verifier（同一验收任务第二轮；与实现者不同模型族）
- **日期**：2026-10-30
- **对象**：重建的隔离检出 `C:\wt\l6-verify`（detached HEAD `0f23c7344b813ecc338a128f0910d4b71dc1db44`，工作区干净）；修复提交 `7eb3586`，分发物 `731d07c`（重建）/`0f23c73`（谱系刷新）
- **上一轮报告**：`FULLTEXT-L6-VERIFICATION.md`（verdict 阻断需修复：B1/B2）已由 orchestrator 提交，本轮未改写；本文件为唯一新增 sentinel。
- **隔离声明**：与上一轮相同——只读、全部测试在临时目录与合成 fixture、未读取真实会话/知识库/生产 Archive/真实配置、未注册/删除真实计划任务（任务只做生成内容检查）、未跑真实 61/64 安装。

## 逐条结论

### 1. cargo test（同一 msys/git-bash shell，大写 `SYSTEMROOT` 环境）——通过（含一个非阻断的既有 flaky 竞态说明）

- `cargo test --locked --no-fail-fast`（`agent-memory/`，本 shell 原生环境运行）：
  - 第一次完整运行：**615 passed / 2 failed / 2 ignored**（exit 101）。失败的 2 条为 `recall_archive_graph::{a_live_source_never_reports_related, a_codex_subagent_session_is_never_expanded}`——**与上一轮的 3 条 SSL 失败完全不同**（上一轮的 `recall_config_v2`×2 与 `recall_archive`×1 本轮全部转绿，证明 B1 与 N2 修复生效）。
  - 这 2 条的失败是**先于修复存在的测试隔离竞态**，非 7eb3586 引入：错误为 `Os code 3`（ERROR_PATH_NOT_FOUND，父目录消失）于 `<tmp>\.tmpX\.tmpY\archive\state\schema.json`（双层嵌套临时目录）。机制：测试体先 `fixture()`（`tempfile::tempdir()` 按当时进程 `%TEMP%` 落位）后 `TestEnv::new()`；当另一并行线程正持有 TestEnv（TEMP 被进程级重定向到其目录）时，本线程的 fixture 目录嵌套进对方目录；对方测试结束时 `TestEnv::drop` 递归删除宿主目录，连带拔掉本线程的 archive。证据：① 单独重跑该 target 3 次 → **17/17 全绿**；② 紧接的全量重跑 → **exit 0，617 passed / 0 failed / 2 ignored**（26 target）；③ 上一轮同一二进制同测试集 17/17 全绿；④ 竞态模式（fixture 先于 TestEnv + drop 递归删除）在 7eb3586 之前即存在。
  - **结论**：干净运行 **0 failed 达标**；该竞态为既有 flaky（随调度偶发），列为本轮唯一非阻断发现（见 §N1），建议后续修复测试隔离（`TestEnv::new()` 先于 `fixture()`，或 TestEnv 不重定向 `TEMP/TMP`）。
- **计数说明**：orchestrator 期望 "618 passed"，实际非忽略总数为 **617**（三轮一致：上轮 614+3、本轮 615+2、本轮重跑 617+0）。7eb3586 未新增 Rust 测试（`git show 7eb3586` 仅改 `tests/support/mod.rs`；新增的 1 条是 Python 用例 `ProbeOllamaTests`，不计入 cargo）。"618" 应为把 Python +1 误计入 cargo 所致。

### 2. SSL 失败最小复现下的 `probe_ollama` / 完整 install——通过

沿用上一轮的唯一有效复现载体（迷你 Rust spawner 复刻 TestEnv 环境块；`env -i` 纯内容复刻上一轮已证明对旧代码也通过、不构成复现），模式 `o`（基线 + `OneDrive×3`、**无 `SystemRoot`**——上一轮确定的最小触发组合）：

- 底层触发条件未变（符合预期，属 OS/CPython 行为）：`import ssl; ssl.create_default_context()` → 仍抛 `ssl.SSLError: [SSL] unknown error (_ssl.c:3138)`，exit 1。
- **修复后 `probe_ollama('qwen3-embedding:4b')`**：同一环境 → `{'reachable': False, 'pulled': False, 'model': 'qwen3-embedding:4b', 'detail': '[SSL] unknown error (_ssl.c:3138)'}`，**exit 0**——报告不可达而非抛出。
- **同一环境下完整 `recall_onboarding.py install`**：**exit 0**，各项 `recall-config PASS / mcp-registration PASS / prompt-route SKIP / kb-harness PASS / recall-embedding WARN`（WARN=不可达，整体不失败）。

### 3. `63-verify-agent-memory-lineage.ps1`——通过

`powershell -File assets/scripts/63-verify-agent-memory-lineage.ps1` → **19/19 全部 PASS，exit 0**（含上一轮失败的 2 条端到端：`HEAD~1 谱系下安装 + 90 审计通过（PS 5.1/PS 7）` 与 `HEAD~1 谱系下 build -Verify 通过`）。B2 修复（沙箱补拷 `recall_embedding.py`）实证有效。

### 4. 跨目录字节复现 `-Verify`——通过

`powershell -File agent-memory/build/build-release.ps1 -Verify`（在 `C:\wt\l6-verify`，与 orchestrator 构建目录 `C:\wt\hybrid-retrieval-20261028` 不同路径）：

- **VERIFY OK，exit 0**。第一次构建（检出内 target）哈希 = 第二次构建（**全新 %TEMP% 临时 target 目录**，真重新编译链接）哈希 = **`20ea2093348e0bf84766c7f3254c9d1267cfdf4c07983d966c2a86a68087444b`** = 清单 `binarySha256` = 分发物。跨目录（两种路径）字节一致，上一轮 N1 的"仅原目录可复现"已消除。
- PE 特征确认 `/Brepro` 生效：`TimeDateStamp=0x37b577ae`（内容哈希常量，非墙钟；上一轮为真实构建时刻）；调试目录 4 条目 `tds` 全 0、原随机 RSDS PDB GUID 不复存在；`binaryBytes=9713664`。
- 谱系：清单 `commit=731d07c…（HEAD 祖先）`（0f23c73 仅刷新清单，构建输入未变），`treeSha256=2fa1ef6f…` 与当前树一致，`rustflags` 已记录 `-C target-feature=+crt-static -C link-arg=/Brepro`。

### 5. 62 与 pytest——通过

- `powershell -File assets/scripts/62-verify-agent-memory-offline.ps1`（PS 5.1.19041.7802）：**37/37 PASS，exit 0**。
- `pwsh -File …`（PS 7.6.9）：**37/37 PASS，exit 0**。
- `python -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`（`assets/scripts`）：**67 passed**（上一轮 66 + 新增 `ProbeOllamaTests::test_an_ssl_failure_while_building_the_opener_is_reported_not_raised`），exit 0。

### 6. 修复本身未引入新问题（7eb3586 代码审查）

- **B1**（`recall_onboarding.py` probe_ollama）：`Request`/`build_opener` 移入 `try`，注释准确说明"构造 opener 即可失败（HTTPS handler 急切创建 SSL 上下文）"；`ssl.SSLError ⊂ OSError` 落入既有 except；成功路径行为不变；新增回归测试用 `mock.patch.object(build_opener, side_effect=ssl.SSLError)` 精确钉住本缺陷。无新问题。
- **B2**（63 沙箱复制清单 + `recall_embedding.py`）：最小且精确；63 复跑 19/19 即证。
- **N2**（`tests/support/mod.rs`）：PASSTHROUGH 改 `eq_ignore_ascii_case` 查找、按规范名（`SystemRoot`）回填——行为对既有精确大小写环境不变，仅救回大写导出场景；与 cargo test 全绿互证。
- **N1**（`build-release.ps1`）：`/Brepro` 追加进 `rustflags`（仅此一次构建生效的临时注入逻辑保持不变）；第二次构建改用带 GUID 名的全新临时 target 目录并 `finally` 清理，`--target-dir` 引号处理正确；`-SkipReproCheck`（63 使用）路径不受影响；manifest 记录新 `rustflags` 与 `reproducibleDoubleBuild=true`（本次为真双构建）。导入表仍无 GUI/无动态 CRT（-Verify 的 hardened 断言通过）。
- 未发现修复引入的其他行为变化；L6 主功能面（64/90/62/pytest）全部复验通过。

## 非阻断发现

- **N-RE1（既有 flaky，建议后续修复）**：`recall_archive_graph` 两条用例在全量并行运行中存在测试隔离竞态（`fixture()` 先于 `TestEnv::new()` + 进程级 TEMP 重定向 + `TestEnv::drop` 递归删除），偶发 `ERROR_PATH_NOT_FOUND`。上轮该 target 全绿、本轮单独重跑 3×17/17、全量重跑 617/0/2——非确定性且先于 7eb3586。建议：测试体内把 `TestEnv::new()` 提到 `fixture()` 之前，或让 `TestEnv` 不重定向 `TEMP/TMP`（使 tempfile 目录永不嵌套进 TestEnv 目录）。
- **N-RE2（计数勘误）**：orchestrator 期望 "618 passed / 2 ignored / 0 failed"，实际非忽略总数 **617**（无新增 Rust 测试；+1 为 Python 用例）。0 failed 已达成，判定不受影响。

## 已执行命令与退出码

| # | 命令 | 退出码 / 结果 |
|---|---|---|
| 1 | `git -C C:/wt/l6-verify log --oneline -6 && status --short && rev-parse HEAD` | `0f23c734…` ✓；工作区干净 |
| 2 | `sha256sum assets/dist/agent-memory/agent-memory.exe` | `20ea2093…744b` ✓ == 任务给定值 |
| 3 | `git show 7eb3586`（全文 diff 审查） | 四项修复对症，无新问题（见 §6） |
| 4 | `cargo test --locked --no-fail-fast`（第一次完整运行） | exit 101：615/2/2 → 2 条为 `recall_archive_graph` flaky 竞态（§1） |
| 5 | `cargo test --locked --test recall_archive_graph` ×3（隔离） | 3× `17 passed; 0 failed` |
| 6 | `cargo test --locked --no-fail-fast`（第二次完整运行，日志复核总数） | **exit 0：617 passed / 0 failed / 2 ignored**（26 target） |
| 7 | `powershell -File agent-memory/build/build-release.ps1 -Verify`（跨目录） | **exit 0：VERIFY OK**；重建 == 清单 == dist `20ea2093…`；`commit 731d07c…（HEAD 祖先）` |
| 8 | 新 exe PE 解析（TimeDateStamp/调试目录/大小） | `/Brepro` 生效：哈希时间戳、tds 全 0、无随机 GUID；9713664 字节 == 清单 |
| 9 | `powershell -File assets/scripts/63-verify-agent-memory-lineage.ps1` | **exit 0：19/19 PASS** |
| 10 | `powershell -File assets/scripts/62-verify-agent-memory-offline.ps1`（PS 5.1） | **exit 0：37/37** |
| 11 | `pwsh -File assets/scripts/62-verify-agent-memory-offline.ps1`（PS 7.6.9） | **exit 0：37/37** |
| 12 | `python -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py` | **exit 0：67 passed**（4.73–9.33s） |
| 13 | 迷你 Rust spawner（上一轮最小复现载体，mode=o：无 SystemRoot + OneDrive×3）→ `ssl.create_default_context()` | exit 1 `ssl.SSLError [SSL] unknown error (_ssl.c:3138)`（底层触发条件仍在） |
| 14 | 同环境 → `probe_ollama('qwen3-embedding:4b')` | **exit 0**：`reachable=False, pulled=False, detail='[SSL] unknown error…'` |
| 15 | 同环境 → 完整 `recall_onboarding.py install`（临时 UserRoot + kb 桩） | **exit 0**：`recall-embedding WARN`，其余 PASS/SKIP |
| 16 | `cat assets/dist/agent-memory/build-manifest.json` + `git show 731d07c --stat` / `git diff c170f75..0f23c73 --stat` | 谱系与 rustflags 记录正确；改动集与声明一致 |

—— 本报告为第二轮唯一新增 sentinel；上一轮 `FULLTEXT-L6-VERIFICATION.md` 未改写；被检检出未做任何修改，未执行任何 git 写操作。
