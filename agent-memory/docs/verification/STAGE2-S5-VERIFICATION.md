# STAGE2-S5 独立验收报告：全文索引归档补缺（活源优先、归档补缺）

- 验收对象：隔离检出 `C:\wt\s5-verify`，HEAD=`6e5294c`，基线 `d47f526`
- 验收人：独立 Verifier（全新上下文，与实现者不同模型族）
- 日期：2026-10-31
- 方法：只读被检仓库；全部 fixture 由验收方自建于 `C:\wt\s5v-work\`（合成数据，非仓库测试 fixture）；未读取任何真实会话库、生产 Archive 或真实配置（`<user-home>\.local\share\opencode`、`.claude`、`.codex`、`.session-evidence`、`.agent-memory`、`.config\opencode` 全程未打开）；未注册/运行真实 61/64/90；未 git commit；Python 一律 `python -I`；长任务前台等待。
- 本报告是唯一写入仓库的文件（`<repo>/agent-memory/docs/verification/STAGE2-S5-VERIFICATION.md`）。

## 结论

**verdict：阻断需修复。**

基线功能与既有测试、N5 修复、JSONL 补缺回读、V1-only 覆盖、只读性、去重（常规形态）、成本对照均按声明工作。阻断项集中在 **OpenCode 增量路径的删除/恢复感知**（S5 的核心承诺在该路径不成立）、**多 sessionId carrier 的永久重复行**、以及 **行镜像物化的成本**。

| 编号 | 阻断项 | 证据来源 |
|---|---|---|
| B1 | OpenCode 一旦「本次索引时行数没有下降」，被删除会话仍是 `origin=live`、无 `deleted_at`、补缺不生效，且该命中 `memory_read` 直接失败（`REF_LOCATOR_MISSING`） | 自建 e2e、基线对照 |
| B2 | OpenCode 会话以同 id 回到活源（原时间戳）时仍为 `origin=archive` 且带 `deleted_at`，不切回活源 | 自建 e2e |
| B3 | 单个 carrier 内含多个 sessionId、其中只有部分会话消失时，仍存活的会话会被写入归档行且永不登记/清理 → 同一会话两份行（永久） | 自建 mini fixture |
| B4 | OpenCode 行镜像首次构建与缺失时回读物化按「行 × fsync」计费（实测约 7.4 ms/行）：50k 行 363 s；5k 行回读 41.7 s；随白名单载荷线性放大 | 50k 成本 fixture + 微基准 |

## 阻断项

### B1 OpenCode 删除会话/删除子行的增量索引不感知删除，核心承诺失效

**位置**
- `agent-memory/src/recall/fulltext/build.rs:683-695`：`full` 只在 `current_rows < watermark.rows`（或重建原因）时触发。
- `agent-memory/src/recall/fulltext/build.rs:719`：增量扫描条件 `WHERE {ts_column} >= ?1`，只能看到新行；删除行永不被重扫，也没有任何「活源已不存在」的对账。
- `agent-memory/src/recall/fulltext/build.rs:890-905`：`refresh_sessions` 非 `full` 时只 upsert，从不删除已消失的 `sessions` 行。
- `agent-memory/src/recall/fulltext/build.rs:505-520`：`opencode_live_sessions` = `doc_keys` 活源行 ∪ `sessions` 表 − 登记表；上面两处残留都会把已删除会话判为「活源现有」。
- `agent-memory/src/recall/fulltext/fallback.rs:872-877`：`wanted = 归档会话 − live_sessions`，因此被误判为 live 的会话不会补缺。

**命令与证据**（自建世界 `C:\wt\s5v-work\fixtures\e2e`：Claude 4 文件含 subagents、Codex `.jsonl`+`.jsonl.zst`、OpenCode V2+V1 含 V1-only 与 V1/V2 同 id；`capture-all` → 删会话/文件 → `capture-all` → `index`）

1. 首轮 `capture-all` + `index`：opencode `archive_only_sessions=1`（V1-only），正常。
2. 删除 `oc-del` 的 `session_message` + `session_v2`，**同时**给 `oc-live` 插入 2 条（模拟真实「其它会话在长，总行数不减」）→ `capture-all`（退出 0，`records_added=4`，delete delta 已提交：`session_message pk:["sm-od1"] op=delete`、`session_v2 pk:["oc-del"] op=delete`，`observed_at_ms=1793441138344`）→ `index`（退出 0）：

```
index: status=updated rows=8
  detail.archive = {"status":"ok", ... "archive_only_sessions":1, "added":0, "removed":0}
memory_search "vfy5 opencode deleted session marker kappaoc"（exact）
  → 1 hit, origin="live", 无 deleted_at, locator {"session_id":"oc-del","session_message_id":"sm-od1","pointer":"/text"}
memory_read <该 ref> → exit 5, {"ok":false,"code":"REF_LOCATOR_MISSING"}
```

3. 同一状态只加 `--rebuild` 即恢复正确：`archive_only_sessions=2, added=4`，命中 `origin="archive"`、`deleted_at="2026-10-31T10:05:38Z"`，locator `archive_fallback.deleted_at_ms=1793441138344`（与 delete delta 的 `observed_at_ms` **逐值相等**），回读逐字节等于删除前内容。说明补缺实现本身正确，失效点是增量对账。

4. 同一缺陷的子行形态：删除活源会话 `oc-live` 的一条普通消息（`sm-ol2`）并补 1 条新消息（总行数不变）→ `capture-all`（delete delta 已提交）→ `index`：`rows=9`，该条仍以 `origin=live` 返回，`memory_read` → exit 5 `REF_LOCATOR_MISSING`。

5. 基线对照（同一变异、各自独立世界）：`d47f526` 二进制表现相同（5 条死命中、`memory_read` exit 5）。**「删除行仍被 live 命中而不可读」是既有缺陷**；但 S5 声称的「已删除会话可找回（`origin=archive` + `deleted_at`）」在增量路径上不成立，且完成记录 §R5「被删子行本身在活源索引中随之消失（与活源一致）」与实测相反（行仍在，且读不了）。

**为何是阻断**：必查 #2 要求的 OpenCode 场景（删会话及子行→capture→index→`origin=archive`+`deleted_at`）只在「总行数恰好下降」这一偶然条件下成立；生产上其它会话持续增长时不会成立，且会把「不可读的死命中」当成活源命中返回。必查 #4 的「活源优先」边界亦因此把已删子行留在索引里。

**建议**：增量阶段必须做删除对账。最小改动：`refresh_sessions` 非 `full` 时也删除不在本次 `load_sessions()` 结果里的 `sessions` 行；并让活源扫描在本次观察中按「活源现有 session_message 主键集合」清理 `doc_keys` 中该 db 的活源行（可用 `sqlite_delta_batch_metas` 里已提交的 delete 键做增量删除，成本可控）；`opencode_live_sessions` 的「sessions 表」来源随之可信。修复后需补一条「删除会话/子行 + 其它会话增长（行数不下降）」的回归用例——现有 `opencode_partial_child_deletion_keeps_live_priority` 因删行导致 `current_rows` 下降触发全量重建，且对命中集合的断言为空循环，未覆盖该路径。

### B2 同 id 回到活源（原时间戳）不切回 `origin=live`

**位置**：`build.rs:683-695` + `build.rs:719`（恢复的行时间戳早于 watermark.max_ts 时不重扫）+ `fallback.rs:853-887`（`opencode_live_sessions` 对已登记会话只看活源 `doc_keys`，而该会话没有活源行；`pending` 又因 `archive_rows_present` 跳过）。

**命令与证据**（e2e，`oc-del` 已处于 archive 状态）：

```
恢复：INSERT session_v2('oc-del') + session_message('sm-od1', time_created=300, 原文)
capture-all（退出 0）→ index（退出 0）:
  opencode: status=unchanged rows=9, archive_only_sessions=2, removed=0
  memory_search "kappaoc" → 1 hit, origin="archive", deleted_at="2026-10-31T10:05:38Z", locator 带 archive_fallback
```

把同一条消息的 `time_created` 改为 306（前移过 watermark）后重跑：`status=updated, archive_only=1, removed=1`，命中变 `origin="live"` 且无 `deleted_at`。即恢复是否生效取决于新行时间戳是否越过 watermark。

**为何是阻断**：直接违反必查 #2「恢复到活源 → 只剩 `origin=live` 一份」与 README「An OpenCode session deleted and re-created under the same id answers from its live rows」。后果是把**活着的会话标成已删除**（带 `deleted_at`），用户在结果里看到伪造的删除时间；内容还能从归档读出，所以不是数据丢失，但是状态与归因错误，且会长期存在（直到该会话产生新消息或发生全量重建）。

**建议**：B1 的删除对账同时解决「登记表与活源会话表的对账」：每轮用活源 `session_v2` 主键集合与登记表求交，凡活源仍存在的会话立即 `remove_archive_session` + `forget`；恢复的旧行内容由「登记 → 活源」切换时的补扫（按 session_id 定向重扫，而非按时间水位）补入。

### B3 单个 carrier 含多个 sessionId 时，活源会话被补入归档行且永不清理（永久重复）

**位置**：`fallback.rs:796-832`（carrier 只按 `pending` 是否为空决定是否重建，但重建后 `scan_jsonl_reader` 扫描**整份** carrier）+ `build.rs:1376-1460`（`scan_jsonl_reader` 对每条记录都 `insert`，不按 session 过滤）+ `fallback.rs:826-831`（只把 `pending`（wanted）会话写进登记表）。

**命令与证据**（自建 `C:\wt\s5v-work\fixtures\multi`）：

```
claude/proj/shared.jsonl 含 sess-a 与 sess-b 两条记录；claude/proj/a.jsonl 只含 sess-a
capture → index → 删除 shared.jsonl → index
doc_keys: [('sess-a','archive',1), ('sess-a','live',1), ('sess-b','archive',1)]
archive_sessions: [('sess-b',)]        ← sess-a 的归档行没有登记
memory_search "needle multi"（exact）→ 3 hits:
  ('live','sess-a'), ('archive','sess-a'), ('archive','sess-b')
再连续 index 两轮：doc_keys 不变，重复行永久存在
```

**为何是阻断**：必查 #3 的硬约束「两边都有的会话绝不出现两次」被破坏，且因为未登记，`registered.difference(wanted)` 永远不会删除它；`group_by_session`/hybrid 会话去重只能把两条折叠成一条展示，`session_hits` 计数仍被污染。触发条件是「单个 carrier 内含 ≥2 个 sessionId 且只有部分会话消失」；Claude Code 正常一文件一 session，故真实概率低，但活源构建器规则本身（逐行 `sessionId`）允许该形态，且一旦发生是**永久**重复。

**建议**：`apply_jsonl` 重建 carrier 时只插入 `pending` 集合内的会话（扫描器加 session 白名单参数），或把「本次实际写入归档行的会话」全部 `remember_archive_session`，使下一轮可被清理。二者取其一即可闭合不变量。

### B4 行镜像构建/回读物化成本：每行一次 fsync，50k 行 363 s

**位置**：`fallback.rs:1439-1484`（`replay_state_keep_deletes` 对基线与 delta 行逐条 `insert_mirror_row`，**全程没有显式事务**）+ `fallback.rs:1342-1368`（`insert_mirror_row` → `conn.execute`，SQLite 自动提交 = 每行一次事务/fsync）+ `fallback.rs:1828-1904`（回读缺镜像时把整份状态物化到临时库，同一逐行路径）。

**命令与证据**

| 场景（50 会话 × 1000 条 = 50 000 行 `session_message`） | 耗时 | 峰值 RSS |
|---|---|---|
| 首次 `index`（1 个 archive-only 会话，需建镜像） | **363 242 ms** | 87.6 MB |
| 随后 `index`（增量，镜像已存在） | 296 ms | 13.6 MB |
| 无待补会话 `index`（S5 二进制） | 6 465 ms | 30.6 MB |
| 同上 fixture（`d47f526` 基线二进制） | 5 549 ms | 28.7 MB |

- 镜像体积：10 403 840 B（≈ 白名单载荷 10.4 MB）；`rows/<baseline_digest>.sqlite`，无 `.building.*` 残留。
- 规模曲线（同样的「先采集、删一个会话、再采集」流程，首次 index）：1 000 行 13.9 s、5 000 行 42.1 s、10 000 行 78.0 s、20 000 行 147.6 s → **≈7.4 ms/行，与总行数线性**，与待补会话数无关。
- 回读：删掉镜像后 `memory_read` 一个归档 ref（5 000 行库）= **41 720 ms**（同路径物化整库）。
- 归因实验（Python sqlite3，2 000 次同构 `INSERT OR REPLACE`）：真自动提交 13 520.8 ms（6.76 ms/行）vs 单事务 9.2 ms（0.005 ms/行），相差约 1 470×，与 7.4 ms/行吻合。
- 无待补会话时 `index` 与基线同量级（+16.5%，含清单指纹计算），该条达标。

**为何是阻断**：完成记录自己写明生产 OpenCode 白名单载荷「约 GB 量级」。按 7.4 ms/行，GB 级载荷的首次镜像构建与「镜像缺失时的一次回读」是**小时级**；而镜像只在有待补会话时构建，恰恰是「有会话消失」这一最需要可用的时刻。计划任务（30/45 分钟一次）会在该窗口内长时间占用并可能被超时打断。

**建议**：`replay_state_keep_deletes` 用单个事务包住基线行与全部 delta 行（`apply_batches` 已有事务，照抄即可）；`evidence_db` 的临时物化同样包事务。改完预计回到秒级。可选：`PRAGMA journal_mode=WAL/synchronous=NORMAL`（缓存可重建，崩溃丢弃可接受）。

## 非阻断问题

| 编号 | 问题 | 位置 / 证据 | 影响与建议 |
|---|---|---|---|
| N1 | 回读成功路径遗留空临时目录 | `fallback.rs:2051-2064` 先 `create_dir_all(<tmp>/s5-readback-<pid>-<ts>)`；`fallback.rs:1995`/`2001` 用 `std::fs::remove_file(parent)` 删目录，在 Windows 上必然失败且错误被吞。证据：一次 MCP/CLI 回读后 `<user-home>\AppData\Local\Temp` 下多出 `s5-readback-<pid>-<ts>`（本轮累计 13 个空目录） | 临时文件本身已删（`owned` 分支正常），但空目录无界增长。改 `remove_dir`（或成功路径也清目录） |
| N2 | CHANGELOG 数值自相矛盾 | `CHANGELOG.md` S5 条目内同时有「**703 passed** / 3 ignored」+「新增 12 条」与「测试 12→14 条 … 全量 **705 passed**」 | 同一条目内两个终值；建议只留 705/14（本轮实测 705/3/0） |
| N3 | 文档断言与实测不符 | `agent-memory/README.md`「An OpenCode session deleted and re-created under the same id answers from its live rows」；`docs/verification/STAGE2-S5-COMPLETED.md` §R5「被删子行本身在活源索引中随之消失（与活源一致）」；`assets/references/agent-memory-archive.md` 4.8「已删除会话的补缺命中仍为 origin=archive」 | 三处均只在「行数下降/全量重建」路径成立（见 B1/B2）。修复 B1/B2 后自然一致；否则应改写为附带条件 |
| N4 | `coverage.sessions` 计数包含已从活源删除的会话 | `build.rs:890-905`（只 upsert）+ `coverage` 取 `sessions` 表：删除 `oc-del` 后 `detail.coverage.sessions=4`，活源实际 3 个会话 | 指标误导；随 B1 一并修 |
| N5 | `index` 的 `detail.archive.status` 只有 `ok`/`failed`，无法区分「没有归档源」与「归档源不可用」 | 无归档源时根本不输出 `detail.archive`（`build.rs:186-199`）——行为正确；但 `FallbackContext::unavailable` 与「归档根未初始化」都归为 `failed`，`reason` 文本可区分 | 可接受，仅记录 |
| N6 | 归档 provider 启用后的重复回答未做自动化防护 | `index` 报告 `archive: skipped_disabled`（实测），64 帮助与参考文档改为「保持停用」；若用户手工 `enabled:true`，独立 provider 与补缺行会同会话双答 | 与设计一致（文档化停用），非缺陷；如需硬防护可在配置校验期拒绝 |

## 已通过项（逐条对应必查）

**必查 1 — 测试与既有断言**
- `cargo test --locked --no-fail-fast`（`C:\wt\s5-verify\agent-memory`）→ **exit 0，705 passed / 3 ignored / 0 failed**（32 个 test bin）。
- Python：`assets/scripts` 下 `python -I -m unittest discover -s . -p "test_recall_*.py"` → **exit 0，73 tests OK**。
- `git diff d47f526 6e5294c -- agent-memory/tests` → 只有一个新增文件 `tests/recall_archive_fallback.rs`（1 106 行，纯 `+`），**既有测试文件零改动**。
- `git diff --check d47f526 6e5294c` → exit 0（干净）。
- 新增行无真实用户路径：`git diff` 中唯一相关命中是 `STAGE2-S5-COMPLETED.md` 里的 `<user-home>` 占位；无任何用户主目录绝对路径（Windows 用户目录、`/home`、`/Users` 形态）。

**必查 2 — 自建合成 fixture 端到端（与仓库 fixture 不同数据：`vfy5 …` 文本、4 个 Claude 文件含 subagents、Codex 普通/`.zst`、OpenCode V2+V1 含 V1-only 与 V1/V2 同 id）**

| 场景 | 结果 |
|---|---|
| Claude 删除转录 → exact/hybrid | `origin=archive`，`memory_read` 文本逐字节等于原文，`range_sha256` == 命中 fingerprint |
| Claude subagents（活源） | `origin=live`，回读逐字节一致 |
| Claude `append_range` 增长后再删除 | 两条命中均 `origin=archive`，`context_before=1` 回读得到**跨两次采集拼接边界**的两段（record_index 0、1），内容正确 |
| Codex 删除 `.jsonl` / 仅 `.jsonl.zst` | 均 `origin=archive`，回读逐字节一致（`.zst` 走 `decoded` 对象） |
| OpenCode 删除会话（行数下降触发重建） | `origin=archive`，`deleted_at` 存在且 **= delete delta `observed_at_ms`**（1793441138344 → `2026-10-31T10:05:38Z`），回读逐字节一致 |
| OpenCode V1-only（文本 + 工具 part） | `origin=archive`，ref `schema_gen=sqlite-v1`，locator `session_id+message_id+part_id`，回读逐字节一致 |
| V1/V2 同 id | 只命中 V2 副本（`origin=live`），V1 副本文本零命中 |
| 恢复到活源（Claude） | 唯一命中 `origin=live`，`archive_only_sessions→0`，`removed=1` |
| OpenCode 删除会话 + 行数不下降 / 恢复原时间戳 | **B1 / B2（阻断）** |

**必查 3 — 重复与身份（常规形态）**
- 连续两轮 `index`：全部命中 `hit_id` 完全一致；缓存根整删后重建：命中集合（含 `hit_id`、`origin`、`deleted_at`）**完全相同**。
- 直接查索引库：三个源均无「同一 session_id 同时有 live 与 archive 行」的混合（除 B3 的构造场景）。
- `group_by_session=true`：每会话一条，`session_hits` 计数合理；hybrid 结果 `hit_id` 唯一（会话级去重生效）；`hybrid + group_by_session` 返回 exit 2（既有契约，行为与基线一致）。
- 身份重建：手改索引 `meta.fallback.rule_version` → `rebuild_reason=fallback_rule_changed` 并重补；改归档根 → `fallback_archive_changed`；新采集（新 manifest）只更新指纹并增量（`added` 只含新待补会话）。

**必查 4 — 活源优先边界（除 B1 已述）**
- 只删活源会话的部分子行（delete delta 已提交）→ 会话仍 `origin=live`、无 `deleted_at`、不产生归档行、不进 `archive_sessions`（被删子行本身仍作为死命中留存，见 B1）。
- 会话本体在活源、消息全删（补 2 条其它会话消息以避免全量重建）→ 仍 `origin=live`、无归档行、不计入 `archive_only_sessions`（不误判为 archive-only）✓。

**必查 5 — 只读与缓存**
- MCP（`agent-memory mcp`，JSON-RPC 行协议）`tools/list` + `memory_search` + `memory_read`（归档 ref）全程后，`<cache_root>` 逐文件 `(size, sha256)` **无变化**（16 文件）。
- CLI `memory_search`/`memory_read` 前后同样不变；整删 `<cache_root>` 后重建结果一致（fake embedding）。
- 回读的系统临时文件被删除（`owned` 分支）；遗留空目录见 N1。
- 无待补会话时 `index` **不创建** `archive-inventory/rows/`（实测该目录不存在，只有 `current.json` + `sessions/`）；有待补会话时只生成 `rows/<digest>.sqlite`，无 staging 残留。
- 伪造 `archive_fallback.record_sha256` → `REF_LOCATOR_MISSING`（fail-closed）；代际/键不匹配 → `INVALID_REQUEST`；`--strict-snapshot` 对补缺 ref → `DB_UNAVAILABLE`。

**必查 6 — 成本**：见 B4 表格（无待补会话时 6.47 s vs 基线 5.55 s，同量级；镜像构建与回读物化成本不达标）。

**必查 7 — N5**
- 同一归档、同一 SQLite delta 记录：`d47f526` → `read --sha256` exit 3（`corrupt archive state: record …`）；`6e5294c` → exit 0，字节可解压/切片，内容就是 delta 行 JSON，且 `sha256(stdout)==<record sha>`。
- 同一归档的 JSONL 记录：两个二进制输出**完全相同**（`kind=record`、字节数一致、哈希自校验通过），切片行为不变。

**必查 8 — 文档与实现**
- `README.md`「Archive fallback」小节、`agent-memory-archive.md` 4.8、`64-install-recall.ps1` 帮助、`CHANGELOG.md` 与实现总体一致（缓存路径、`origin`、`archive_sessions`、重建原因、V1、N5、独立 provider 停用）。
- 与实测冲突的三处断言见 N3；数值不一致见 N2。
- `details.archive`、`coverage.archive_only_sessions/archive_root` 实测存在且语义正确（除 N4 的 `sessions` 计数）。

## 已执行命令与退出码

| 命令 | 工作目录 | 退出码 / 结果 |
|---|---|---|
| `cargo test --locked --no-fail-fast` | `C:\wt\s5-verify\agent-memory` | 0；705 passed / 3 ignored / 0 failed |
| `python -I -m unittest discover -s . -p "test_recall_*.py"` | `C:\wt\s5-verify\assets\scripts` | 0；73 tests OK |
| `git diff d47f526 6e5294c -- agent-memory/tests` | `C:\wt\s5-verify` | 仅新增文件，1 106 行 `+` |
| `git diff --check d47f526 6e5294c` | `C:\wt\s5-verify` | 0（干净） |
| `git archive d47f526 \| tar -x` + `cargo build --locked --bin agent-memory` | `C:\wt\s5v-work\baseline-src` | 0（基线二进制，用于 N5 与成本对照） |
| `agent-memory init/capture-all/index`（自建 e2e 世界，含 append 增长、删文件、删会话、删子行、恢复） | `C:\wt\s5v-work\fixtures\e2e` | 均 0（除刻意构造的失败读） |
| `agent-memory memory_search/memory_read`（exact + hybrid，14 个 needle） | 同上 | 0；归档 ref 回读逐字节一致 |
| `memory_read <已删除会话命中>`（增量路径） | 同上 | **5** `REF_LOCATOR_MISSING`（B1） |
| `memory_read <已删除子行命中>`（增量路径） | 同上 | **5** `REF_LOCATOR_MISSING`（B1） |
| `memory_read <恢复后仍标 archive 的命中>` | 同上 | 0（内容可读，状态错误，B2） |
| `agent-memory mcp`（initialize/tools/list/tools/call ×2） | 同上 | 0；cache 根哈希不变 |
| 伪造 ref / 代际不匹配 / `--strict-snapshot` | 同上 | 5 / 2 / 2（均 fail-closed） |
| `read --sha256 <SQLite delta 记录>`（s5 与基线各一次） | `C:\wt\s5v-work\fixtures\e2e\archive` | 0（s5）/ 3（基线，N5 缺陷复现） |
| 50 000 行成本 fixture（首建/增量/无待补对照/读回退） | `C:\wt\s5v-work\fixtures\cost-*` | 0；耗时见 B4 |
| 规模曲线 1k/5k/10k/20k | `C:\wt\s5v-work\fixtures\scale2-*` | 0；≈7.4 ms/行 |
| 微基准：sqlite3 2 000 次插入（自动提交 vs 单事务） | `C:\wt\s5v-work\logs` | 13 520.8 ms vs 9.2 ms |
| 多 sessionId carrier mini fixture | `C:\wt\s5v-work\fixtures\multi` | 0；重复行复现（B3） |
| 基线死命中对照（两二进制、各自独立世界） | `C:\wt\s5v-work\fixtures\deads-*` | 均 exit 5（既有行为） |

## 备注

- 未验证：真实数据评估（S5 验收线 exact 0.864 / 合并 1.000）属生产根验证；`assets/dist` 未重建（完成记录声明由 orchestrator 统一执行）；真实 61/62/64/90 未运行。
- 复现素材全部在验收方私有目录 `C:\wt\s5v-work\`（`scripts/mkfixture.py`、`scripts/probe.py`、`scripts/cost.py`、`fixtures/*`、`logs/*`），不含任何真实会话数据；被检仓库除本报告外未被写入。
