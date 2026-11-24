# STAGE2-S1 VERIFICATION（独立 Verifier，被验 07b95b1，父 71c9a1d）

- verdict：**阻断需修复**
- 方法：隔离检出 `C:\wt\s1-verify` 只读；全部用合成库 + 自建临时目录（已清理）；未读任何真实会话/库/配置。额外构建了一个仅依赖被验 crate 的外部 harness（临时目录内）驱动 CLI 与 `replay_sqlite`，与源库白名单表逐行对比。

## blocking

### B1 `--verify`（及 7 天周期校验）在 `--session-since` 下无视过滤，写入范围外行，下一次增量又把它们删掉
- 位置：`agent-memory/src/capture_sqlite.rs` `run_verification_and_correct`（约 4262–4480）：实时扫描对每个白名单表 `SELECT [rowid,] * FROM <t>` 全表，无 `time_updated >= since` / 会话集过滤；并把全表 pk/hash 写回观测状态 `primary_keys/row_hashes`。
- 复现（合成库：`old` 会话 20 条 message+20 条 part，`new` 会话 300+300，project/todo 空）：
  1. `capture --session-since 1970-01-01T00:00:03Z` 首次 -> 基线，范围内 2 会话行。
  2. 同参数加 `--verify` -> `mode=delta rowsInserted=41 verifyMismatches=41`：`old` 会话及其 40 个子行被当作“补录”写入证据（违反过滤规格，并污染 `verifyMismatchesTotal`）。
  3. 源库新增 1 条范围内 message 后同参数增量 -> `rowsInserted=1 rowsDeleted=41`：刚补录的 41 行被当作删除；replay 的 message 数 320 -> 301。证据库出现“先错写后错删”，且默认每 7 天必现一次（周期校验在 `--session-since` 生产形态下就会触发）。
- 影响：过滤形态下校验结果不可信，违反规格 5（按会话时间过滤）与 4（--verify 补录）的组合；证据中产生虚假 delete。
- 建议：校验扫描复用与基线/观测一致的过滤（会话集 + `time_updated` 约束；子表用 TEMP 表 JOIN 或集合过滤），状态回写也只写范围内键；补一条 `verify + session_since` 用例（范围外行不得入证据、后续增量不得产生 delete）。

## non_blocking

### N1 同一 `time_updated` 值上、观测之后插入且主键小于水位键的新行被漏采，直到 `--verify`
- 位置：`observe_changes`（约 1540–1640）候选判断 `value == previous_value => key > previous_key`。
- 证据：S1（20 行同 ts=100，最大键 p069；之后插入 5 行 ts=100 的 p000–p004）-> 增量后 replay 20 行 vs 源 25 行；再插入同 ts 的高键行仍不补回；`--verify` 后一致。S7（3000 行同 ts 777，偶数键基线，奇数键事后插入）-> replay 1501 vs 源 3000，`--verify` 后一致。
- 为何：新行 INSERT 完全可识别（键不在 `previous.primary_keys`），当前却因 `(value,key)` 单调水位丢弃，属于可修复的丢失而非固有盲区；README 只笼统写“at/below the watermark”。
- 建议：`value == previous_value` 时 `key > previous_key || !previous.primary_keys.contains(key)` 即视为候选。内容同值被改仍是真盲区（保持声明）。

### N2 `--session-since` 下旧会话重新活跃后，其既有子行不被采集（直到 `--verify`，而 B1 使该补救本身有缺陷）
- 证据（F1）：阈值 3000，`old` 会话（t=1000）后被 UPDATE 为 9500 进入范围；增量后 replay message 8 vs 过滤后源 11、part 6 vs 9；仅新插入的子行被采。另一个中途进入范围的会话同样（9 vs 11）。
- 原因：子表仍按自己的 watermark 取候选，已进入 selected 的会话的历史子行低于水位。README 未声明。真实 OpenCode 里“恢复旧会话”正是这一形态。
- 建议：会话新进入选集时按 `session_id` 补读其全部子行；或在 README 列为盲区。

### N3 `--session-since` 要求所有会话子表有 `session_id` 列，否则整次采集硬失败
- 证据：白名单表 `message` 为 `(it's, k"2, v)` 复合键且无 `session_id` 时，`capture --session-since ...` 返回 `sqlite_unavailable: no such column: session_id in SELECT * FROM "message" WHERE session_id IN (...)`（exit 7）。无过滤路径正常。真实 OpenCode 有该列，故影响小；但应降级为“该表不过滤并告警”或明确报 schema 错误码。位置：`export_baseline` 子表 SQL（约 1361）。

### N4 每次增量采集都把整条增量链（全部代、含 before/after 行内容）解压并解析进内存
- 位置：`capture_sqlite.rs:3681`（`sqlite_deltas(archive, ...)` 无条件调用，即使 `changes` 为空）与 `:4291`；`sqlite_deltas` 返回 `Vec<SqliteDeltaBatch>`，所有 `lines` 驻留。
- 为何：链上限 = 基线字节 50%，真实基线数 GB 时链可达 GB 级内存/IO；另外 `fill_before_from_evidence` 对含 update/delete 的表每次完整解压该表全部基线块（O(基线)），而 OpenCode 的 part 几乎每个周期都有 update。规模用例仅有 3 KiB 增量链，未覆盖。建议：仅在 `changes` 含 update/delete 时才读链，并按表/按行流式；在 S3 真实试采上核对。
- 同类：`replay_sqlite` / `sqlite_deltas` 即使目标代只用一代也解析全部代的 delta，历史越长越慢。

### N5 `--verify` 内存随行数线性增长
- 规模实测 250k 行 peak-heap 259 MiB（预算 300 MiB 针对稳态，verify 达标但余量小）。`expected_by_table`、`live_hashes`、观测状态 pk->hash 三份 map 并存，行内容不驻留（已确认）；百万级行会远超 300 MiB。S3 需实测。

### N6 `--session-since` 若由调度器传“滚动阈值”（如 now-7d），每次都是 `filter_changed` -> 整库重基线
- 与规格一致（过滤变化即重基线），但与“30 分钟周期不得整库重导”目标冲突；S2 驱动必须传固定阈值，建议在 README 明示。

### N7 文档
- README 无上述 N1/N2 的盲区声明；其余（SQLite 一节、格式矩阵 @2、测试数 629/3 ignored、50% 链规则、过滤/校验描述）与实现一致。新增行无 `<user-home>` 真实路径（`git diff` 扫描 0 命中）。`Cargo.toml` 仍启 rusqlite `backup` 特性（`capture_sqlite.rs` 已不使用），可在清理时移除。

## 必查项结论

| 项 | 结论 |
|---|---|
| 1 全套测试 | 通过：629 passed / 0 failed / 3 ignored（exit 0） |
| 2 规模用例 | 通过：基线 2.65 s（250,001 行，19 块，567,194 KiB -> 2,230 KiB，peak-heap 161 MiB）；稳态 2.77 s（改100+增100+删10，mode=delta，增量 3 KiB，peak 139 MiB；预算 <=10 s、<300 MiB 均满足）；verify 5.99 s、0 mismatch、peak 259 MiB；合成库构建 532 s |
| 3 凭据 | 通过：含 credential/account/control_account/kv/session_share/other/带引号表名（均填哨兵）的合成库，跑首次、增量、`--rebaseline`、`--verify`、`--session-since`（两次）、取消过滤后，对运行时根全部 22 个文件做字节 + zstd 解压后扫描：6 个哨兵 0 命中（阳性对照可命中白名单内容）；仅出现非白名单表名（计数事实），非白名单 DDL/列名未入库；观测状态仅含键集/水位/会话 id，无行内容（以 200 字节重复载荷验证）。代码审查：所有行读取 SQL 均由 `SQLITE_TABLE_WHITELIST` 门控（基线、观测、校验、`scan_*`、TEMP 表 JOIN），白名单外只有 `SELECT count(*)`；`read_schema` 对全表只读 DDL/`table_info` |
| 4 标识符 | 通过：`quote_ident` 双引号转义；白名单表含 `a"b`、`x y`、`group`、`select`、`it's`、WITHOUT ROWID 复合键、名为 `rowid` 的列、`weird"t SELECT` 表，首次/增量/verify/replay 与源库逐行一致；`time_updated` 阈值为整数格式化、其余绑定参数。唯一缺陷见 N3 |
| 5 水位对抗 | time_updated 倒退（S2）漏采，verify 后补齐（声明盲区）；同值内容改/删后同值重插（S2c/S4）漏采，verify 补齐（声明盲区）；非 ASCII/引号/空串/emoji 文本主键（S3）一致；删后新值重插一致；未 checkpoint 的 WAL（S5，writer 持有）一致；持续并发写（S6，5 次并发采集，exit 全 0）停写后再采一次与源一致；同值低键新行漏采见 N1 |
| 6 重放一致性 | 通过：S3/S5/S6/C 多轮增量后逐行一致（replay 行集 == 源白名单行集）；V2 外键乱序（S8，session_message 先于 session_v2）一致；replay.rs 物化用 FK OFF + 父表优先（测试覆盖） |
| 7 50% 链规则 | 通过：9.8 MB 基线 + 60 次单行小增量，始终 gen 1（链 100 KB、长度 60）；5% 行更新每轮约 1 MB，累计超 50% 时才重基线一次，随后继续 delta。未出现频繁整表重基线；链长度无条数上限，但字节受 50% 约束（重放成本见 N4） |
| 8 代码审查 | 崩溃顺序/fail-closed/@1 拒绝与测试一致（相关用例通过）；`--verify` 不整表驻留行内容，但见 B1/N4/N5；未发现把行内容写入观测状态的路径 |
| 9 文档 | 见 N7 |

## 已执行命令与退出码
- `cargo test --locked --no-fail-fast`（`C:\wt\s1-verify\agent-memory`）-> 0（629/0/3）
- `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture` -> 0（用时 544 s）
- `cargo build --release --locked --bin agent-memory`（独立 target 目录）-> 0
- 自制 harness（外部 crate，path 依赖被验 crate）`cargo build --release` -> 0
- Python（`-I`）驱动：`cred.py`（凭据，含全根 zstd 解压扫描）、`adv.py`（S1-S8）、`adv2.py`（标识符/过滤/链规则）、`adv3.py`（B1 复现）；capture 子命令 exit 均为 0，唯独 N3 场景 exit 7（预期复现）
- `git diff 71c9a1d 07b95b1 | grep chenr` -> 0 命中；`git diff --check` -> 干净
- 被验仓库源码未修改（仅 `target/` 构建产物）；临时目录 `%TEMP%\s1-verify-2117` 已清理
