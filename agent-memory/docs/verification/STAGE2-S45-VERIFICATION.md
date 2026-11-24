# STAGE2-S45 独立验收报告：SQLite 采集档案参数化（SqliteCaptureProfile）与 64 `-ArchiveCaptureOnly`

- 验收对象：隔离检出 `C:\wt\s45-verify`，HEAD = `ba16320`（`fix(suite): -ArchiveCaptureOnly 参数移到 install() 签名末尾`），基线 `320941e`；`HEAD^{tree}` = `433976a9…`，与 `C:\wt\stage2-s45` 同树；两处 `git status --short` 均为空（本报告写入 `C:\wt\stage2-s45` 前）。
- 验收角色：独立 Verifier（全新上下文；与实现者不同模型族），只读被检仓库，不 commit。
- 环境：Windows；`rustc 1.99.0` / `cargo 1.99.0`；Python 3.13.17；PowerShell 5.1；v2 fixture 与旧版二进制均由本 Verifier 在专用目录 `C:\wt\v45-work` 构造，未改动被检仓库。
- 隔离：未读取任何真实会话/数据库/生产 Archive/真实配置（`<user-home>/.local/share/opencode`、`.claude/projects`、`.codex`、`.session-evidence`、`.agent-memory`、`.config/opencode`、`.hermes` 全程未读）；未注册/删除任何真实计划任务（64 路径用 fake executor + 临时 UserRoot）；未运行真实 61/64/90；cc-switch 仅只读其公开源码 `session_manager/providers/hermes.rs`、`mcode.rs`。
- 测试全部使用合成数据库与临时目录；旧版二进制由 `git archive 320941e` 导出后独立构建（`target-old`），新版由被检树构建（`target-new`），互不污染。

## 结论（verdict）

**阻断需修复**

代码与行为层面全部通过：691/3 全量测试、OpenCode 证据与旧二进制逐项等价（含首次/增量/`--session-since`/`--verify`/replay）、Hermes/mcode 风格合成档案端到端（含哨兵 0 命中、档案隔离与切换）、64 `-ArchiveCaptureOnly` 的 CLI 入口与检索语义、规模用例内存与预算断言均符合规格。

**唯一阻断项是文档卫生**：本 diff 新增的完成记录里出现了一条真实用户主目录路径，违反本次验收第 7 条「新增行无 `<user-home>` 真实路径前缀」与全局「真实用户路径用 `<user-home>` 占位、仅防新增」的规则。修复只需改一行，不涉及代码。

---

## Blocking

### B1. 新增文档行包含真实用户主目录路径

- 位置：`agent-memory/docs/verification/STAGE2-S45-COMPLETED.md:4`（该文件在本 diff 中为新增，129 行）。
- 核实命令与证据：
  - `grep -nF 'C:\Users' agent-memory/docs/verification/STAGE2-S45-COMPLETED.md` → 命中第 4 行（`cc-switch 源码 C:\Users\<user-name>\Documents\GitHub\cc-switch …`）。
  - `git -C C:\wt\s45-verify diff 320941e ba16320 --unified=0 | grep '^+' | grep -E 'C:\\Users'` → 全 diff 仅此 1 条新增行命中。
  - 同目录既有文件（`STAGE2-S1-VERIFICATION.md`、`FULLTEXT-L5B-VERIFICATION.md` 等）也含同类路径，属历史存量；本次规则是「仅防新增」，因此这条新增行仍需修。
- 为何列为阻断：这是本次验收第 7 条的显式验收条件，且路径隐私规则要求提交到 Git 的文件使用 `<user-home>` 占位；完成记录会随 orchestrator 提交进入仓库。
- 建议（最小修复）：把该行改为 `cc-switch 源码 <user-home>/Documents/GitHub/cc-switch`（或「本地 cc-switch 公开源码」），其余不变。

---

## Non-blocking

### N1. 64 内联注释仍写「已配置启用的 archive 源」

- 位置：`assets/scripts/64-install-recall.ps1:129`（`# 先采集后索引：需要 recall.json 已配置启用的 archive 源…`）。
- 证据：同文件参数帮助（`:23`）与实现已改为「只要求 archive 源存在」；`recall_onboarding.py:1382-1386` 的 `capture_enabled` 判定已去掉 `source.get('enabled')`。实测 `-ArchiveCaptureOnly` + `-CaptureSchedule`（archive `enabled:false`）无 FAIL 且包装脚本带 capture-all。
- 为何不阻断：仅注释过时，不影响行为；但会误导后续维护者以为禁用源不会开启采集。
- 建议：把注释改为「需要 recall.json 已配置 archive 源（启用与否均可，capture-all 用其 root）」。

### N2. 完成记录 §4 的「保留字面量清单」漏了一条同类兜底

- 位置：`agent-memory/src/capture_sqlite.rs:2516` `.unwrap_or("id")`（`backfill` 计算会话键列）；完成记录 §4 只列了 `observe_one_table`/`verify_table_merge` 的两处 `.unwrap_or("session_id")`。
- 证据：该分支外层已由 `profile.is_session_table(&change.table)` 门控，`unwrap_or` 运行时不可达；`grep` 全文件后仅此一条未入清单。
- 为何不阻断：与已声明保留项同类，不构成「不经档案的 OpenCode 逻辑」。
- 建议：在完成记录 §4 补一行，保持审计清单完整。

### N3. `SqliteBaseline::profile()` 对未知非空 `profile_id` 静默回落 OpenCode

- 位置：`agent-memory/src/capture_sqlite.rs:3071-3073`。
- 证据：`sqlite_capture_profile_by_id(&self.profile_id).unwrap_or(OPENCODE_PROFILE)`；若证据记录的是尚未在本进程注册的合法档案 id（例如单独跑 replay 而未先调用 `register_sqlite_capture_profile`），会按 OpenCode 的 `replay_order` 物化，而不是报错。观测状态与 crash adopt 对档案不匹配则已是 fail-closed（`:1346-1356`、`:3488-3492`）。
- 为何不阻断：`replay.rs` 物化时 `PRAGMA foreign_keys = OFF`，顺序回落只影响父表优先插入，不影响行内容；本次 OpenCode 等价性与合成档案 replay 均通过。影响面是「未注册档案的离线 replay」这一未来场景。
- 建议：对非空且无法解析的 id 返回 `ArchiveError::Corrupt`（或至少在 README 写明「replay 前须注册档案」），与观测状态的 fail-closed 语义一致。

### N4. README 对「档案切换」的措辞比实现更绝对

- 位置：`agent-memory/README.md:88`（“a profile switch re-baselines instead of diffing across profiles”）。
- 证据：同形不同 id 的切换实测为 `degraded` + `snapshot_deduplicated:observation_state_missing`（内容相同走 dedupe，不产生新基线字节，0 insert/update/delete），下一轮正常增量；完成记录 §7 描述准确。
- 为何不阻断：语义上仍是「拒绝跨档案 diff」，只是不一定产生新基线对象。
- 建议：改为“rejects the cross-profile diff and requires a new baseline (identical content dedupes to the existing baseline)”。

### N5. `-ArchiveCaptureOnly` 缺少 `-ArchiveRoot` 时是静默 no-op

- 位置：`assets/scripts/64-install-recall.ps1:116`（无条件传 flag）；`recall_onboarding.py:267-291`（`archive_root` 为空时不追加 archive 源，`build_config` 保留既有源）。
- 证据：只传 `-ArchiveCaptureOnly` 不传 `-ArchiveRoot` 时不会禁用既有 archive 源，也不报错。
- 为何不阻断：帮助文本已写明「与 `-ArchiveRoot` 同用」，且不会造成配置损坏。
- 建议：在 64 入口校验「`-ArchiveCaptureOnly` 需要 `-ArchiveRoot`（或已有 archive 源）」并给 WARN。

### N6. 本机规模用例墙钟高于完成记录参考值（内存一致）

- 位置：`agent-memory/tests/p3_sqlite_scale.rs`。
- 证据（本机，前台跑完，`cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture`，exit 0）：
  - 稳态 `1.370 s / 62-63 MiB`（插入 100/更新 100/删除 10；delta 3 KiB；解压证据为基线 9%）。
  - `--verify`@200k `5.648 s / 59 MiB`，0 mismatch。
  - `--verify`@400k `10.809 s / 109 MiB`，增长 +86%（预算 ≤100 MiB 通过），全部 budget 断言 true。
  - 对照任务给定约 1 s / 62 MiB、约 4 s / 59 MiB：内存完全一致，墙钟偏高；同机 fixture 构建耗时 591.9 s（1.6 GB 合成库），说明本机磁盘/IO 明显慢于实现者环境。
- 为何不阻断：OpenCode 路径解码后的证据与旧二进制逐项等价（delta 对象字节仅因行内墙钟 `observed_at_ms` 不同，解析后的 table/key/op/before/after 完全一致），新增仅为档案间接层；内存无退化，测试自身预算断言全部通过。
- 建议：若需严格墙钟对照，在同一台机器上用 `320941e` 构建旧二进制跑同一用例做同机对照；本次未做（避免再花 ~10 分钟构建，且证据等价已覆盖正确性）。

### N7. 会话重入的「水位以下」时间戳仍属已声明盲窗，建议在档案文档里点名

- 位置：`agent-memory/src/capture_sqlite.rs` 水位候选读取（`observe_one_table` 的 `lower = previous_value.max(session_since)`）。
- 证据：本 Verifier 的 mcode 合成库把范围外会话 `updated_at_ms` 置为 1500（低于基线水位 1600）时，重入会话不会被常规观测发现（`--verify` 可补）；置为 1800（高于水位）时正确产生 3 条 insert（会话 + 2 条既有子行）且无误报删除。
- 为何不阻断：这是模块文档已声明的盲窗（“writes at or below the watermark”），`--verify` 会纠正；对 OpenCode 与所有档案一致。完成记录 §5.3 的重入用例本身选择的是高于水位的值。
- 建议：在 README 的 capture-profile 小节补一句「`--session-since` 重入同样受水位盲窗约束，需 `--verify` 兜底」，避免接入方误以为重入必然即时可见。

---

## 已执行命令与退出码

| # | 命令（cwd） | 退出码 | 结果 |
|---|---|---|---|
| 1 | `git -C C:\wt\s45-verify diff --check 320941e ba16320` | 0 | 无空白错误 |
| 2 | `git -C C:\wt\s45-verify diff --name-status 320941e ba16320 -- agent-memory/tests` | 0 | 仅 `A tests/p3_sqlite_profiles.rs`；既有测试零修改 |
| 3 | `cargo test --locked --no-fail-fast`（`C:\wt\s45-verify\agent-memory`，`CARGO_TARGET_DIR=C:\wt\v45-work\target-new`） | 0 | **691 passed / 3 ignored / 0 failed**；`p3_sqlite_profiles` 8 passed |
| 4 | `python -I -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`（`assets\scripts`） | 0 | **73 passed** in 12.17s |
| 5 | `cargo check --locked --all-targets`（同 3） | 0 | 0 warning |
| 6 | `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture`（前台，同 3） | 0 | 1 passed；数据见 N6 |
| 7 | `cargo run -q`（独立 probe，Hermes/mcode/隔离/切换/replay/哨兵） | 0 | `PROBE OK`（详见下） |
| 8 | `equiv.exe <old-archive> <new-archive>`（旧/新二进制各自跑同一 OpenCode fixture 后解码比对） | 0 | `DECODED EVIDENCE IDENTICAL`（baselines=2、deltas=1、final seq=3、replay 7 表） |
| 9 | 旧二进制 `capture` / 新二进制 `capture` 在同一合成 OpenCode fixture 上（首次/增量/session-since/verify/replay） | 0 | 各步均 0；报告与证据比对见下 |
| 10 | 新二进制读旧证据（`320941e` 产生的 archive+state） | 0 | `skipped_unchanged`，无重基线 |
| 11 | 旧二进制读新证据（`ba16320` 产生的 archive+state） | 0 | `skipped_unchanged`，`profile_id` 被未知字段忽略（可回退） |
| 12 | `python -I u64.py`（临时 UserRoot，`r.main(argv)` 完整命令行入口 + fake task executor） | 0 | `-ArchiveCaptureOnly` 路径全通过（详见下） |
| 13 | `powershell -File parse64.ps1`（语法/参数名只读解析） | 0 | `syntax-ok`；参数含 `ArchiveCaptureOnly` |
| 14 | `python -I equiv.py`（旧/新二进制等价驱动） | 0 | 6 步（init + 4 capture + replay）全 0 |
| 15 | `grep -nF 'C:\Users' agent-memory/docs/verification/STAGE2-S45-COMPLETED.md` | 0（命中） | 第 4 行 → B1 |

## 关键证据

### 1. OpenCode 档案等价性（旧 `320941e` vs 新 `ba16320`）

- 同一合成 OpenCode 库（`session`/`message`/`part`/`session_v2`/`session_message`/`project`/`todo` + 非白名单 `credential`），分别执行：首次基线 → 修改（增会话/改会话/增 message/增 part/改 message/删 todo）→ 增量 → `--session-since`（filter_changed 重基线）→ 同参数 `--verify` → `replay`。
- 解码后证据逐项相同：基线 `baseline_digest`、每个 chunk 的 `sha256`/`raw_sha256`/`rows`/`min_key`/`max_key`、全部行内容；delta 的 `table/key/op/before/after`；collector state 的业务字段（`baseline_digest`、`known_tables`、`known_columns`、`delta_chain_*`、`verify_mismatches_total`、排除表等）；观测状态（去掉 `profile_id` 与观测时间）；`replay_sqlite` 的 7 张表全量行。
- 唯一的运行时差异均属预期噪声：delta 对象 sha（行内 `observed_at_ms` 取墙钟）、capture_id、manifest 摘要/派生 artifact 路径（含运行时根路径与上述 delta 摘要）；派生 artifact 的业务内容归一后相同，anomalies/messages/sessions 计数相同。
- 既有测试：`agent-memory/tests` 在 diff 中只有新增文件，零断言修改；`cargo test` 691 = 678（改动前）+ 5 单测 + 8 集成用例，与完成记录一致。

### 2. 残留字面量审查（`capture_sqlite.rs` / `replay.rs`）

- `capture_sqlite.rs:218-243`：`session`/`message`/`part`/`session_v2`/`session_message`/`project`/`todo`、`time_updated`、`seq`、`time_created`、`created`、`session_id` 全部集中在 `SqliteCaptureProfile::OPENCODE` 定义内（引擎从档案读取，单测锁定逐项等价）。
- `capture_sqlite.rs:2062`、`:5736`：`.unwrap_or("session_id")` 分别以 `child_scoped`（观测）与 `scoped_child`（验证）门控，运行时不可达；完成记录 §4 已列。
- `capture_sqlite.rs:2516`：`.unwrap_or("id")` 同类不可达，未入完成记录清单 → N2。
- `capture_sqlite.rs:759`：`DuplicateTracker` 的 `"data"`/`"content"` 为跨框架内容列嗅探统计，完成记录 §4 已列。
- `replay.rs:1489` 的 `["session_v2","session"]`、`row.get("id")`、`ses_` 形状检查与 OpenCode 读回投影：属 derived 层，完成记录 §4 已声明为后续工作；本次引擎级 `capture_sqlite::replay_sqlite` 与框架无关。
- 结论：无「不经档案的 OpenCode 专属采集逻辑」残留；剩余字面量均有档案定义/不可达/已声明理由。

### 3. 合成档案真实性与端到端（独立 fixture，非仓库测试数据）

- 对照 cc-switch：`hermes.rs` 的 `sessions(id TEXT PRIMARY KEY, …, last_activity_at REAL)`、`messages(id INTEGER PK AUTOINCREMENT, session_id, …, timestamp REAL)`；`mcode.rs` 的 `local_runtime_sessions(session_id, …, created_at_ms, updated_at_ms, …)`（无声明主键）、`local_runtime_messages(session_id, display_messages_json)`、`local_runtime_message_rows(id, session_id, role, data_json, created_at_ms)`、`local_runtime_message_row_migrations(session_id)`。合成档案的表名、会话键、时间列、子表关联列与之一致（`tests/p3_sqlite_profiles.rs:40-94`，本 Verifier 另建了不同内容的库复核）。
- Hermes 风格（独立库，55 行基线）：基线 → unchanged → 增量（insert 1/update 2/delete 1）→ `--verify`（0 mismatch）→ replay 与源逐行一致；凭据哨兵在 **10 个运行时根文件的原始字节与全部 zstd 解压内容** 0 命中。
- mcode 风格（独立库，`--session-since=900`）：基线 65 行（含 hidden 会话——按完成记录 §7，采集不做展示层过滤）→ 会话重入（`updated_at_ms` 越过水位）3 insert（会话 + 2 条既有子行）、0 误报删除 → 新会话+行、删行（insert 2/delete 1，盲改不计数）→ `--verify` 捕获盲改（mismatch 1、update 1，correction delta）→ replay 与源逐行一致；哨兵在 14 个文件 0 命中。
- 排除表事实：`auth_tokens`/`local_runtime_message_row_migrations`/`mcode_credentials` 仅以 `{table,row_count}` 记录，未读行内容。

### 4. 档案隔离与切换

- 同一运行时根两个源、两个档案（`probe-hermes`、`probe-mcode`）：各自 state 记录正确 `profile_id`，互不影响的 unchanged 运行通过。
- 同一源切换同形不同 id 档案：`change=degraded`、`reason=observation_state_missing`、`snapshot_trigger=snapshot_deduplicated:observation_state_missing`、0 insert/update/delete（未跨档案 diff、无重复证据字节）；下一轮 `skipped_unchanged`；再下一轮带真实修改时正常 `delta`（insert 1）。与完成记录 §3/§7 一致。

### 5. `-ArchiveCaptureOnly`

- 临时 UserRoot + fake task executor，调用 `recall_onboarding.main(['install', … '--archive-root', …, '--capture-schedule', '--archive-capture-only'])`（完整命令行入口；未触碰真实计划任务）：exit 0、无 FAIL；`recall.json` 中唯一 archive 源 `enabled: false`；`index-refresh.ps1` 含 `capture-all --config $cfg --skip-if-locked`；`index-schedule` 明细为「先采集后索引」。
- `agent-memory capture-all --config <recall.json> --plan`：exit 0，报告 `root` 解析为该 archive 源（路径归一后相等），`sources=[]`（archive 不是采集输入源，符合语义）。
- 真实 `capture-all --config`（非 plan）：exit 0，`ok=true`，root 同上，`state/capture-all.json` 落在该 root。
- `memory_search`（新二进制）：同配置下 `enabled:false` 时 `source_status` 只有 `kb`，archive 完全不参与；把同一 archive 根以 `enabled:true` 配置后，`source_status` 出现 `archive`（status `empty`）。两次退出码 4 均只因合成 vault 缺 `.meta/scripts/ask.py`（`SOURCE_ROOT_INVALID`），与 archive 无关。
- 对照：不加 `-ArchiveCaptureOnly` 时 archive 源 `enabled: true`（行为不变）。

### 6. 文档与卫生

- `git diff --check` 干净；64 脚本 PowerShell 语法解析通过且参数表含 `ArchiveCaptureOnly`；`-ArchiveCaptureOnly` 在 `install()` 签名末尾（`recall_onboarding.py:1248`），`main()` 以关键字传递（`:1486`），本次 CLI 实测未发生参数错位。
- 文档与实现一致：`INSTALL.md` / `UPGRADE.md` / `assets/references/agent-memory-archive.md` / `CHANGELOG.md` / `agent-memory/README.md` 均已同步 `-ArchiveCaptureOnly` 与 capture-profile 语义。
- 唯一不一致是 B1（真实用户路径）与 N1/N4 的措辞；均不影响行为。

---

## 总评

S4.5 的引擎参数化与 64 `-ArchiveCaptureOnly` 在代码、证据格式、检索语义和端到端行为上均通过对抗式验收；OpenCode 证据与旧二进制逐项等价，非 OpenCode 合成档案（Hermes/mcode）、隔离与切换、哨兵排除均实测成立。阻断项仅一篇新增文档中的真实用户主目录路径，改一行即可；另有 5 条注释/文档/边界语义的可选改进（N1–N5、N7）与 1 条本机墙钟偏差说明（N6）。
