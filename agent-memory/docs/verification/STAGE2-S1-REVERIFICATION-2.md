# STAGE2-S1 REVERIFICATION-2（独立 Verifier，被验 9b87979，上一轮 9bc76f1）

- verdict：**通过**
- 方法：隔离检出 `C:\wt\s1-reverify2`（detached 9b87979）只读；自建临时根 `%TEMP%\s1-reverify2-a7f3`（含 `h/` 外部 harness crate，path 依赖被验 crate，`cargo build --release --offline`，直接调用 `capture_source_with_options` / `replay_sqlite` / `sqlite_deltas` / `observe_database` / `evidence_decompressed_bytes`），合成库 + `tempfile` 运行时根逐行比对 replay 与「源库按同一过滤」。全程未读取任何真实会话/数据库/配置（`~/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory`、`~/.config/opencode` 均未访问）；Python 一律 `python -I`。
- 总体：上一轮的 R1、R2 两个阻断均已按声明修复并复现通过；adopt 后自动校验、N-R3 两条读取路径、N-R4/N-R5 均成立；凭据哨兵、B1、N4 无回归；全套测试与规模预算达标。发现 1 处文档重复段与 1 个**空转的回归用例**（`legacy_delta_batches_without_ranges…` 实际未改写 manifest），后者使 N-R3 在本仓库内无真实覆盖——实现本身已由本轮独立验证通过，故不阻断，但建议修复用例。

## 逐条结论

### R1（阻断）补采跳过本次已读/已归档主键 —— **通过**

- 位置：`capture_sqlite.rs:1974`–`2041`（`extend_backfill`，关键行 `:2026` 改为 `if self.contains(&raw_key) || previous_keys.contains(&raw_key) { continue }`，不再走 `disambiguate_key`）；`capture_sqlite.rs:2146`–`2186`（补采集合 = 新进入范围 ∪ 本次被更新过的会话行，`:2175` 的 `backfill_trusted` 不可信时整体清空）。
四类复现（harness `s1h r1a|r1b|r1c|r1d`，全部 exit 0，129 项断言全 PASS）：

| 场景 | 复现形态 | 观测 | replay vs 同一过滤源库 | 下一周期 |
|---|---|---|---|---|
| r1-a（上一轮 c2） | 阈值 4000 基线（`ses_old` t=1000 在范围外，`ses_cur` 300 条子行）→ 向 `ses_old` 增 message+part（t=9000）并把会话 `time_updated` 改 9000 | `ins=3`（会话 + 2 个水位新子行），`del=0` | 0 差异，`#rowid:`/`#dup:` 键 0 个 | `skipped_unchanged`，`ins/upd/del=0` |
| r1-b（上一轮 c3） | 范围外会话的既有子行被改（t=9000）+ 新增兄弟行，随后会话入范围 | `ins=5`、`upd=0`（5 行全部是首次归档，无重复） | 0 差异，无幻影键 | `skipped_unchanged` |
| r1-c | 会话行与其子行全部删除后同 id 重建（新子行 t=9000） | `ins=1`、`upd=3`、`del=1`（重建的会话/复用 id 的消息是同键 update，`p_a_1` 为 insert，`p_a_0` 为 delete） | 0 差异，无幻影键 | `skipped_unchanged` |
| r1-d | 3 个范围外会话分 3 波进入范围，每波进入前各做增/改/删；每波之间再对已在范围内的会话做增改 | 每波 `ins=4`，in-scope 波 `ins=1/upd=1` | 每波 0 差异，无幻影键 | `skipped_unchanged` |

- 关于「反复离开/重入 3 次」：`observe_changes` 的 `selected_sessions` 只增不减（`capture_sqlite.rs:2061` `previous.selected_sessions.clone()` + `:2101` `selected.extend(session_ids)`），同一基线代内会话**进入后不会再离开**范围，因此「离开再重入」在代码上不可构造；本轮以「3 次连续进入 + 每次进入前子行增删改」作为等价的最强形态，并另加「已采集会话被反复触碰（会话行 update）不重复记账」的 3 万子行成本场景（`s1h cost`：会话行 update 后采集 91 ms、`ins=0`、0 差异、无幻影键）。若将来允许范围收缩（例如滚动阈值不经重基线），该结论需重测。
- 上一轮报告的证据链残留（幻影 `#rowid:N` + 下一周期 delete 抖动 + 小库整库重基线）在本轮四个场景中均未出现。

### R2（阻断）与 SQLite BINARY 排序一致的类型化比较器 —— **通过**

- 位置：`capture_sqlite.rs:5051`–`5191`（`SortValue` / `sort_values_from_key:5125` / `compare_sort_keys:5177`）、`:5193`–`5249`（`pk_collation_non_binary`）、`:5251`–`5497`（归并 + 两路单调性检查 + 清空回退 + drain `:5450`–`5473`）、`:5647`–`5719`（当次回退路由与 `verifyBoundedTables`）。
单个真实盲插（盲 = `time_updated` 低于水位且键不触发 N1 同值规则，前置断言该插入在常规观测中 `ins/upd/del=0`）：

| 表形态 | `verifyMismatches` | 补录 | `verifyBoundedTables` | replay |
|---|---|---|---|---|
| `message(id INTEGER PRIMARY KEY)` 1..30 缺 9 | 1 | `ins=1` | `[]` | 0 差异 |
| `todo(session_id, position)` 复合主键 0..24 缺 9 | 1 | `ins=1` | `[]` | 0 差异 |
| 无主键 rowid 表（显式 `rowid=9` 盲插） | 1 | `ins=1` | `[]` | 0 差异 |
| 负整数与 REAL 混合主键（盲插 −4.5） | 1 | `ins=1` | `[]` | 0 差异 |
| 同列 TEXT×INTEGER（盲插 `'Alpha2'` 落在文本档） | 1 | `ins=1` | `[]` | 0 差异 |
| 复合键含 NULL 分量（第三个 NULL 行） | 1 | `ins=1` | `[]` | 0 差异 |
| 主键列 `COLLATE NOCASE`（盲插 `'ALPHA'`） | 1 | `ins=1` | `["todo"]` | 0 差异 |
| `WITHOUT ROWID` + `COLLATE NOCASE`（运行中错位回退） | 1 | `ins=1` | `["todo"]` | 0 差异 |

- 上一轮 R2 的形态（1 行真实差异放大为 22/16/17 行 insert+delete 风暴）未再出现；连续两次 `--verify` 第二次 0 mismatch（8 个场景均验证）。
- 最小键与最大键删除：`message` 删 id=1、id=30 后 `--verify` → `del=2`、`vmis=0`、`missingBefore=0`、replay 0 差异，`pk:[1]`/`pk:[30]` 不再出现在 replay。
- 归并 drain（「大于全部源行」的待处理条目）由 adopt 场景真实触发：删除最高键 `prt_24` 后自动校验计到 2 个 delete + 1 个 insert（`vmis=3`）；若 drain 缺失则只会是 2。

### 崩溃 adopt 后的一次性全量校验 —— **通过**

- `adopt_committed_unit` 的基线 adopt 把 `last_verify_at_ms` 置 0（`capture_sqlite.rs:3203`），`capture_delta` 的 `verify_due` 因此包含 `previous.last_verify_at_ms == 0`（`:4280`–`:4283`）；delta adopt 保持原值（`:3370`）。
- 复现（`s1h adopt`）：基线 → `--rebaseline` + `AfterManifestBeforeState` 崩溃 → 重跑 `change=recovered_after_crash`、`ok=false` → adopt 窗口内删 `prt_05` 与最高键 `prt_24`、插入同水位低键 `prt_000`（常规观测三者均不可见）→ 下一次采集 `verify=true`、`vmis=3`、`ins=1/del=2`，replay 与源库 0 差异 → 之后连续两次采集 `skipped_unchanged` 且 `verify=false`（未在每次采集都触发全量校验）。
- 与 N-R4 的声明一致：degraded 文案已改为指向「下一次采集自动校验，`--verify` 可立即触发」。

### N-R3 无键范围记录的 delta 批次 —— **通过（实现）；本仓库用例空转**

- 实现两处：`fill_before_from_evidence` 的 `None if meta.table_ranges.is_empty() => {}`（`capture_sqlite.rs:2355`–`2358`）与 `build_delta_overlays` 的 `!meta.table_ranges.is_empty() && …`（`:4903`–`:4911`）。
- 独立复现（`s1h legacy`）：gen2 改 `prt_05` 后，把全部 manifest 的 `source.sqlite.delta_table_ranges` 置 `{}`（**2 个 manifest 实际被改写**），gen3 再改同一行 → gen3 的 `before` 为 `v1-5`（来自被清空范围的 gen2 批次）而非基线 `{"type":"text","n":5}`；随后 `--verify` `vmis=0`（期望侧 overlay 也正确读取了无范围批次）；replay 0 差异。
- **但仓库内用例未真正构造该形态**：`tests/p3_sqlite_consistency.rs:3468` 用 `value["sources"].as_array_mut()` 改写 manifest，而 manifest 顶层字段是 `source`（单数）、范围字段是 `source.sqlite.delta_table_ranges`（snake_case）。实测 top-level keys = `["schema","capture_id","captured_at_ms","device_id","provider","source","objects","records","attachments"]`，`value["sources"]` 不存在 → 循环体从不执行、`touched` 恒为 false → **用例在修复前后都会通过**。列为 non_blocking（实现已独立验证正确）。

### N-R4 / N-R5 —— **通过**

- N-R4：README 崩溃条目已写明「基线 adopt 的主键集为空且不可信 → 下一次采集自动全量校验（`lastVerifyAtMs` 从 0 开始）→ `--verify` 可立即触发」，与实现一致；degraded 报错文案同步。但同一段落被重复写了两遍，见 non_blocking 第 1 条。
- N-R5：`tests/p3_sqlite_decompress_isolation.rs` 末尾空行已删；`git diff --check 9bc76f1 9b87979` → exit 0。

### 回归

- **凭据哨兵**（`s1h cred`）：`credential`/`account`/`kv`/`control_account`/`session_share`/`event` 六表填 6 个哨兵值，依次跑首次（带过滤）、增量、`--verify`、再增量、补采（旧会话被触碰入范围）、`--rebaseline` 崩溃 + adopt、adopt 后自动校验；运行时根 19 个文件原始字节扫描 0 命中，10 个 zstd 对象解压后扫描 0 命中；阳性对照（白名单行体的 `ROW-BODY-SENTINEL`）在 3 个对象中命中，证明扫描确实解压到内容；`state/` 下 5 个观测状态文件解压后 0 个含行体（状态只存键/水位/哈希）。
- **B1**（`--verify` 遵守 `--session-since`）：阈值 4000 基线 7 行（2 会话 + 2 message + 2 part + project）→ 同参 `--verify` `skipped_unchanged`/0 mismatch → 范围内盲改 + 范围外盲改同时存在 → `vmis=1`、`upd=1`，replay 与「同一过滤源库」0 差异，`part` 键集合中不含 `ses_old`。
- **N4**（无变化观测解压 0 字节）：无变化观测 `evidence_decompressed_bytes()=0`；纯 insert 增量 0；update+delete 增量 413 字节（只解压被触及证据）。

### 代码审查（第 7 项）

- **比较器 vs `ORDER BY`**：两路流都用 `order_suffix`（`capture_sqlite.rs:584`–`600`，列序取自 `declared.primary_key`，与 `row_key` 生成 `pk:[…]` 的分量顺序同源），比较器按分量逐列比较，`NULL < INTEGER/REAL < TEXT < BLOB`、数值按值、文本/BLOB 按字节序，与 SQLite 跨存储类排序一致；`rowid:N` 解码为整数。非 BINARY collation 由 `PRAGMA index_list`/`index_xinfo` 预检（`:5193`–`:5249`），未覆盖的 `WITHOUT ROWID` 由两路流各自的单调性检查兜底（`:5316`/`:5366`/`:5459`），实测能当场回退并列出（`nocasewr`）。两条兜底路径「同一次运行内完成 + 结果清空重算」（`:5690`–`:5719`），无双重计数。
- **有界回退内存**：`verify_table_bounded` 仍是「整表 expected map + 整表 live map」（`:5500`–`5533`），O(表行数)——与返工 1 之前同量级、与 §返工 2「有界回退路径内存 ∝ 表行数」的披露一致；默认路径（合并）未变。归并路径每表额外 clone 一份 overlay 快照（`:5647`、`:5687`），内存 ∝ delta 链而非行数。
- **是否新增把行内容写进观测状态**：`TableState` 字段仍只有 columns/pk/水位/`primary_keys`/`row_hashes`（`:907` 起）；`run_verification_and_correct` 只回写 `last_verify_at_ms` 与 `verify_mismatches_total`（`:5735`–`:5738`），不写行内容、不写键集；状态文件字节扫描（行体哨兵）0 命中。
- 已排查但**不构成缺陷**的形状：TEXT 列中的非法 UTF-8 值以 `{"invalid_utf8_hex":…}` 表示、在比较器里与 BLOB 同档（`:5158`–`:5172`），与 SQLite 的「全部 TEXT 按字节」不一致；该不一致只会让某一路流在比较器下非单调 → 触发有界回退（结果仍正确），不会静默错序，故不列为 finding（未单独构造数据，仅代码路径推演）。
- `pk_collation_non_binary` 把 origin `c`（普通索引）也算入，若表上存在非 BINARY 的**次级**索引也会退回有界路径——偏保守、结果正确，仅影响该表 verify 的内存/耗时。

### 文档与卫生（第 8 项）

- `git diff --check 9bc76f1 9b87979` → exit 0（无空白错误）；`git diff … | grep chenr` → 无命中（新增行无真实用户路径）；相对 9bc76f1 的测试文件改动**纯新增**（`p3_sqlite_consistency.rs` 0 删除行），未削弱既有断言。
- README 四处新增语义（补采不重复记账、校验按 SQLite 排序 + `verifyBoundedTables`、无范围批次始终读取、adopt 后自动校验）与实现逐条对应，唯一偏差是崩溃段落的重复段（non_blocking 1）。

## non_blocking

1. **README 崩溃条目重复了同一段说明**（`agent-memory/README.md:244`–`255`）。原文连续两次写「基线 adopt 从 manifest 重建观测、主键集为空不可信 → 无法看见删除与同值低键插入 → 下一次采集自动全量校验（`lastVerifyAtMs` 从 0 开始）→ `--verify` 可立即触发」，措辞不同但语义完全相同。证据：`grep -n "adoption rebuilds" README.md` → `244`、`249` 两处。为何：读者会以为存在两种不同机制，且是返工合并时的叠加痕迹。建议：删除其中一段（保留含 `--verify` 强制触发的那段）。
2. **`legacy_delta_batches_without_ranges_still_provide_before_images` 是空转用例**（`agent-memory/tests/p3_sqlite_consistency.rs:3444`–`3481`，关键行 `:3468`）。它用 `value["sources"].as_array_mut()` 去清空 `deltaTableRanges`，但 manifest 顶层字段是 `source`（单数），范围字段名为 `source.sqlite.delta_table_ranges`；实测打印 manifest 顶层键为 `["schema","capture_id","captured_at_ms","device_id","provider","source","objects","records","attachments"]`，`value["sources"]` 为 `None` → 循环体从不执行、文件从不重写 → 用例在「修复前」也能通过，N-R3 在本仓库内没有真实回归覆盖。为何重要：本轮 R1/R2 的修复都靠用例把「修复前会失败」的形态钉住，N-R3 的用例恰恰没有这个性质。建议：改为 `value["source"]["sqlite"]["delta_table_ranges"] = json!({})`，并断言 `touched` 计数 > 0（本轮独立 harness 用该路径得到 2 个 manifest 被改写、gen3 `before` 为 `v1-5`、`--verify` 0 mismatch，可作为期望值）。
3. **调试用的 `eprintln!` 残留在校验预检里**（`agent-memory/src/capture_sqlite.rs:5211`、`:5226`）。`pk_collation_non_binary` 在 `AM_TRACE_DECOMPRESS` 被设置时打印 `[index_list …]`、`[collation … key=…]`；该环境变量原有语义是解压度量（`:4254`），此处在每次 verify、每张表都做一次 `var_os` 查询并输出与解压无关的信息。为何：库内调试输出会污染 CLI 的 stderr 契约，且与既有 trace 语义不符。建议：删除，或改为 `log::debug!` 并复用同一开关。
4. **补采信任门由一个隐式耦合推导**（`agent-memory/src/capture_sqlite.rs:2175`–`2181`）。`backfill_trusted` 取 `previous.tables.values().next()`（BTreeMap 首表）的 `primary_keys_known` 代表整份状态的信任度；`extend_backfill` 内则以 `let _ = previous_keys_known;`（`:2012`）丢弃自己算出的同一判断，注释称「由调用方门控」。当前所有表的 `primary_keys_known` 只会整体为 true（常规观测/baseline）或整体为 false（adopt），因此行为正确（本轮 adopt 场景实测补采被整体跳过、由校验愈合）。为何：语义正确性依赖「各表标志永远一致」这一未写下的不变量，后续若出现部分表重建状态就会失效。建议：改为按子表自身的 `previous_state.primary_keys_known` 判定，并删除 `previous_keys_known` 死绑定与 `static EMPTY_KEYS` 变通。
5. **补采集合扩大到「本次被更新过的会话」后，稳态成本与规模用例的形状不一致**（`capture_sqlite.rs:2159`–`2173`）。只要会话行进入本次 update 候选，其全部子行会被再扫一遍（按键跳过，不重复记账，实测 3 万子行的会话被触碰时采集 91 ms、`ins=0`）。规模用例的稳态阶段只改 `part`、不动 `session` 行，因此 1.0 s 的稳态数字不含该形态；真实 OpenCode 每次消息都会更新会话行，稳态每次采集都要多扫活跃会话的子表一次（成本 ∝ 该会话子行数，与库总量无关）。为何：不影响正确性，但稳态预算的证据形状与本改动不匹配。建议：规模用例的稳态阶段增加一次 `UPDATE session SET time_updated=…`，或在 README/记录里注明该成本项。

## 必查项结论

| 项 | 结论 |
|---|---|
| 1 全套测试 | 通过：`cargo test --locked --no-fail-fast` exit 0，**648 passed / 0 failed / 3 ignored**（28 段 `test result` 含 Doc-tests，计数一致） |
| 2 规模用例 | 通过（exit 0，537.18 s）：基线 3.07 s / 19 块 / 567,194→2,230 KiB / 峰值 161 MiB；稳态 **1.01 s**（≤10 s）、峰值 **63 MiB**（<300）、解压证据 56,242 KiB（基线 9%）；`--verify`@250k 4.18 s / 0 mismatch / **59 MiB**；`--verify`@450k 8.80 s / 0 mismatch / **110 MiB**（增长 +51 MiB ≤100）。与 §返工 2 记录（0.97 s / 62 / 59 / 109）同量级 |
| 3 R1 | 通过（四场景 + 3 万子行触碰场景；replay 与同一过滤源库逐行一致、无 `#rowid:`/`#dup:`、下一周期 `skipped_unchanged`） |
| 4 R2 | 通过（整数/复合/rowid 各 1 mismatch + 1 行补录；负整数+REAL、TEXT×INTEGER、NULL 分量、NOCASE、WITHOUT ROWID 错位回退、最小/最大键删除全部正确） |
| 5 崩溃 adopt | 通过（下一次采集自动校验并补录 3 项，含最大键删除；之后两次采集不再校验） |
| 6 回归 | 通过（凭据哨兵 0 命中；B1 不回归；N4 无变化观测 0 字节、纯插入 0 字节） |
| 7 代码审查 | 通过（比较器与 `ORDER BY` 列序/类型一致、非 BINARY 双保险、有界回退内存与披露一致、无新增行内容入状态） |
| 8 文档/卫生 | 通过（`git diff --check` exit 0、无真实路径、测试纯新增）；文档有 1 处重复段（non_blocking 1） |

## 已执行命令与退出码

环境：`C:\wt\s1-reverify2`（detached 9b87979，`git status --porcelain` 为空，被验源码未被修改）；临时根 `%TEMP%\s1-reverify2-a7f3`（`h/` 与 `work/` 均在内部，未使用 `%TEMP%\opencode`）。

- `cargo test --locked --no-fail-fast`（`C:\wt\s1-reverify2\agent-memory`）→ **0**（648 / 0 / 3）
- `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture` → **0**（537.18 s，全部预算断言 true）
- `cargo build --locked` → **0**（0 warning）
- 外部 harness：`cargo build --release --offline`（`%TEMP%\s1-reverify2-a7f3\h`）→ **0**；`s1h.exe all` → **0**（129 项断言全 PASS，`ALL CHECKS PASSED`）
- 场景：`r1a`、`r1b`、`r1c`、`r1d`、`int`、`comp`、`rowid`、`negreal`、`textint`、`nullkey`、`nocase`、`nocasewr`、`minmax`、`adopt`、`legacy`、`cred`、`b1`、`n4`、`cost` 全部 exit 0
- `git -C C:\wt\s1-reverify2 diff --check 9bc76f1 9b87979` → **0**
- `git -C C:\wt\s1-reverify2 diff 9bc76f1 9b87979` 管道到 `grep chenr` → 无命中（grep exit 1）
- `git -C C:\wt\s1-reverify2 status --porcelain` → 空（仅 `target/` 构建产物，未跟踪）
- 本轮临时目录在写完本报告后清理。

## 局限

- 全部结论基于合成库与构造库（含 837 MiB 合成规模库），未使用真实 OpenCode 数据库；真实键分布、冷缓存下的绝对值仍待 S3 实测（与前两轮同）。
- 「会话离开范围后重入」在当前实现中不可构造（`selected_sessions` 只增不减），本轮以 3 次连续进入代替，已在上文 R1 段落说明适用条件。
- 运行中错位回退只在 `WITHOUT ROWID` + `COLLATE NOCASE` 构造中触发；预检路径由 `COLLATE NOCASE` 行表触发。两条路径各自验证一次，未穷举其他非 BINARY collation（`RTRIM` 等同属 `index_xinfo` 可检出类别）。
