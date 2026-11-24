# Archive P2 完成记录 · JSONL 活跃会话增量

范围：计划 `20261024-agent会话原始证据归档.md` 的 **P2** 阶段（Claude Code / Codex 的 JSONL 活跃会话增量采集）。P3–P6 未做。
真实数据：**否**。全部用例只写入 `tempfile` 临时根与合成 fixture，未指向任何真实会话目录，未读取真实会话正文。
Git：**未 commit**（按派单要求），改动留在工作区。

## 1. 实施文件

| 文件 | 动作 | 职责 |
|---|---|---|
| `agent-memory/src/capture_jsonl.rs` | 新增 | P2 增量引擎：文件级水位、完整记录闸门、变更分类、强制重捕与代际、显式降级、并发写入测试缝 |
| `agent-memory/src/coverage.rs` | 新增 | 每源覆盖报告投影（量纲显式命名 + 代际历史） |
| `agent-memory/src/archive/mod.rs` | 修改 | 水位/清单/状态加 P2 字段（全部 `serde(default)` 向后兼容）；`Archive::source_captures`；锁诊断信息与陈旧锁说明 |
| `agent-memory/src/capture.rs` | 修改 | JSONL 路由到增量引擎；`CaptureReport` 增 P2 字段与原语；`Failpoint::AfterManifestBeforeState`；源键规范化；缺源状态清理异常；入口/锁/fail-closed 语义不动 |
| `agent-memory/src/bin/agent-memory.rs` | 修改 | 新增 `coverage` 子命令（JSON），既有子命令与退出码不变 |
| `agent-memory/src/lib.rs` | 修改 | 模块注册（`capture_jsonl`、`coverage`） |
| `agent-memory/tests/p2_jsonl_incremental.rs` | 新增 | P2 验收用例 17 项（合成 fixture + 临时根） |
| `agent-memory/README.md` | 修改 | P2 契约、CLI、目录布局、原子性、锁陈旧告警、已知盲区 |
| `CURRENT.md` | 修改 | 任务状态重置为"无" |

未触碰：`src/recall/**`、`providers/**`、`tests/p1_archive.rs`、`tests/p0_formats.rs`、`tests/boundary_no_default_roots.rs`、`tests/fixtures|support/**`、`Cargo.toml`/`Cargo.lock`、安装脚本与分发物、Obsidian 库。

## 2. 目标 → 实现 → 证据

| # | 目标 | 实现 | 用例 |
|---|---|---|---|
| 1 | 水位按物理文件（已提交字节偏移/行号 + 源指纹） | `Watermark{offset,line,records,generation,head_probe,anchor_probe,probe_len,fingerprint}`；只在对象落盘并回读校验、清单提交、记录索引落盘之后推进 | 1、2、3、8 |
| 2 | 活跃追加；不完整尾记录留到下轮；崩溃不越水位；重复不重 | 尾记录无换行即不消费，`offset` 停在最后完整行末；`Failpoint::BeforeManifestCommit` | 1、2、3、8、13 |
| 3 | mtime 不变增长、active→archived、resume/fork | 只用长度 + 前缀探针判定（不信 mtime）；每物理文件独立定位，不按文件名推父子（采集层对 `file_name/parent/stem` 零命中） | 4、5、6 |
| 4 | 非追加变更禁止按旧偏移续采 | `length_regression` / `same_length_rewrite` / `prefix_rewritten` → 整文件重捕 + 代际 +1，旧对象与旧引用全部保留可读；**截断即便新内容无换行也必须落代际并进报告** | 7、15、17 |
| 5 | 源删除只改状态；覆盖报告计数 | 缺源→`source_missing`（保留水位与对象，清空上一轮异常）；`coverage` 输出量纲显式计数 + 异常 + 代际历史 | 10、9、12、16 |
| 6 | 只读源、单写者锁、fail-closed、CLI JSON 兼容 | 采集只以只读句柄打开源；锁仍在入口且 P2 复用同一把；访问标签照旧最严交集；仅新增字段与 `coverage` | 11、10、16 + 既有 `p1_archive::cli_json_contract_and_exit_codes` |

### 2.1 变更分类（廉价信号，均 O(8 KiB)）

| 观测 | 分类 | 处理 |
|---|---|---|
| 无水位 | `initial` | 全量，代际 1 |
| `len < offset` | `length_regression` | 整文件重捕，代际 +1 |
| 长度不变但头/锚探针不符 | `same_length_rewrite` | 整文件重捕，代际 +1 |
| 增长但已提交前缀探针不符 | `prefix_rewritten` | 整文件重捕，代际 +1 |
| 长度不变且探针相符 | `skipped_unchanged` | 不写任何对象/清单 |
| 增长且探针相符 | `append` | 只取 `[offset, 最后完整行末)` |
| append 但新增字节都未成行 | `skipped_unchanged` | 不消费，`incompleteTailBytes` 显式 |
| 读后复核不符（并发写入） | `degraded` | 不推进水位、报错计数、状态降级、覆盖 `ok=false` |
| 旧水位无 P2 探针（P1 存档） | `forced_recapture / legacy_watermark_without_probes` | 拒绝按旧偏移续采 |

### 2.2 计数口径

`CaptureReport`：`recordsDiscovered` = 已提交前缀记录数 + 本轮扫描记录数；`recordsNew` = 本轮新提交；`recordsDeduped` = 已在档（append 不重扫前缀，故前缀记录计入此项）；`errors` = 本轮异常条件（畸形记录、降级）；`bytes` = 本轮覆盖的源字节。
`coverage`（P2 新 API，量纲不同故分别命名）：`recordsCommitted`（已提交前缀，唯一自洽分母）、`discoveredLatest`（上者 + 最近一轮所见）、`capturedTotal`/`skippedTotal`/`errorsTotal`/`bytesTotal`（生命周期事件计数，可超过 `discoveredLatest`）、`observedLen`（最近一轮看到的源长度）与 `watermarkLen`（最近一次提交的长度）。`ok` 只看当前健康（degraded/failed 状态或最近一轮异常），不看生命周期计数。

## 3. 测试

```sh
cd agent-memory
cargo test
```

实测（2026-10-25，worktree `C:\wt\archive-p2-20261025`）：**exit 0，283 passed / 0 failed / 1 ignored**。

| 目标 | 数量 |
|---|---|
| lib（单元） | 157 |
| `bin` | 0 |
| `boundary_no_default_roots` | 5 |
| `p0_formats` | 4 |
| `p1_archive` | 8（未改） |
| **`p2_jsonl_incremental`** | **17（新增）** |
| `recall_boundary` / `recall_cli_mcp` / `recall_contract` | 6 / 7 / 9 |
| `recall_kb_device` / `recall_opencode` / `recall_read` | 25 / 22(+1 ignored) / 18 |
| doctests（5 个 compile-fail 反向用例） | 5 |

P2 用例（全部合成临时根）：

1. 首轮全量 + 重复采集 + 追加增量 + 连续 3 次增量 + 再次重复（`first_repeat_and_append_are_idempotent`）
2. 不完整尾行留待下轮、尾部继续增长仍不消费、补齐后只补 1 条；`observedLen` 与 `watermarkLen` 分别正确（`incomplete_tail_is_held_back_then_completed`）
3. 追加时崩溃：水位不动、孤儿对象上报、重跑复用同内容地址（`crash_on_append_keeps_watermark_and_recovers_without_duplicates`）
4. mtime 不变而文件增长仍判为 append（`growth_with_unchanged_mtime_is_detected`）
5. active→archived 移动：旧路径只改状态、归档路径全量去重、回读完整（`active_to_archived_move_keeps_state_and_dedupes_content`）
6. resume/fork：两段物理文件各自水位、共享前缀去重、分叉尾各采一份（`resume_fork_segments_are_captured_per_physical_file`）
7. 截短 / 同长度重写 / 同路径换内容（轮转）：三种非追加变更均强制重捕 + 代际递增，旧代对象保留可读，当前文件每行都在索引内（`discontinuity_forces_recapture_and_keeps_old_versions`）
8. 清单提交后、状态落盘前崩溃：重跑不重复对象/记录（`crash_after_manifest_before_state_recovers_without_duplicates`）
9. 并发重写：显式 `degraded`、不计成功、水位不动、覆盖报告 `ok=false`（`concurrent_rewrite_degrades_explicitly_without_advancing_the_watermark`）
10. 源删除只改状态 + 回读完整 + 第二实例 `LockHeld`（`source_deletion_is_state_only_and_second_instance_is_refused`）
11. 采集器不改源：源置为只读属性后采集仍成功，字节/长度/mtime 全等（`capture_never_modifies_the_source`）
12. 畸形 JSON 行照样入档并计数；随后删源不把上一轮异常当"当前异常"（`malformed_records_are_captured_and_counted`）
13. 空文件 / 首行未完整的文件：建立 0 记录水位，不伪造记录（`empty_and_partial_first_line_are_tracked_without_records`）
14. 水位丢失后的"重复全量"仍不重复对象与记录（`repeated_full_capture_without_a_watermark_does_not_duplicate`）
15. P1 形态水位（无探针）升级：强制重捕而非按旧偏移续采（`legacy_watermark_without_probes_is_forced_to_recapture`）
16. CLI `coverage` JSON 契约与退出码不变（`cli_coverage_reports_counts_and_anomalies`）
17. 截断成无换行片段：仍判 `forced_recapture / length_regression`、代际 +1、写入覆盖报告（`truncation_to_an_unterminated_fragment_is_reported`）

## 4. 独立审计与整改

完工前由独立只读子代理（deepseek-flash，未参与实现）按 6 项要求逐条核验并黑盒复现，`cargo test` 结果与本文一致（283/1）。其提出的问题与处理：

| 发现 | 级别 | 处理 |
|---|---|---|
| 截断/重写成无换行内容时被判为 `skipped_unchanged`，异常不进覆盖报告 | 中 | **已修**：只有 `append` 路径可因"无可消费完整行"返回 `skipped_unchanged`；`forced_recapture` 即使 0 记录也提交新代际并带 `discontinuity:*`。用例 17 |
| 覆盖报告 `fileLen` 取最近一次提交长度，与采集报告的观测长度矛盾 | 低-中 | **已修**：新增 `SourceState.observedLen`（每轮更新、`Option` 区分"未记录"），`coverage` 输出 `observedLen` 与 `watermarkLen` 两个字段。用例 2、12 |
| 源删除后仍回放上一轮 `anomalies`，导致覆盖 `ok=false` 与"异常每轮刷新"自相矛盾 | 低 | **已修**：缺源分支清空 `anomalies`、置 `change=source_missing`，并把 `observedLen`/`incompleteTailBytes` 归零。用例 12 |
| `skipped`/`captured` 可超过 `discovered`，三个总量不同量纲 | 低 | **已修**：`coverage` 字段按量纲重命名（`recordsCommitted` / `discoveredLatest` / `*Total`），README 与结构注释写明；`CaptureReport` 字段保持 P1 兼容 |
| 探针与指纹在清单提交后从活文件重算，写者在该窗口改写会让次轮误判 | 低 | **已修**：指纹改为读取前采集；探针优先取自已校验的提交字节（append 时头探针沿用已验证前缀）。残余情形见 §5.3 |
| 硬崩溃遗留锁文件后永久 `LockHeld`，未披露 | 中（运维） | **已披露 + 诊断增强**：锁文件记 `pid` + 取得时间，`LockHeld` 报错文字说明陈旧锁处置；不自动回收（无进程启动标识时猜活性会引入双写者）。README/§5.2 |
| 测试缝进入生产公共 API；`coverage` 恒退出 0 | 提示 | **保留并披露**：单进程无法真实制造读写竞争，缝有文档、生产不安装；`coverage` 与 `status` 一致——报告产出即退出 0，健康在 JSON 体内 |

### 4.1 复审

整改后由另一名只读子代理复验上述 6 项，`cargo test` 结果一致（283/1/0），并新发现两处：① manifest 指纹仍在 chunk 读取之后采集（与主张相反）——**已修**：改为读取前采集；② 缺源时 `observedLen` 被旧版回退逻辑回填为 `watermarkLen`（`0` 哨兵与合法值冲突）——**已修**：字段改为 `Option<u64>`，缺源写 `Some(0)` 且 `incompleteTailBytes` 归零，覆盖报告如实显示 0。其指出的"头探针在窗口增长时读活文件未披露"已补进 README 与 §5.3。

未发现：按文件名推断会话关系、水位早于清单提交推进、丢弃记录而不计数（空行非记录；未完整尾行计入 `incompleteTailBytes`）。

## 5. 已知限制

1. **P3–P6 未做**：OpenCode 与 SQLite 一致性（基线快照 + 行级差分、WAL）、附件与 ContentRef、provider 级覆盖对账与派生重放、套装离线接入与预编译分发物，均未实现。`coverage` 只是 P2 的每源投影，不是 P5 覆盖报告。
2. **硬崩溃后的锁**：`create_new` 哨兵锁只在栈展开时自动释放。被强杀后需人工确认并删除 `locks/capture.lock`（文件内有 `pid` 与取得时间）；不做自动回收。
3. **检测是"有界校验"而非全前缀校验**：若原地重写同时保持长度、首 8 KiB、末 8 KiB 与提交点探针逐字节一致，会判为 `skipped_unchanged`（已实测复现并如实记录）。另两处探针回退到活文件的情形：① 锚探针窗口起点早于本轮增量时，其较早部分从活文件读取；② append 让探针窗口增长（跨 8 KiB）时头探针从活文件读取。两种情形下的同窗口并发改写不可见。全前缀校验会让每轮 O(文件大小)，与计划 §4.4 冲突。
4. **源键规范化**：Windows `canonicalize` 的 `\\?\` 前缀被剥离，使同一文件在"存在/已删除"两种状态下保持同一键。P1 存档若已写入 `\\?\` 形态的键，需按新键重采一次；本任务未在真实运行时根上运行，无迁移影响。
5. **`coverage()` 的成本**：按源逐一扫描全部清单，源/清单数量大时是 O(源 × 清单)，留给 P5 的索引化实现。
6. **并发写入测试缝**：`capture_jsonl::set_post_read_hook` 是公开的测试缝（单进程无法真实制造读写竞争）。生产路径不安装钩子；采集器本身只读源，由用例 11 的只读属性证明。
7. **append 不重扫前缀**：前缀内"中间被改、首尾与探针窗口都不变"的情形不可见（同第 3 条）；`recordsDeduped` 把前缀已采记录计入，属口径说明而非重复提交。
8. **解析探针只在全量重捕时运行**：append 不做 `load_messages` 解析（保持增量成本与文件体积无关），故 `last_parse_error` 在 append 轮次不刷新。
9. **未实现**：SQLite 差分、附件、P5/P6、全文索引；也未实现真实数据试采（需单独授权）。

## 6. 结论

P2 的 6 项目标均有实现与用例证据，独立审计的 6 项发现已全部修复或显式披露，`cargo test` 全绿（283 passed / 1 ignored / exit 0），P1 既有断言的测试文件未改动且保持通过。真实数据：**否**。
