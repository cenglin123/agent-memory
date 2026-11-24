# STAGE2-S1 REVERIFICATION（独立 Verifier，被验 d16290a，上一轮 07b95b1）

- verdict：**阻断需修复**
- 方法：隔离检出 `C:\wt\s1-reverify`（detached d16290a）只读；全部使用合成库 + 自建临时目录 `%TEMP%\s1-reverify-9d41`（含独立 target 目录与外部 harness crate，path 依赖被验 crate，直接调用 `capture_source_with_options` / `replay_sqlite` / `sqlite_deltas` / `observe_database`，逐行比对 replay 与源库白名单表）。未读取任何真实会话/库/配置。
- 总体：上一轮 B1 已修复，规模与全套测试达标；但返工引入/遗留两个有命令证据的正确性缺陷（N2 补采产生幻影重复行；N5 流式归并的比较序与 SQLite 导出序不一致，导致假 mismatch 风暴），以及一个文档声明与实现不符的回退问题。

## blocking

### R1 N2 补采与同次观测的常规读取重复，生成幻影键 `#rowid:N`（旧会话「恢复使用」的典型形态必现）
- 位置：`agent-memory/src/capture_sqlite.rs:2006`（`extend_backfill` 内 `disambiguate_key(self.contains(&raw_key), ...)`）。
- 机理：补采先把已被本轮常规水位读取记过的行（键已在 `keys` 集合中）当成「主键重复」，走 `#rowid:N` 消歧，生成一个新键并记为 insert；该分支本应是 `if self.contains(&raw_key) { continue }`。随后 `if self.contains(&key) { continue }` 对消歧后的键失效。
- 复现（harness `h.exe c2`，合成库）：`old` 会话（t=1000）+ `cur` 会话 300 条 message/part，阈值 4000 基线；之后向 `old` 新增 message/part（t=9000）并把 `session.time_updated` 改为 9000（恢复旧会话）。
  - 增量 `rows_inserted=11`，replay 比「源库按同一过滤」多出 `message pk:["m_old_new"]#rowid:304`、`part pk:["p_old_new"]#rowid:304`（EXTRA，2 行）。
  - 下一个周期（源库无变化）产生 `rows_deleted=2` 清掉幻影；之间的观测点 replay 含重复行，且 delta 链上留下一对虚假 insert/delete。小库（基线很小）时下一周期还会因 `delta_chain_threshold` 触发整库重基线。
  - 同类复现：`h.exe c3`（会话未变期间子行被修改、之后会话入范围；被修改的子行 `p_old_000#rowid:1`、新增的 `p_old_zzz#rowid:7` 变成幻影）；`h.exe c`（删除后同 id 重建会话，`m_s2_000#rowid:10`）。
- 影响：N2 修复在其主用场景（会话重入范围时同时有高于水位的新子行）下写出错误证据，违背「replay 与按同一过滤的源库一致」。返工测试只覆盖补采行全部低于水位的情形，故未发现。
- 建议：补采循环中，键已在 `keys` 中（`self.contains(&raw_key)`）一律 `continue`，不做消歧；补一条「重入会话同时有高于水位的新子行」用例（断言 replay 与过滤后源库逐行一致、无 `#rowid:` 键）。

### R2 `--verify` 流式归并按 Rust 字符串序比较键，但两路流按 SQLite 主键序排列，非文本字典序的键会触发假 mismatch 风暴
- 位置：`capture_sqlite.rs:5054`（`e_key.as_str().cmp(key.as_str())`）。键是 `pk:[...]` JSON 文本（或 `rowid:N`），SQLite 的 `ORDER BY` 按整数/复合/rowid 数值序，二者不一致（如 `pk:[10]` < `pk:[9]`、`rowid:10` < `rowid:9`、`"a"` 与 `"a b"` 的 JSON 引号位置、NULL 主键等）。只要两路存在任何真实差异，归并就失去对齐，把后续大量键判为「基线独有 → delete」与「源独有 → insert」。
- 复现（harness `h.exe d`，每例仅 1 行真实盲插入，id=9/position=9，较旧 `time_updated`）：
  | 表形态 | 真实差异 | 报告的 `verifyMismatches` | 写入修正 |
  |---|---|---|---|
  | `message(id INTEGER PRIMARY KEY)` 1..30 缺 9 | 1 | 43 | ins=22 / del=21 |
  | `todo(session_id, position INTEGER, PK(session_id, position))` 0..24 缺 9 | 1 | 31 | ins=16 / del=15 |
  | 无主键 rowid 表 1..25 缺 9 | 1 | 33 | ins=17 / del=16 |
  | TEXT 主键（含空格/大小写/非 ASCII/数字串） | 1 | 1（正确） | ins=1 |
  随机模糊（`h.exe fuzz`，60 轮整数主键盲插入/盲更新）：60 轮最终状态 0 不一致，但 6 轮 `verifyMismatches>25`（真实差异≤约 10）。观察到的最终 replay 因「后续 insert 覆盖先前 delete」而仍一致，但：①`verifyMismatchesTotal` 与 delta 体积被放大到 O(表行数)；②若遇到「证据多出、源已删但常规观测未发现」的真实删除（崩溃 adopt 后会出现），归并会先给后一个键记 insert 再给其记 delete（`5054` 的 Greater 分支后 drain 分支），最终丢失仍存在的行——此为代码路径推演，未单独构造到数据丢失，故不据此升级，仅作风险提示。
- 影响：实现者 N5 的「期望侧与源侧同序归并、与源逐行一致」声明不成立；真实 OpenCode 的 `todo` 为 `(session_id, position)` 复合主键（含整数分量），按此形态一次盲差异即放大为整表修正。文本主键（part/message/session，OpenCode 的 ASCII id）不受影响。
- 建议：归并比较必须用与 `ORDER BY` 一致的比较器（解码键元组后按 SQLite 排序规则：NULL < 数值 < 文本 < BLOB，文本 BINARY 字节序）；或对含非文本主键分量/rowid 的表退回现有 `verify_table_bounded` 有界映射路径；补一条整数/复合键表的 verify 用例（单个盲插入 → `verifyMismatches==1`）。

## non_blocking

### N-R3 空 `delta_table_ranges`（旧 manifest）并未「回退为全量解压」，`before` 取到过期基线
- 位置：`capture_sqlite.rs:2305` 起 `match meta.table_ranges.get(table) { Some(..) if ... => {} _ => continue }`；`build_delta_overlays`（约 4829–4838）对空 `table_ranges` 的 `all(..)` 为真而整批跳过。
- 复现（harness `h.exe legacy`）：gen1 更新行 p0005；把全部 manifest 的 `delta_table_ranges` 字段删掉；gen2 再更新同一行 -> gen2 的 update 行 `before` 为 `v0-5-xxx…`（基线旧值），应为 `v1-5`。replay 本身（用 after）仍与源一致。
- 影响小：生产证据库为空，无旧 manifest；但返工记录与代码注释宣称的回退语义与实现不符，`--verify` 对旧批次同理会跳过。建议：无范围记录的批次视为「可能包含」并读取；或在 manifest 校验处拒绝缺范围的 delta 批次。
- `min_key`/`max_key` 缺失的基线块处理正确（`:2284` 视为必读）。

### N-R4 崩溃 adopt 之后的首个观测会永久漏掉「adopt 前已被删除的行」与低键同值新行，直到 `--verify`
- 复现（`h.exe adopt`：基线后 `--rebaseline` 并在提交后崩溃，再次运行得 `recovered_after_crash`，`ok=false`）：adopt 后的删除（p120）与低键同值插入（p050）下一周期不入证据，且删除在键集重建后不再被发现（`incr2` 仍 EXTRA p120）；`primary_keys_known=false` 门控按设计不误报 insert，也没有把已归档行误报，N1 门控本身无误报（此项通过）。
- 属于已声明的盲区（adopt 周期报告 degraded），但 README 未提示「adopt 后应执行 `--verify`」。建议文档补一句并让 degraded 状态的修复提示指向 `--verify`。

### N-R5 其余
- `agent-memory/tests/p3_sqlite_decompress_isolation.rs:156` 新增文件末尾多余空行（`git diff --check` 报 1 处）。
- 规模用例 verify 峰值 59→110 MiB（2× 行数，+51 MiB，增长 86%），与返工记录一致；仍远低于 300 MiB。

## 必查项结论

| 项 | 结论 |
|---|---|
| 1 全套测试 | 通过：`cargo test --locked --no-fail-fast` exit 0，634 passed / 0 failed / 3 ignored |
| 2 规模用例 | 通过（exit 0，606 s）：基线 4.55 s（250,001 行，19 块，567,194→2,230 KiB，峰值 161 MiB）；稳态 1.74 s（delta 3 KiB，解压证据 56,242 KiB = 基线 9%，峰值 62 MiB）；`--verify`@250k 6.06 s、0 mismatch、59 MiB；@450k 11.89 s、0 mismatch、110 MiB；预算全部满足 |
| 3 B1 | **已修复**：上一轮复现步骤（`h.exe a`）：过滤基线 -> 同参 `--verify` 为 `skipped_unchanged`/0 mismatch；其后常规增量 `ins=1 del=0`，无虚假 delete。更难情形：范围内盲改（p_new_005）+范围外盲改（p_old_005，及两侧各删一条 message）同时存在，verify 仅补录范围内 1 条 update（`vmis=1`），范围外忽略，replay 与过滤后源库逐行一致；verify 重跑幂等；随后取消过滤 -> 恰好一次 `baseline`，replay 与源全库一致，再 `--verify` 无变化 |
| 4 N1/N2 | N1：同值低键新行 +20 条被采（`ins=20`），无误报；`primary_keys_known=false`（崩溃 adopt）路径不误报 insert，但 adopt 前已删/低键新行属已声明盲区（N-R4）；同值内容改写仍是声明盲区。**N2 正确性失败：见 R1**（补采与常规读取重复产生 `#rowid:N` 幻影行）；补采本身（行全低于水位）正确；会话删除后同 id 重建、离开又回来在该 bug 之外一致，verify 后一致 |
| 5 N4 | 通过（范围比较）：块键范围由实际键字符串的 min/max 计算，与导出序无关，因此不会漏块；构造 13 种前缀（空串/大小写/空格/`!`/引号/`é`/`中`/数字串）文本主键与整数主键，各 2 个基线块、3 代更新，186/200 条 update 的 `before` 与上一版本逐字段一致，replay 与源一致。旧 manifest 回退不正确：见 N-R3 |
| 6 N5 | **失败：见 R2** |
| 7 凭据 | 通过：含 `credential`/`account`/`kv`/`control_account` 哨兵的合成库，跑首次、增量、`--verify`、`--session-since`、N2 补采、`--rebaseline`，对运行时根全部文件做原始字节 + zstd 解压扫描：哨兵 0 命中；阳性对照（白名单表内容）命中 1 个对象 |
| 8 文档/构建 | `Cargo.toml` 不含 `backup`；`cargo build --locked` 通过；新增行无 `<user-home>`（0 命中）；README「Session filter」固定阈值/重基线一次/重入补采/`unfilteredTables` 说明与实现一致（但 README/注释对旧 manifest 回退的描述不实，见 N-R3） |

## 逐项（返工声明）

- B1：已修复（见上，通过）。
- N1：通过（门控无误报；盲区声明收窄属实）。
- N2：**未通过**（R1）。
- N3：既有用例通过；未另测。
- N4：块/批次范围查找正确，无变化观测不解压（规模用例 9%，隔离用例通过）；旧 manifest 回退不实（N-R3）。
- N5：内存达标，但比较序错误（R2）。
- N6：文档与 CLI 帮助已写明，通过。
- N7：`backup` 已移除，通过。

## 已执行命令与退出码
- `cargo test --locked --no-fail-fast`（`C:\wt\s1-reverify\agent-memory`）-> 0（634 / 0 / 3）
- `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture` -> 0（606.53 s）
- `cargo build --locked` -> 0；`cargo build --release --locked --bin agent-memory`（独立 target 目录）-> 0
- 外部 harness（`%TEMP%\s1-reverify-9d41\h`，`cargo build --release --offline`）-> 0；场景 `a`（B1）、`c`/`c2`/`c3`（N2）、`n1`、`d`（N5）、`fuzz`、`n4`、`legacy`、`cred`、`adopt` 均 exit 0（场景内断言以输出中的 CONSISTENT/INCONSISTENT 判读）
- `git diff f5f61c0 d16290a | grep chenr` 新增行 -> 0 命中；`git diff --check` -> 1 处末尾空行（N-R5）
- 被验仓库源码未修改（仅 `target/` 构建产物）；临时目录已清理
