# 最小统一回忆 P0/P1 — 复审修正记录（sentinel）

状态：fresh verification 的 **B1–B4 已修复**，关键非阻断项（F4/F7/F8/F9/F10/F11/F14/F16 等）已处理；P2/P3 未推进。
真实数据：**否**。全部用例只使用运行时写入 `<temp>` 的合成 fixture；未读取真实会话、真实凭据、真实用户级配置。

## 1. 验证命令与结果

```sh
cd agent-memory
cargo test            # exit 0 — 221 passed, 0 failed, 1 ignored, 0 warnings
cargo tree --depth 1  # exit 0 — 9 个直接依赖，无 tauri
```

- 组成：144 lib 单测 + 72 集成测试 + 5 compile-fail doctest。
  集成分布：`boundary_no_default_roots` 5、`p0_formats` 4、`p1_archive` 8、
  `recall_boundary` 6、`recall_contract` 9、`recall_opencode` 22（另 1 ignored）、`recall_read` 18。
- 唯一 ignored 规模用例（1000 会话 / 100000 part，`data` 合计 35,679,000 B ≈ 34 MiB）：
  - release：0.892 / 0.960 / 0.903 s（取最差 0.960 s）；
  - debug：5.12 / 5.32 / 5.41 s（目标 ≤ 10 s、目标值 5 s）；
  - 同用例 32 MiB 对照：`scanned_sessions < total_sessions` + 源级 `truncated` + `BUDGET_BYTES`（fixture 体量由 `SELECT SUM(LENGTH(data))` 断言 > 32 MiB，不再使用与 fixture 不自洽的数值）。
- 实现记录（基线核对，本 worktree 实跑）：
  - repo：套件仓（工作目录 `recall-mvp-20261025`），HEAD `49aba07`；
  - `git cat-file -t c8aae7b` → `commit`；`git merge-base --is-ancestor c8aae7b HEAD` → exit 0；
  - `git diff c8aae7b -- agent-memory/Cargo.toml agent-memory/Cargo.lock` → 空（直接依赖自基线未变）；
  - cc-switch 基线：`v4.0.20` / `85ad4969b3399500baaea9d46b1e08f87aba80e1`（沿用 Archive 切片已登记的移植与 `LICENSE`/`NOTICE`；本次不重新 vendor，见 §2 路线）。

## 2. 修改清单

### B1 · snippet 以首命中为中心（`src/recall/text.rs`、`providers/opencode.rs`）

- `LexMatch` 新增 `first_term_len`；`snippet(text, first_byte, hit_len, limit)` 以命中为中心取窗：
  命中词 ≤ 窗口时必含命中字节；短行优先对齐行首；长单行命中在行尾时保持可见；
  起点/终点均对齐 UTF-8 字符边界（长度最多 `limit + 3`）。
- 新增单测：长单行末尾命中、CJK 多字节窗口；集成断言 `endoutput`（20 000 字符工具输出末尾）
  的 snippet 必含命中词且 `len <= 400`。

### B2 · `port/` 不再产生 `DB_*` 码与响应 detail（`port/model.rs`、`providers/mod.rs`）

- 删除 `PortError::code()` / `PortError::detail()`；`port` 只保留变体与 `message()`。
- 新增 `providers::port_code()` / `providers::port_detail()`，作为唯一映射权威；
  `SourceOutcome::from_port_error` 只调用它们。`detail.reason` 仍只使用冻结键。

### B3 · `memory_read` 校验 OpenCode `ref.rel_path`（`src/recall/read.rs`）

- 载入 part 后要求 `part.rel_path == ref.rel_path`（SQLite = 配置 `db` 相对 `root`；
  旧 JSON = locator 推导的 part 文件），不符 → `REF_LOCATOR_MISSING`，不返回正文。
- 反向用例覆盖 V1、V2、旧 JSON（含不存在文件与 `../escape.json`）。

### B4 · 规模 fixture 与预算对照自洽（`tests/support/mod.rs`、`tests/recall_opencode.rs`）

- 生成器由 80 字符正文提升到 300 字符：100000 part 的 `data` 合计 35.7 MB > 32 MiB。
- 规模用例先断言 fixture 体量 > 32 MiB，再跑 32 MiB 对照（`scanned < total` + `truncated` + `BUDGET_BYTES`），
  同时保留 64 MiB 全量扫描的延迟断言；注释与报告同步修正。

### 非阻断项

- **F4 块读取接入读路径**：`legacy_json` 的 session/message/part 读取全部改走
  `read_bounded_file`（64 KiB 对齐块、读取前 metadata 上界检查）；part 文件超过
  `max_bytes_per_source` 时不读取并计 `oversize`。SQLite 侧按 part 语义设上界
  （V1 在 SQL 侧按 `length(data)` 过滤并计数；V2 以 `content[i]` 序列化长度计
  oversize，仅对整行设 64 MiB 硬上限），不整读大对象。
- **F7/F10 side_effects**：分类规则改为“已知 A/B 路径 = 违约；运行时 sidecar = C；
  新建/未完成（before 为 0 字节）文件 = C `window_only`；既有源载体被修改/删除 = 违约”，
  不再把所有新文件判成泄漏。新增：`Window` 真实变更用例、
  `memory_read` 并发窗口非空 C 级用例、已知 A/B 路径写入导致
  `SIDE_EFFECT_VIOLATION` 的端到端用例。
- **F8**：新增 `tests/fixtures/recall/kb_stub/ask.py`（ok/degraded/error/timeout/
  `contract_version` 缺失/≠1/`detail.mode` 缺失分支），仅作 P2 离线替身，不被当前用例执行。
- **F11**：`recall::load_config` 已 re-export；README 示例可编译。
- **F12**：新增 V2 注入/元数据/长 `state.input`/非字符串 `data.text` 用例。
- **F14**：V2 oversize 按 `content[i]` 的 part 长度判定，不再用整行长度误伤小 part。
- **F16**：测试计数与用例分布按实际输出记录（`recall_opencode` 22 passed + 1 ignored）。

### 路线说明（原 A1）

- **复用现有移植模型，未重复 vendor**：cc-switch `SessionMessage`/`SessionBlock`/`PartLocator`
  等已存在于 Archive 切片（`src/model.rs`、`src/providers/*`，基线 `v4.0.20` /
  `85ad4969…`）；recall 复用该模型，只在 `src/recall/port/` 补充本计划需要的原语。
  `NOTICE` 已更新为“复用 + 文件清单 + 无第二份 vendor / 无 cc-switch 依赖”。

## 3. 支持矩阵

| 能力 | 状态 |
|---|---|
| 配置（必填/未知键/顺序/路径/解释器）、请求逐键、退出码 | 完成 |
| `hit_id`/`ref` 八键/`root_sha256`/`text_range`/`range_sha256` | 完成 |
| OpenCode 旧 JSON / V1 / V2 正文检索、代际探测、V1 回退 | 完成 |
| `project_part_text`（tool 全量、无 preview）、注入三谓词、`ignored_types` | 完成 |
| `parent_id`/`ocsr-` 排除、预算推进、`coverage`、`truncated`/`oversize` | 完成 |
| 64 KiB 块读取（legacy JSON）+ 前缀指纹 | 完成 |
| `memory_read` 单/多段、上下文、`max_bytes`、`rel_path` 校验、`REF_*` | 完成 |
| `WARN_FILE_STALE`、`side_effects[]`（C 级 + A/B 违约端到端） | 完成 |
| 分层边界（port 不产生 `DB_*`/detail） | 完成 |
| KB/device 适配器、CLI、stdio MCP、`--strict-snapshot` | **未实现（P2/P3）** |

## 4. 剩余限制

- P2/P3 全部未实现；`kind=kb|device` 以 `unsupported` + `reason=adapter_pending_p2` 显式占位。
  若未显式写 `reason`，合法 KB 配置在 P2 落地前会被误读为 schema 不支持——这是占位语义，
  交付说明已注明，P2 必须替换。
- `kb_python_baseline.json` 仍需在真实库上单独授权的一次性只读导出，未生成。
- 未实现持久索引、学习/晋升、跨源 score 归一化或跨源去重。
- 仅在 Windows 验证；`root_sha256` 对非 ASCII 字母不做 Unicode 大小写折叠（只折叠 ASCII，
  保持字节长度以便 rel_path 切片）。
- SQLite 行读取由 `rusqlite` 按行提供，无法像文件那样分块流式解码；上界以单 part/整行长度控制。
- `side_effects[]` 只声明“窗口内有变化”，不归属写入者；新建文件的来源无法判定时只标
  `window_only`。
