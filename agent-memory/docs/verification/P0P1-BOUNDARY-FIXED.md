# P0/P1 边界修复完工报告 — 默认真实会话根通道删除/封闭（verification-001 返工）

日期：2026-10-25。工作区：`C:\wt\agent-memory-p0p1-20261025`（只改 `agent-memory/**`，未 git commit，未读取任何真实 Agent 会话数据）。
结论：**全部默认真实会话根通道已关闭，验收通过**（下方含逐项证据；如任一通道未关，本文件不会写成功）。

## 一、修改文件

| 文件 | 改动 |
|---|---|
| `src/config.rs` | `get_home_dir()` 改为 fail-closed（只认 `CC_SWITCH_TEST_HOME` 合成根，缺失返回 `None`）；删除 `get_claude_config_dir` / `get_codex_config_dir` / `read_codex_config_text` / `get_opencode_data_dir` / `get_opencode_db_path` 全部 home 兜底解析器；模块文档写明 Windows `dirs::home_dir()` 不受 HOME/USERPROFILE 影响 |
| `src/codex_state_db.rs` | 删除 `CODEX_SQLITE_HOME` 环境兜底；`resolve_user_path` 的 `~` 展开仅对授权合成根生效，无合成根时 fail-closed 跳过该 state db |
| `src/providers/codex.rs` | 删除 `scan_sessions()` / `session_roots()` / `scan_sessions_in_roots()` / `load_thread_titles()`（home 兜底链）；保留并公开 `scan_sessions_in_roots_with_titles(roots, titles)`（标题显式注入）；新增公开 `load_thread_titles_from_config_dir(config_dir)`（config 显式注入，读注入目录的 config.toml / session_index.jsonl / state_5.sqlite）；3 处单测改调 `_with_titles` 形式，断言原样保留 |
| `src/providers/claude.rs` | `scan_sessions()` → `scan_sessions_in_root(projects_root)`（显式根），删除 `get_claude_config_dir` 依赖 |
| `src/providers/opencode.rs` | 删除 `get_opencode_base_dir` / `get_opencode_data_dir` / `get_opencode_db_path`（XDG_DATA_HOME + `dirs::home_dir()` 绕过统一隔离解析的 base-dir 通道）；`scan_sessions()` → `scan_sessions_in(storage, db_path)`，`scan_sessions_json(storage)` / `scan_sessions_sqlite(db_path)` 全部显式传路径；单测不再操纵 XDG_DATA_HOME，断言原样保留 |
| `src/lib.rs` | 删除 `scan_sessions(provider_id)` 无参默认入口（原"无参数就扫描三框架真实根"敞口）；crate 文档新增 5 个 `compile_fail` doctest 反向测试 |
| `Cargo.toml` / `Cargo.lock` | `doctest = true`（启用编译期反向测试）；移除未使用的 dev 依赖 `serial_test`；移除 `dirs`（其唯一用途即被删掉的 home 兜底） |
| `tests/boundary_no_default_roots.rs` | 新增：5 项边界反向测试（见下） |
| `tests/p0_formats.rs` | 仅改头注释：与实际测试位置对齐（OpenCode 三代际/Claude sidecar 实测在各 provider 单元测试），断言未动 |
| `README.md` | 新增 "Isolation: no default real-session-root channels" 节；写明 Windows `dirs::home_dir()` 不受 HOME/USERPROFILE 影响、"设置临时 HOME"不构成隔离证明；测试计数更新 |
| `NOTICE` | 本地修改第 4 条重写（无 home 兜底、无环境重定向、默认入口删除）；依赖清单更新；补 Windows home 局限 |
| `P0P1-COMPLETED.md` | 更正完成日期笔误（2026-10-26→2026-10-25）；补实现文件清单遗漏（`.gitignore`、本类报告）；依赖行同步；p0_formats 测试位置说明；"最终不存在真实根通道"过早结论更正并保留历史违例披露 |

## 二、默认根封闭方式

统一原则：**任何会话根/配置解析只接受显式注入；唯一的环境开关 `CC_SWITCH_TEST_HOME` 只授权合成根，缺失即 fail-closed（`None`），绝不兜底真实 home**。

1. **Codex 标题/config/root**：`scan_sessions_in_roots_with_titles(roots, titles)`（root+标题双显式）与 `load_thread_titles_from_config_dir(config_dir)`（config 显式注入）。测试用显式标题 map 或合成 config 目录（诱饵同）。删除了经 `get_home_dir()`→`dirs::home_dir()` 读真实 `<home>\.codex` 的 config.toml / session_index.jsonl / state_5.sqlite 的整条链。
2. **OpenCode base-dir**：删除（连同 `XDG_DATA_HOME` / `dirs::home_dir()` 解析），扫描 API 一律显式传 storage/db 路径；不再存在绕过统一隔离解析的 base-dir。
3. **`lib.rs::scan_sessions()`**：删除（连同三个 provider 的无参 `scan_sessions()`），编译期不可达，由 `compile_fail` doctest 钉住；P2 必须建在显式注入上，无隐性真实根后门。
4. **环境通道**：`OPENCODE_DB`（P0/P1 时已移除）、`CODEX_SQLITE_HOME`（本次移除）、`CODEX_HOME`、`XDG_DATA_HOME`、HOME/USERPROFILE 一律不响应；`sqlite_home = '~/...'` 的 `~` 只对授权合成根展开，无则跳过该 db（fail-closed）。
5. **Windows HOME 局限**：`dirs::home_dir()` 走 `SHGetKnownFolderPath(FOLDERID_Profile)`，**HOME/USERPROFILE 重定向无效**，故"设置临时 HOME"不作为隔离证明；README/NOTICE 已写明，且 crate 不再调用 `dirs::home_dir()`（`dirs` 依赖已删，源码 grep 仅剩注释提及）。

## 三、裸测试命令与退出码

命令（shell 中显式移除 CC_SWITCH_TEST_HOME / OPENCODE_DB / HOME / USERPROFILE / XDG_DATA_HOME / CODEX_SQLITE_HOME / CODEX_HOME 后执行，不设置任何替代值）：

```
cargo test
```

- **exit 0**（最终复跑 2026-10-25）：lib **108 passed**（移植测试数与修复前一致，未削弱）、bin 0、`boundary_no_default_roots` **5 passed**、`p0_formats` **4 passed**、`p1_archive` **8 passed**、Doc-tests **5 passed**（compile_fail 反向）。合计 130。
- `cargo tree` → **exit 0，92 行**；`tauri|wry|gtk|webkit|tao|muda|tray|dirs|serial_test` 命中 **0**（无 GUI）。
- grep/代码审查：`src/` 中 `home_dir|get_claude_config_dir|get_codex_config_dir|read_codex_config_text|get_opencode_data_dir|get_opencode_db_path|OPENCODE_DB|CODEX_SQLITE_HOME|XDG_DATA_HOME|USERPROFILE|scan_sessions(` 命中仅剩文档/注释行与 fail-closed 的 `get_home_dir()` 本身；无默认真实根。

## 四、反向测试（能真实抓住旧问题，含负向对照实证）

`tests/boundary_no_default_roots.rs`（5 项）：

1. `bare_env_home_resolution_fails_closed` —— 无任何环境变量/无显式根时 `config::get_home_dir()` 必须为 `None`；旧实现（`dirs::home_dir()` 兜底）在此返回真实 profile，即被抓住。
2. `decoy_home_titles_are_read_only_through_explicit_injection` —— 合成 home（`CC_SWITCH_TEST_HOME` 显式授权）下埋诱饵 `session_index.jsonl`（PROBE-DECOY-INDEX-TITLE）与 `state_5.sqlite`（PROBE-DECOY-DB-TITLE）；显式根扫描 + 空标题 map 必须只出文件派生标题，显式注入同一合成 config 才可得到诱饵标题。此即旧 bug 的探针场景复刻。
3. `tilde_sqlite_home_resolves_only_against_authorized_synthetic_home` —— 注入 config 的 `sqlite_home='~/.codex-alt'`：无合成根时 state db 必须被跳过（fail-closed），有授权合成根才解析。
4. `opencode_decoy_storage_is_read_only_through_explicit_injection` —— 合成 home 下埋 OpenCode 诱饵 storage/db，即使 XDG_DATA_HOME/CC_SWITCH_TEST_HOME 全指过去，也只有显式路径 API 可见（无参入口由 compile_fail doctest 钉住）。
5. `source_tree_contains_no_real_home_resolution` —— 源码级绊线：`src/` 代码（剔除注释行）不得出现 `dirs::home_dir`、`HOME`/`USERPROFILE` 读取、`OPENCODE_DB`/`CODEX_SQLITE_HOME` 响应。

`src/lib.rs` crate 文档 5 个 `compile_fail` doctest —— `agent_memory::scan_sessions("codex")`、`providers::{codex,claude,opencode}::scan_sessions()`、`config::get_codex_config_dir()` 均须编译失败（API 编译期不可达）；`Cargo.toml` 已改 `doctest = true` 使之随 `cargo test` 执行。

**负向对照实证**（证明断言不是摆设，已回滚临时改动）：

- 在 `scan_sessions_in_roots_with_titles` 临时重新接入 home 兜底标题加载（复刻旧通道）→ `cargo test --test boundary_no_default_roots` **exit 101**，精确失败于 `decoy_home_titles_are_read_only_through_explicit_injection`："ambient (home-fallback) title loading leaked into an explicit scan"；回滚后全绿。
- 把一个 `compile_fail` doctest 临时指向现存 API → `cargo test --doc` **exit 101**："Test compiled successfully, but it's marked `compile_fail`"；回滚后全绿。

既有 P0/P1 测试未削弱：lib 108 / p0_formats 4 / p1_archive 8 数量与断言全部保留（codex 3 处单测仅把调用换成显式注入入口 `scan_sessions_in_roots_with_titles`，断言一字未改；opencode 3 处单测仅去掉 XDG_DATA_HOME 操纵、改显式 db 路径，断言一字未改）；原"诱饵标题可污染扫描"的隐性依赖改由第 2 项反向测试显式钉住，净增强。

## 五、历史违例与当前结论

- **历史违例（保留披露）**：移植首轮联调时，一次失败的 opencode 用例曾随上游默认行为只读扫描过本机真实 OpenCode 库（断言 2324≠2），经移除 `OPENCODE_DB` 响应修复；fresh verification 另以合成探针证实完工时刻仍存在默认真实根通道（codex 标题 home 兜底读真实 `<home>\.codex` 的 config.toml/session_index/state db、OpenCode base-dir 绕过隔离、`lib.rs::scan_sessions()` 默认扫描三框架真实根、Windows 下 HOME/USERPROFILE 重定向无效）。均只读触达会话元数据层，未读取会话正文/000【备忘录】/凭据。
- **当前结论**：上述通道全部删除或封闭（显式注入 + fail-closed），裸 `cargo test`（无 HOME/USERPROFILE/CC_SWITCH_TEST_HOME/OPENCODE_DB）全绿 exit 0 且不读真实 Agent 数据；`cargo tree` 无 GUI；反向测试（含负向对照）钉住不回归。**本次修复全程未读取真实 `~/.codex` / `~/.claude` / OpenCode 数据**——边界反向测试的诱饵全部位于合成临时目录且仅经显式授权/显式注入使用。
