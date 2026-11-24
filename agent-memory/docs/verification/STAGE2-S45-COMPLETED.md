# STAGE2-S45 完成记录：SQLite 采集引擎的采集档案参数化（SqliteCaptureProfile）

- 状态：**完成**（2026-10-31，工作区 `C:\wt\stage2-s45`，分支 `stage2-s45-profiles`，基于 HEAD=320941e；未 git commit，由 orchestrator 提交）
- **真实数据 = 否**：全部测试使用合成 fixture 与临时目录（`cargo` 测试自身的 `tempfile` 根）；未读取任何真实会话/数据库/生产 Archive/真实配置（`<user-home>/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory`、`~/.hermes` 等全程未读）。cc-switch 源码 `<user-home>\Documents\GitHub\cc-switch`（公开源码，任务明确允许只读参考）仅阅读 `src-tauri/src/session_manager/providers/hermes.rs` 与 `mcode.rs` 的 SQL 与测试 fixture 以获取真实表结构，未运行 cc-switch、未接触其任何数据库。未重建 `assets/dist`；未运行真实 61/64/90。

## 1. 实现

| 文件 | 变更 |
|---|---|
| `agent-memory/src/capture_sqlite.rs` | 新增 `SqliteCaptureProfile`（字段见 §2）与 `SqliteCaptureProfile::OPENCODE`（与原常量逐项等价，单测锁定）；`OPENCODE_PROFILE`、`SQLITE_TABLE_WHITELIST`/`PARENT_FIRST_TABLE_ORDER`（别名，`pub static &[&str]`）、档案注册表（`OnceLock<RwLock>`）与 `register_sqlite_capture_profile`/`sqlite_capture_profile_by_id`/`profile_for_provider`；`watermark_column_in`/`whitelist_schema(profile,…)`/`schema_fingerprint_in`/`unknown_whitelist_schema_in`/`probe_schema(profile,…)`/`count_excluded_tables(profile,…)`；`read_all_rows`/`scan_table_keys`/`scan_session_ids`/`scan_session_keys`/`scan_child_keys`/`observe_connection`/`export_baseline`/`observe_one_table`/`observe_changes`/`BackfillExt`/`decide_plan`/`adopt_committed_unit`/`capture_baseline`/`capture_delta`/`run_verification_and_correct`/`verify_table_merge`/`verify_table_bounded`/`record_health_failure`/`finish_unchanged` 全部档案化；`ObservationFile` 增 `profile_id` 字段并在 `load_observation_in` 做档案一致性守卫（空=旧证据按 opencode，其他值必须与当前档案一致，否则拒绝→degraded 重做基线）；`scan_session_keys` 非主键会话表走 `row_key` 规则取活键集（键空间一致性，见 §4）；`observe_database` 保持原签名（OpenCode 档案）并新增 `observe_database_in`；既有单测调用的 `watermark_column`/`schema_fingerprint`/`unknown_whitelist_schema`/`load_observation` 保留为 `#[cfg(test)]` OpenCode 包装（既有测试零修改）；新增 5 条单测（§5）；模块文档新增「Capture profiles (S4.5)」节 |
| `agent-memory/src/archive/mod.rs` | `ManifestSqlite` 与 `SqliteState` 各增 `profile_id: String`（serde default；缺字段/空 = 旧证据按 opencode 解释） |
| `agent-memory/src/capture.rs` | `CaptureOptions` 增 `sqlite_profile: Option<&'static str>`（`None`=生产默认按 provider 选档案；未知 id 报错不回落） |
| `agent-memory/src/replay.rs` | `materialize_observation_in(profile,…)` 用档案 `replay_order` 做父表优先插入；`replay_sqlite` 从 `SqliteBaseline::profile()` 取档案；原 `materialize_observation` 保留为 `#[cfg(test)]` OpenCode 包装（既有测试零修改） |
| `agent-memory/src/bin/agent-memory.rs` | `CaptureOptions` 字面量补 `sqlite_profile: None`（CLI 行为不变） |
| `agent-memory/tests/p3_sqlite_profiles.rs` | 新增：Hermes 风格与 mcode 风格合成库的 8 条集成用例（§5） |
| `agent-memory/README.md` | 「SQLite evidence capture」新增「Capture profiles (S4.5)」小节（字段表、provider 选择与注册口、旧证据解释、档案切换语义、新框架接入=cc-switch provider+采集档案）；白名单/水位/父表顺序/会话过滤四处表述改为档案口径 |
| `CHANGELOG.md`（仓库根） | 顶部新条目（2026-10-31 S4.5） |

## 2. 采集档案字段表（`SqliteCaptureProfile`）

| 字段 | 含义 | OpenCode 档案值 |
|---|---|---|
| `profile_id` | 档案 id，写入 manifest（`ManifestSqlite.profile_id`）、采集状态（`SqliteState.profile_id`）与观测状态文件 | `"opencode"` |
| `tables` | 表白名单，有序 = 基线导出/分块顺序 | `session, message, part, session_v2, session_message, project, todo` |
| `replay_order` | 回放物化父表优先插入顺序 | `project, session, session_v2, message, session_message, part, todo` |
| `session_tables` | 会话表及会话键列 `(table, key column)` | `(session, id)`, `(session_v2, id)` |
| `session_time_column` | `--session-since` 作用的会话时间列（epoch 毫秒） | `time_updated` |
| `child_session_columns` | 子表及会话关联列 `(table, column)`；未列出的表永不按会话裁剪 | `message/part/session_message/todo` → `session_id` |
| `watermark_columns` | 水位列候选（有序），适用于无覆盖的表 | `time_updated, seq, time_created, created` |
| `table_watermark_overrides` | 按表水位候选（优先于档案默认） | 无 |
| `excluded_tables` | 显式排除表（文档化用途；白名单外本就只记 `{table, row_count}` 事实） | 无 |

合成档案（测试代码中定义，同为未来真实接入的形态）：

| 档案 | 表（白名单顺序） | 会话表/键列 | 会话时间列 | 子表关联 | 水位 | 显式排除 |
|---|---|---|---|---|---|---|
| `hermes-synthetic` | `sessions, messages` | `sessions`.`id` | `last_activity_at` | `messages`.`session_id` | 默认候选（该 schema 全为 REAL 时间戳，无整数水位列 → 走引擎声明过的全表哈希路径） | `auth_tokens`（合成凭据表，填哨兵） |
| `mcode-synthetic` | `local_runtime_sessions, local_runtime_messages, local_runtime_message_rows` | `local_runtime_sessions`.`session_id`（无声明主键，行键为 `rowid:N`） | `updated_at_ms` | `local_runtime_messages`.`session_id`、`local_runtime_message_rows`.`session_id` | 默认 `updated_at_ms→created_at_ms`；`local_runtime_message_rows` 按表覆盖为 `created_at_ms` | `local_runtime_message_row_migrations`（迁移记账，消息行本身是权威）、`mcode_credentials`（合成凭据表，填哨兵） |

两表结构按 cc-switch 源码逐列构造（hermes.rs 的 `CREATE TABLE sessions/messages` 测试 fixture、mcode.rs 的 `local_runtime_*` 测试 fixture），内容全部合成。

## 3. 档案选择与证据记录

- 生产路径 provider 决定：`profile_for_provider` 查注册表，未注册回落 OpenCode 档案（覆盖 `claude`/`codex` 及全部历史源）。`opencode` provider → OpenCode 档案。
- `register_sqlite_capture_profile(provider, &PROFILE)`：后续「新框架接入 = cc-switch provider + 采集档案」的预留注册口；本次不接入新 provider 的 CLI/capture-all 枚举（第二阶段之后的工作）。
- `CaptureOptions::sqlite_profile`：测试与接线用的显式选择口；未知 id 返回 `ArchiveError::Corrupt`（fail-closed，不回落）。
- 证据/状态记录：每次 baseline、delta、`--verify` 补录 manifest 均写 `profile_id`；`SqliteState` 与观测状态文件同步记录。
- 旧证据兼容：`profile_id` 为 serde default；反序列化缺字段/空 → 按 `opencode` 解释（`SqliteBaseline::profile()`、观测状态加载，均有测试）。
- 档案守卫：观测状态指向其他档案 → 视为不可信状态，拒绝 diff、报 `sqlite:observation_state_unreadable: …capture profile…` 异常并 degraded 重做基线（`observation_state_missing` 触发器）；crash adopt 拒绝采纳其他档案提交的单元。同形不同 id 档案切换：导出内容相同走既有 dedupe（不产生重复证据字节），状态以新档案重新落盘、运行报 degraded（`snapshot_deduplicated:observation_state_missing`），下一轮正常增量（集成用例锁定该完整序列）。

## 4. 仍保留的 OpenCode 专属/历史标识符清单与理由

| 位置 | 标识符 | 保留理由 |
|---|---|---|
| `capture_sqlite.rs` `SqliteCaptureProfile::OPENCODE` | 全部表名/列名字面量 | 这就是档案定义本身——引擎从档案读取，字面量集中在此一处（+别名静态项） |
| `capture_sqlite.rs` `SQLITE_TABLE_WHITELIST`/`PARENT_FIRST_TABLE_ORDER` | pub 别名 | 兼容既有引用（`replay.rs` 历史导入、外部使用者）；指向 OpenCode 档案同一切片，单测锁定逐项相等 |
| `capture_sqlite.rs` backfill 计算会话键列时的 `.unwrap_or("id")` | 会话键列兜底字面量 | 外层由 `profile.is_session_table(&change.table)` 门控，运行时不可达（验收 N2 补录） |
| `capture_sqlite.rs` `observe_one_table`/`verify_table_merge` 中 `.unwrap_or("session_id")` | 会话关联列兜底字面量 | 两处分支均先经 `scoped_child`/`child_scoped` 门控（该值为 `Some` 才可达），`unwrap_or` 仅满足类型检查、运行时不可达；与历史默认列一致以防未来门控回归 |
| `capture_sqlite.rs` `DuplicateTracker::observe` 的 `"data"`/`"content"` | 跨表重复载荷探测列 | 通用内容列名嗅探（跨任意框架的统计事实），非会话结构语义；mcode 的 `data_json` 等不在列，仅影响 `duplicatePayloadRows` 计数，不影响正确性 |
| `capture_sqlite.rs` `#[cfg(test)]` 包装与测试断言中的旧常量值 | `watermark_column`/`schema_fingerprint`/`unknown_whitelist_schema`/`load_observation`、等价性单测中的字面量 | 既有测试**零修改**是 OpenCode 等价性证明的一部分；包装委托到档案路径（OpenCode），生产代码不经过它们 |
| `replay.rs` `replay_sqlite`（derived）的 `["session_v2","session"]`、`row.get("id")`、`ses_` 形状检查、`providers::opencode::load_messages_sqlite` | 派生回读的 OpenCode 投影 | 引擎级 replay（`capture_sqlite::replay_sqlite`，本次测试所用）已与框架无关；把非 OpenCode 会话投影成 `DerivedSession` 需要各框架的读回投影器，属计划第二阶段之后的工作（同「semantic units 投影器」），不在 S4.5 范围 |
| `capture.rs`/`capture_all.rs` 的 provider 常量与 OpenCode 枚举根 | `SUPPORTED_PROVIDERS` 等 | 采集枚举是 S2 产物且任务明确本次不改；档案选择口（注册表 + options 覆盖）已就位 |

## 5. 测试

### 5.1 基线等价性

- `cargo test --locked --no-fail-fast`：**既有 678 passed / 3 ignored 一条未改、一个断言未动，全部通过**（改动前后各跑一轮全量对照：改动前 678/3 → 改动后（含新测试）691/3）。这是 OpenCode 档案逐项等价性的行为证明；另有单测 `opencode_profile_is_item_for_item_the_previous_constants` 逐字段锁定。

### 5.2 新增单测（`src/capture_sqlite.rs` tests，5 条）

| 用例 | 覆盖 |
|---|---|
| `opencode_profile_is_item_for_item_the_previous_constants` | OpenCode 档案与原常量逐项等价（表白名单顺序、回放顺序、会话表/键列、时间列、水位候选、子表关联、别名切片） |
| `profile_watermark_override_beats_profile_default` | 按表水位覆盖优先于档案默认；无匹配列 → None |
| `profile_registry_resolves_by_provider_and_id_with_opencode_fallback` | 注册表按 provider/按 id 解析；未知 provider 与 `opencode` 回落 OpenCode；未知 id miss |
| `evidence_without_profile_id_is_interpreted_as_opencode` | 无 `profileId` 的旧 manifest 反序列化 → 空 id → `SqliteBaseline::profile()` = opencode |
| `observation_state_without_profile_id_loads_under_opencode` | 旧观测状态（@2 无 `profile_id`）在 OpenCode 档案下可加载；指向其他档案的状态被显式拒绝 |

### 5.3 新增集成用例（`tests/p3_sqlite_profiles.rs`，8 条）

| 用例 | Hermes 风格 / mcode | 覆盖 |
|---|---|---|
| `hermes_profile_baseline_delta_and_credential_sentinel` | H | 首次基线（行数、排除表事实）；`profile_id` 写入 manifest/状态/观测状态；水位增量=增 1/改 1/删 1（全表哈希路径，内容改动无水位列也被发现）；delta manifest 记录档案；凭据哨兵在运行时根**原始字节 + 全部对象解压内容** 0 命中 |
| `hermes_profile_verify_pass_and_replay_match_the_source` | H | `--verify` 与增量同轮（哈希路径已捕获，验证 0 误报）；replay 与源逐行一致（sessions+messages 全表行集合相等） |
| `mcode_profile_session_since_baseline_and_credential_sentinel` | M | `--session-since`（按档案时间列 `updated_at_ms`）：仅 `updated_at_ms≥900` 的会话及其子行入库，范围外会话完全不入库；排除表 `{表,行数}` 事实（含凭据表）；`profile_id` 三处记录；哨兵 0 命中 |
| `mcode_profile_watermark_delta_insert_update_delete_and_replay` | M | 水位增量：新会话+新行（insert 2）、`updated_at_ms`/覆盖列 `created_at_ms` 推进的行与会话更新（update 2）、最高行删除（delete 1，行键集对比）；delta manifest 记录档案；replay 与源逐行一致（三张白名单表） |
| `mcode_profile_session_reentry_backfills_child_rows_without_false_deletes` | M | 会话重入（N2）：范围外会话 `updated_at_ms` 越过阈值 → 会话行 insert + 其水位之下的既有子行回填 insert（共 3），无误报删除（键空间一致性回归点）；已入选会话再被 touch（R1）→ 仅 1 条会话行更新、子行不重复入库；replay 一致 |
| `mcode_profile_blind_edit_is_corrected_by_verify` | M | 盲改（改 `data_json` 不动 `created_at_ms`）→ 常规观测 `unchanged`（声明盲窗）；`--verify` → 补录 1 条 update、`verifyMismatches=1`；correction delta（seq 2；unchanged 运行不占序号）记录档案；replay 一致 |
| `switching_profiles_never_diffs_across_profiles` | H+V2 | 同形不同 id 档案切换：观测状态被拒 → degraded（`snapshot_deduplicated:observation_state_missing`）、导出相同走 dedupe 不重复存证、状态重指新档案、下一轮正常 delta |
| `unknown_profile_override_fails_the_capture` | — | 未知 `sqlite_profile` id → 显式报错（fail-closed） |

哨兵扫描实现：递归枚举运行时根全部文件，逐文件断言原始字节不含哨兵；能被 zstd 解码的文件（对象/观测状态）再断言解压内容不含哨兵。

### 5.4 规模实测（release，前台跑完，与基线对照）

| 阶段 | 本次（S4.5 后） | 任务给定对照 | 结论 |
|---|---|---|---|
| 稳态增量（100 改 + 100 增 + 10 删，20 万行级源） | **1.024 s / 62 MiB peak** | ~1 s / 62 MiB | 无退化 |
| `--verify` @200k 行 | **4.086 s / 59 MiB peak** | ~4 s / 59 MiB | 无退化 |
| `--verify` @400k 行（2×） | 8.806 s / 109 MiB，增长 +86% | 预算 ≤300 MiB / ≤100 MiB 增长 | 通过 |
| 首次基线（25 万行，567 MiB 未压缩导出） | 2.734 s / 161 MiB（19 chunks） | — | 通过 |

## 6. 命令与退出码

| 命令（cwd `C:\wt\stage2-s45\agent-memory`） | 退出码 | 结果 |
|---|---|---|
| `cargo test --locked --no-fail-fast`（改动前基线） | 0 | 678 passed / 3 ignored / 0 failed |
| `cargo check --locked --all-targets` | 0 | 0 warning |
| `cargo test --locked --no-fail-fast`（改动后全量） | 0 | **691 passed / 3 ignored / 0 failed** |
| `cargo test --locked --test p3_sqlite_profiles --no-fail-fast` | 0 | 8 passed / 0 failed |
| `cargo test --release --locked --test p3_sqlite_scale -- --ignored --nocapture` | 0 | 1 passed；实测见 §5.4 |
| `git diff --check` | 0 | 无空白错误 |

## 7. 限制

- **Hermes 风格 schema 的 REAL 时间戳**：`sessions.last_activity_at`/`messages.timestamp` 为 REAL（epoch 秒），引擎水位读按整数毫秒语义取值（`as_i64`），因此该 schema 无可用整数水位列，走引擎既有声明路径——全表读 + 行哈希对比（OpenCode `project`/`todo` 同路径）；插入/更新/删除均被正确发现并有测试。**未新增 REAL→整数的水位换算能力**（属新引擎能力，超出「只把常量与列名改为从档案读取」的边界）。
- **`--session-since` 的单位契约**：过滤器是 epoch 毫秒；会话时间列值需与之同单位（mcode 的 `*_at_ms` 满足；Hermes 秒级 REAL 列不满足）。档案字段文档已写明该契约；秒级框架的真实接入需要单位映射，属后续接入工作。
- **键空间一致性修复的范围**：`scan_session_keys` 仅对「会话表主键 ≠ 会话键列」的形状改走 `row_key` 规则；重复键消歧序号在「候选读下界」与「过滤扫描」的 WHERE 边界不同这一历史边界（OpenCode 亦存在、仅重复主键会话表的病态形状）未改动。
- **同形不同档案切换走 dedupe**：内容相同则不重复存证、状态重指新档案、报 degraded——证据单元 manifest 保留原档案 id（如实记录采集事实）。异形档案切换必然 `schema_change`/`unknown_table_or_column` 重做基线。
- **mcode `visibility/archived/parent_session_id/session_kind` 列表过滤**是 cc-switch session_manager 的展示语义，不属于采集档案；采集按行全量/水位归档（隐藏会话同样是会话证据）。
- clippy 未随工具链安装（`rustup component` 未擅自添加），静态检查依赖 `cargo check --all-targets` 0 warning 与全量测试。

## 8. 未做（超出 S4.5 边界，均属第二阶段之后）

- 未接入任何新 provider 的 CLI/capture-all 枚举（Hermes/MCode 仅以合成档案验证引擎参数化）。
- 未做非 OpenCode 会话的派生读回投影器（`replay.rs` derived 层仍为 OpenCode 投影；见 §4）。
- 未做 REAL 时间戳水位换算、会话时间列单位映射等新引擎能力。
- 未改 `assets/dist`、未运行真实 61/64/90、未 git commit（由 orchestrator 执行）。
- [UNCERTAIN] 无遗留不确定项；cc-switch 侧仅按任务指示阅读了两个 provider 文件中的 SQL 与 fixture，若真实 `runtime-state.sqlite` 的建表语句含测试 fixture 未体现的主键/约束，接入时以真实 DDL 更新档案字段即可，不影响本次引擎参数化。
