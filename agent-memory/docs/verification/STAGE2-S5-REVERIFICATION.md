# STAGE2-S5 返工 2 独立复验报告：全文索引归档补缺（B1–B4 与 N1–N4）

- 复验对象：隔离检出 `C:\wt\s5-reverify`，HEAD=`b378a42`；返工 diff `8fd47b5..b378a42`；基线 `d47f526`
- 上一轮验收报告（先完整阅读）：`agent-memory/docs/verification/STAGE2-S5-VERIFICATION.md`（verdict：阻断需修复，B1–B4）
- 复验人：独立 Verifier（全新上下文，与实现者 GLM 不同模型族）
- 日期：2026-10-31
- 方法：只读被检仓库（只执行 `cargo test`/`cargo build`，未改源码、未 commit）；全部 fixture 由复验方自建于 `C:\wt\s5r2-work\`；未读取任何真实会话/数据库/生产配置（`<user-home>\.local\share\opencode`、`.claude`、`.codex`、`.session-evidence`、`.agent-memory`、`.config\opencode` 全程未打开）；未注册/运行真实 61/64/90；Python 一律 `python -I`；长任务前台等待。
- 本报告是唯一写入目标仓库的文件（`C:\wt\stage2-s5\agent-memory\docs\verification\STAGE2-S5-REVERIFICATION.md`）。

## 结论

**verdict：通过。**

上一轮的 B1–B4 四项阻断在自建世界里按原复现步骤重跑后**全部修复**：删除对账（会话级 + 子行级）、回活切换、多 sessionId carrier 白名单、镜像单事务与可重建缓存调优均按声明工作；N5、JSONL/V1 补缺回读、只读性、无待补不建镜像、MCP 不写缓存等回归项未回退；测试与仓库卫生达标。

没有功能性阻断。新对抗点中**行级对账的固定成本**在 22 万行规模为 **4.10–4.71 s（中位 4.28 s）**，相对 `d47f526` 基线 **0.334–0.379 s（中位 0.357 s）约 12×**，超过本轮给出的「>3×」判定线；绝对增量约 3.9 s、单次仍 <10 s，且不影响正确性，故列为 **non_blocking（需后续优化）**。另有 N1 未真正修复、N4 未修复（`coverage.sessions` 仍含归档会话）及若干文档/边界项，均非阻断。

| 编号 | 上一轮问题 | 本轮结论 | 关键证据 |
|---|---|---|---|
| B1 | 增量删除不感知（会话/子行） | **已修复** | 删会话+其它会话增长（行数不降）→ 同轮 `origin=archive` + `deleted_at`，回读逐字节一致；删子行+补行（行数不变）→ 该行零命中、无死命中 |
| B2 | 同 id 回活源（旧时间戳）不切回 | **已修复** | `time_created=300`（水位 401）恢复 → 唯一命中 `origin=live`、无 `deleted_at`；连续两轮稳定 |
| B3 | 多 sessionId carrier 产生永久重复 | **已修复** | 删 `shared.jsonl` 后 `sess-a` 仅 live 1 行、`sess-b` archive 1 行；连续 3 轮不变 |
| B4 | 镜像构建/回读物化逐行 fsync | **已修复** | 50k 行首建 **8.742 s / 83.5 MB**（原 363 s）；缺镜像回读 **1.464 s / 76.7 MB**（原 41.7 s） |
| N1 | 回读成功路径遗留空临时目录 | **未修复** | 缓存镜像命中（`owned=None`）时仍创建临时目录且不清理；实测 `s5-readback-*` 18→19 |
| N2 | CHANGELOG 数值自相矛盾 | **部分修复** | 703 已标「初版」，但「最终值见下方返工条目」指向 705；实际终值 710 不在 CHANGELOG |
| N3 | 文档断言与实测不符 | **基本修复，新增一处不符** | B2/B1 相关三处断言已成立；4.8「`coverage.sessions` 与活源一致」不成立（见 N4） |
| N4 | `coverage.sessions` 含已删会话 | **未修复** | 删除 `oc-del` 后活源 V2=4，`coverage.sessions=6`（4 live + 2 archive），`index.detail.sessions=4` |

## 必查 1：测试与既有断言

- `cargo test --locked --no-fail-fast`（`C:\wt\s5-reverify\agent-memory`）→ **exit 0，710 passed / 3 ignored / 0 failed**（32 个 `test result:` 组，含 5 个 doc-tests）。
- Python：`python -I -m unittest discover -s . -p "test_recall_*.py"`（`assets/scripts`）→ **exit 0，73 tests OK**。
- `git diff d47f526 b378a42 -- agent-memory/tests` → 只有新增文件 `tests/recall_archive_fallback.rs`（1465 行，纯 `+`），**既有测试文件与断言零修改**；返工新增 5 条用例（19 = 14 + 5）。
- `git diff --check d47f526 b378a42` → **exit 0**；`git status --short` → 干净。
- 新增行无真实用户路径：对 diff 检索 Windows/Unix 用户主目录绝对路径形态与用户名均无命中；文档只使用 `<user-home>` 占位。

## 必查 2：B1–B4 原样复现（自建世界）

世界：`C:\wt\s5r2-work\worlds\e2e`（Claude 4 文件含 subagents、Codex `.jsonl`/`.jsonl.zst`、OpenCode V2+V1 含 V1-only 与 V1/V2 同 id；文本 needle 全部为合成串 `rv2 …`）。archive 源 `enabled:false`（补缺仍生效）。

### B1 删除对账

1. 首轮 `init` → `capture-all` → `index`：OpenCode `archive_only_sessions=1`（V1-only），`oc-del` 命中 `origin=live`，`memory_read` exit 0。
2. 删除 `oc-del`（`session_message` + `session_v2`），同时给 `oc-live` 插入 2 条（总行数 6→7，**不下降**）→ `capture-all`（exit 0）→ `index`（exit 0，`status=updated`）：
   - 命中 1 条：`origin=archive`，`deleted_at=2026-10-31T11:07:58Z`；
   - `memory_read` exit 0，文本逐字节等于删除前；locator `archive_fallback.deleted_at_ms=1793444878234`，与 archive manifest 中该 delete delta 的 `observed_at_ms` **逐值相等**；
   - `doc_keys` 中 `oc-del` 仅 1 条 `archive` 行；
   - 增长会话的新行 `origin=live`、回读 exit 0；`index.detail.sessions=4`，等于活源 `session_v2` 行数。
3. 子行删除（总行数不变）：删除 `oc-live.sm-olc` 并补 1 条 → capture → index（updated）：
   - 被删行的 needle **0 命中**（上一轮为 live 命中 + `memory_read` exit 5 `REF_LOCATOR_MISSING`）；
   - 同会话其它消息仍 `origin=live`、回读 exit 0；`archive_only_sessions` 不增加。
4. 连续两轮 no-op `index`：`status=unchanged`，命中集合与 `hit_id` 不变。

结论：B1 在「行数不下降」的增量路径成立；L2 起「被删行仍 live 命中且不可读」的既有缺陷已闭合。

### B2 回活切换

在 `oc-del` 已为 archive 状态后，恢复 `session_v2` 与 `session_message('sm-od1', time_created=300)`（300 ≤ 当时水位 401）→ capture → index：

- 仅 1 条命中，`origin=live`，**无 `deleted_at`**；
- `doc_keys` 中该会话只剩 `live` 1 行（归档行与登记项均删除）；
- `archive_only_sessions` 2→1；随后两轮 `index` 均 `unchanged`，仍 1 条 live。

定向重扫 vs 全量重建：独立世界 `e2e-rescan` 恢复 2 条原时间戳消息，增量产物与随后 `index --rebuild` 的 `doc_keys⋈units`（doc_key/origin/session/message/digest/title/session_kind/ts）**10 行完全相同**（`only_incremental=0`、`only_rebuilt=0`）。

### B3 多 sessionId carrier

世界 `multi`：`shared.jsonl` 含 `RV2-sess-a`/`RV2-sess-b`，`solo.jsonl` 只含 `RV2-sess-a`。首轮 `sess-a` live 2 行。删除 `shared.jsonl` → capture → index：

- `RV2-sess-a`：仅 `live` 1 行（solo），**0 条 archive 行**；
- `RV2-sess-b`：`archive` 1 行；
- `shared-a` needle 0 命中（被删 carrier 副本没有以归档行复活）；`solo-a` live；`shared-b` archive；
- 连续 3 轮 `index`：计数不变、`status=unchanged`、无重复。

### B4 镜像成本（50 000 行）

世界 `b4-50k`：50 会话 × 1000 条 = 50 000 行；删 1 会话后 capture，再 index（首次镜像构建）。

| 场景 | 上一轮 | 本轮实测 | 峰值 RSS |
|---|---|---|---|
| 首次 `index`（镜像构建 + 1000 条归档单元） | 363 242 ms | **8.742 s** | 83.5 MB |
| 紧随其后的 no-change `index` | 296 ms | 4.182 s | 24.2 MB |
| 删镜像后回读一个归档 ref（临时物化） | 41 720 ms | **1.464 s** | 76.7 MB |
| 纯 50k 全量索引（对照） | — | 5.322 s | 25.9 MB |

镜像 6 045 696 B；`rows/` 无 `.building.*` 残留；缺镜像回读后 `rows/` 未被重建（回读只写临时文件），`readback_ok=true`、文本逐字节一致；归档命中 `origin=archive`、`deleted_at=2026-10-31T11:16:42Z`。

## 必查 3：新对抗点

### 3.1 行级对账成本（20 万+ 单元行，无变化增量 index）

fixture：220 会话 × 1000 条 = **220 000** 条 `session_message`（每条约 1 个文本单元，与生产「约 22 万索引行」同量级），WAL。两个二进制各自独立 cache，先各自全量建索引，再**交替**各跑 3 次 no-change 增量（同一时段另有非本复验的进程占用 CPU，交替测量以抵消漂移）。

| 场景（debug 构建，与上一轮基线 profile 相同） | HEAD `b378a42` | 基线 `d47f526` | 比值 |
|---|---|---|---|
| 无变化增量 `index`（含 archive 源） | 4.714 / 4.282 / 4.103 s，中位 **4.282 s** | 0.379 / 0.334 / 0.357 s，中位 **0.357 s** | **12.0×** |
| 峰值 RSS | 13.9 MB | 12.4 MB | — |
| 首次全量 `index`（冷 cache） | 48.387 s | 43.159 s | 1.12× |
| 去掉 archive 源（只留对账 + sessions 重建） | 3.188 / 3.255 / 3.323 s，中位 3.255 s | 0.357 s | **9.1×** |

- 归因：去掉 archive 源后仍约 3.2 s → 约 2.9–3.9 s 来自新增的逐已索引行 `EXISTS` 主键探测（`build.rs:807-850`）与 `sessions` 每轮重建，而不是 S5 既有 inventory。
- 是否达标：**未达标**。结果命中本轮「>3× 基线」判定线（12×）；绝对增量 3.9 s、单次 <10 s，且计划任务 30/45 分钟一次，运营上可接受。列 **non_blocking**，建议按「已提交 delete delta 键」做增量删除，或一次性载入主键集合做内存 diff。若项目把 3× 当作硬门（不接受绝对成本豁免），本条应升级为 blocking；本报告按绝对影响判非阻断。
- 完成记录的「线性且相对水位扫描很小」不准确：增量 run 的时间几乎全部来自该探测。

### 3.2 对账与并发写 / 中途崩溃

世界 `conc`（220k 行，WAL），另一连接持续「删最旧 + 插最新」同一会话（时间戳单调递增、高于水位），`index` 运行期间并发写：

- 两轮 `index`（写线程活跃）均 **exit 0**（3.09 s / 2.83 s）；
- 写线程共 **476 次删除 + 476 次插入**；
- 停止写入后再 `index`（exit 0）：该会话 **live=indexed=400，逐 id 集合相等**（missing=0、extra=0）；
- 抽样 5 条被删消息：全部 **0 命中**；抽样 10 条存活命中：`memory_read` 全部 exit 0（0 失败）。

崩溃恢复：在增量 `index` 运行中 kill 进程，cache 留下 `fts.sqlite-journal`；下一轮 `index` exit 0，`status=updated`、`rebuilt=false`、`added=10`、`removed=0`，该会话 live/indexed 集合相等（missing=0、extra=0）。

结论：并发删除不会误删仍存活的行；停止写入后无死命中；对账中途被杀后下一轮从事务回滚恢复并补齐差异。残余窗口：某行在「探测返回存在」之后、提交之前被另一连接删除时，本轮会留下一条死命中，下一轮清理；这是无快照对账的固有窗口（代码路径可核实），未在本轮观察到持久化死命中。

### 3.3 `sessions` 每轮重建的依赖字段

世界 `meta`（`oc-worker` 标题 `ocsr-`、`oc-sub` 带 `parent_id`、`oc-live`、`oc-both`、`oc-del`；V1-only `oc-v1only`）：

- `session_kinds=["worker"]` 首轮与 no-op 轮均只返回 `oc-sub`/`oc-worker`；`interactive` 返回其余；重建前后集合不变。
- 把 `oc-live` 标题改为 `ocsr-renamed-live` 后 `index`（updated，1 add/1 remove）：exact 与 hybrid 的 `session_kinds=["worker"]` 均命中 `oc-live`，标题更新为 `ocsr-renamed-live`，`interactive` 过滤不再命中；再一轮 no-op 后保持（`update_session_columns` 同时更新 `units` 与 `conversation_units`）。
- 缺省 hybrid：6 条结果对应 6 个不同 session，无同会话重复；`session_hits` 分别为 1/2；`group_by_session=true` 每会话 1 条、计数一致。
- `index.detail.sessions` 等于活源 `session_v2` 行数（删除后 4）。
- 例外：`coverage.sessions` 仍为 6（4 live + 2 archive），见 N4。

### 3.4 `journal_mode=OFF` 的作用域

- 代码只有两处调用 `tune_rebuildable_cache`：`fallback.rs:1174`（镜像 staging）与 `fallback.rs:1900`（回读临时物化）；`fts.sqlite` 走 `open_writable`，未设置 journal 模式。
- 实测：`cache/live/opencode/fts.sqlite` → `PRAGMA journal_mode=delete`；活源 `opencode.db` → `delete`；archive 证据以对象分块保存，无独立证据 sqlite 被改写。
- 缓存镜像被重新打开后报告 `journal_mode=delete`（OFF 不是持久模式）；`apply_batches` 的小事务仍用默认回滚日志（`fallback.rs:1541` 起）。
- 结论：OFF 仅作用于可删除重建的镜像/临时物化，未泄漏到活源全文索引或证据库。

## 必查 4：回归

| 项 | 结果 |
|---|---|
| Claude 删除转录 → archive 回读 | 1 命中 `origin=archive`，`schema_gen=claude-jsonl-v1`，`memory_read` exit 0，文本逐字节一致 |
| Codex 删除 `.jsonl` | 1 命中 `origin=archive`，回读 exit 0，文本一致 |
| Codex 仅 `.jsonl.zst` | 1 命中 `origin=archive`，回读 exit 0，文本一致 |
| OpenCode V1-only 文本/工具 | 均 `origin=archive`、`schema_gen=sqlite-v1`，回读 exit 0；工具单元返回整块投影（与设计一致） |
| V1/V2 同 id | V1 副本 needle **0 命中**（以 V2 为准） |
| 无待补会话不建镜像 | 两轮 index 前后 `archive-inventory/rows` 均不存在，`archive_only_sessions=0` |
| MCP 只读 | `initialize`/`tools/list`（2 tools）/`memory_search`/`memory_read` 全程 exit 0；`<cache_root>` 逐文件 (size, sha256) 无变化 |
| N5 SQLite delta record 回读 | HEAD exit 0、`sha256(stdout)==record sha`；`d47f526` exit 6（integrity check failed） |
| 伪造/代际不匹配 | 上轮已验、本轮未改相关代码；未回退 |
| 归档命中身份 | `range_sha256` 与命中 fingerprint 一致，`archive_fallback` locator 校验照常 |

## 新发现

### blocking

无。

### non_blocking

| 编号 | 位置 | 证据 | 影响与建议 |
|---|---|---|---|
| P1 | `build.rs:807-850`、`1014-1026` | 220k 行无变化 index：中位 4.282 s vs 基线 0.357 s = **12.0×**；去掉 archive 源仍 9.1× | 超过本轮 3× 判定线，但绝对 <10 s、不影响正确性。建议改为按已提交 delete delta 键删除，或一次性载入主键集合做内存 diff，并把文档成本描述改为「O(索引行数)，220k 行约 3–4 s」 |
| N1 | `fallback.rs:2013-2016` | 命中缓存镜像（`owned=None`）时 `cleanup_readback_temp` 不执行；实测一次 OpenCode 归档回读后 `s5-readback-*` 目录 18→19（新增 `s5-readback-13996-…`）；镜像缺失路径（`owned=Some`）正常清理 | 每次缓存命中回读遗留一个空目录，无界增长（文件本身已删）。把 `cleanup_readback_temp(&temp, &temp_dir)` 移到成功路径末尾无条件执行；`NotFound` 已被忽略 |
| N4 | `coverage.rs:70-73`、`fallback.rs:1601-1612` | 删除 `oc-del` 后活源 V2=4，`coverage.sessions=6`，索引 `sessions` 表 6 行（含 `oc-del`、`oc-v1only`），`archive_sessions`=2；`index.detail.sessions=4` | 指标把归档会话算作 `sessions`，与 4.8 新句「与活源一致」冲突（文档见 N3）。不改语义则应改文档；要「活源会话数」则排除 `archive_sessions`。返工测试只断言 `sessions >= 2`，覆盖不到 |
| N3 | `assets/references/agent-memory-archive.md:348`、`STAGE2-S5-COMPLETED.md:181` | 同上：`coverage.sessions` 实测 6 ≠ 活源 4；完成记录称「N4 与活源一致」 | 文档与实现不一致，建议按 N4 的决定修正；上轮 README/§R5/4.8 的另三处断言已由 B1/B2 实测支持 |
| N2 | `CHANGELOG.md:14,16` | 703 标「初版…最终值见下方返工条目」，但下方返工 1 条目写 705；实际终值 710 只在 `STAGE2-S5-COMPLETED.md` | 同一条目仍给不出最终值；建议追加返工 2 的 710/19 条，或把指针改为完成记录 |
| NB-5 | `fallback.rs:1150-1162` | 崩溃后旧 pid 的 `.building.<pid>` staging 不会被后续 run 回收（代码核实；本轮未注入镜像构建崩溃） | 只占磁盘、不影响正确性；建议创建前清理同 digest 的 `.building.*`，或纳入启动清理 |
| NB-6 | `build.rs` 增量扫描 `WHERE {ts_column} >= ?1`（既有模型） | 在已索引 220k 世界上向 live 会话插入 2 条 `time_created` 低于全局 `max_ts` 的新行，连续 `index` 均 `unchanged`，索引始终缺这 2 行（`missing=['sm-q-050377','sm-q-050378']`，`watermark.max_ts=100420`，两行 ts=50377/50378） | 真实 OpenCode 时间戳全局递增，正常路径不触发；B2 的定向重扫只覆盖「已登记归档会话」。导入/时钟回拨会漏索引且不自愈（除非 `--rebuild`）。非本次返工引入 |

## 已执行命令与退出码

| 命令（工作目录） | 退出码 / 结果 |
|---|---|
| `cargo test --locked --no-fail-fast`（`C:\wt\s5-reverify\agent-memory`） | 0；710 passed / 3 ignored / 0 failed |
| `python -I -m unittest discover -s . -p "test_recall_*.py"`（`assets/scripts`） | 0；73 tests OK |
| `git diff d47f526 b378a42 -- agent-memory/tests` | 0；仅新增 1465 行，既有测试零改动 |
| `git diff --check d47f526 b378a42`；`git status --short` | 0；干净 |
| `python -I C:\wt\s5r2-work\scripts\scenario_b1b2.py`（自建 e2e） | 0；B1 删会话/子行 + B2 回活通过 |
| `python -I C:\wt\s5r2-work\scripts\misc.py`（B3、rescan、回归、MCP、N5） | 0；B3 三轮稳定、rescan 10 行一致、回归全过 |
| `python -I C:\wt\s5r2-work\scripts\b4_steps.py build/full/delta/mirror/readback/journal` | 均 0；数据见 B4 |
| `python -I C:\wt\s5r2-work\scripts\scale220_steps.py clone` / `runs` | 0；数据见 3.1 |
| `python -I C:\wt\s5r2-work\scripts\conc_quick.py`（并发写 476 次删/插） | 0；live=indexed=400、0 死命中、10/10 回读成功 |
| `python -I C:\wt\s5r2-work\scripts\crash_test.py`（对账中途 kill） | 0；下一轮 `updated`、`rebuilt=false`、集合相等 |
| `agent-memory read --root <archive> --domain default --sha256 <sqlite_delta record>` | HEAD 0（hash 自校验通过）；基线 6（integrity check failed） |
| `agent-memory mcp`（initialize/tools/list/memory_search/memory_read） | 0；cache 逐文件哈希不变 |
| `agent-memory index`（220k，无变化，两二进制交替各 3 次） | 均 0；HEAD 4.10–4.71 s / 基线 0.33–0.38 s |

## 备注与限制

- 未验证：真实生产根上的 exact 0.864 / 合并 1.000 评估；`assets/dist` 未重建；真实 61/62/64/90 未运行（按约束不运行、不注册）。
- 未读取任何真实会话/数据库/生产 Archive/真实配置；所有 fixture 在 `C:\wt\s5r2-work\`，文本 needle 为 `rv2 …` 合成串。
- 计时为 debug 构建（与上一轮基线对照同 profile）；release 的绝对耗时会降低，但逐行探测的 O(行) 形态不变。
- 复验期间 harness 多次异常重启，曾导致并发脚本重复启动；3.2 的数据取自最终一次互斥运行（476 次写操作）与独立 `crash_test.py`，异常中间态未计入结论。
