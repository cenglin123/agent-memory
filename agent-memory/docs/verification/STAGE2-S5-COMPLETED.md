# STAGE2-S5 完成记录：全文索引归档补缺（活源优先、归档补缺）

- 状态：**完成**（2026-10-31，工作区 `C:\wt\stage2-s5`，HEAD=d47f526；未 git commit，由 orchestrator 提交）
- **真实数据 = 否**：全部测试使用合成 fixture 与临时目录（`cargo` 测试自身的 `tempfile` 根；工作过程自建临时目录 `%TEMP%\s5-checks`）；未读取任何真实会话/数据库/生产 Archive/真实配置（`<user-home>/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory`、`~/.config/opencode` 全程未读）。未重建 `assets/dist`；未运行真实 61/64/90。Python 一律 `python -I`。

## 1. 实现

| 文件 | 变更 |
|---|---|
| `agent-memory/src/recall/fulltext/fallback.rs`（新增） | 归档补缺核心：`configured_archive_root`（与 capture-all `resolve_runtime_root` 同一规则：第一个 `kind=archive` 源，启用与否均可）；`FallbackContext::prepare`（开档案 + manifest 集合指纹 + 清单加载/推导）；carrier 清单（`CarrierGroup`：path/provider/kind/fingerprint/sessions/needs_infer/baseline_digest/capture_seq）与缓存（`<cache_root>/archive-inventory/current.json` + `sessions/<fp>.json`，`cache_store::write_atomic` 写入、旧指纹文件剪枝）；`apply_jsonl` / `apply_opencode`（补缺应用，失败降级为 `detail.archive.reason`，永不阻塞活源构建）；OpenCode 行镜像（`ensure_row_mirror`/`MirrorReader`/`apply_batches`/`MirrorState`：基线 digest 键、`last_capture_seq` 增量）；JSONL carrier 拼回（`rebuild_jsonl_bytes`：`jsonl_file` 按代际选 current generation 后按 `range_start` 拼接并校验连续性，`compressed_jsonl_file` 取 `decoded` 对象）；读侧 `ReadEvidence`（`rebuild_jsonl_carrier` 带 §3.2 carrier 校验、`materialize_opencode_session` 先便宜探测 session 表再物化）与 `read_fallback` 路由 |
| `agent-memory/src/evidence.rs`（新增） | 面向 recall 层的 SQLite 证据读投影 facade（`replay_sqlite_tables`、`sqlite_baselines`、`sqlite_delta_batch_metas`、`sqlite_delta_batches_after`、`read_baseline_rows_filtered`、`SqliteReplay`/`SqliteDeltaBatch` 等 re-export）——让 `src/recall` 只导入证据视图而不是采集引擎，`recall_boundary` tripwire（禁 `crate::capture`/`canonicalize(`/`fs::write`/`File::create`/`OpenOptions` 等）保持有效 |
| `agent-memory/src/capture_sqlite.rs` | `replay_sqlite` 重构为 `replay_sqlite_tables(archive, source, tables, upto, domain)` 的全表特例（表集合过滤：基线块与 delta 行按表过滤解压/保留，记账语义与未过滤版本一致）；新增 `read_baseline_rows_filtered`、`sqlite_delta_batches_after`；`sqlite_delta_batch_metas`/`read_delta_lines`/`SqliteDeltaBatchMeta` 改 `pub`（经 facade 供 recall 使用） |
| `agent-memory/src/archive/mod.rs` | **N5 修复**：`ReadHandle::read_record` 先判载荷语义再切片——`record_accessible` 返回 `batch_compressed`（记录所属 capture_id 的 manifest `source.kind == "sqlite"` ⇒ delta 批次对象是应用层 zstd、偏移指向解压后 JSONL），此时先 `zstd::decode_all` 再按偏移切片；末次 sha256 校验不变（fail-closed 不变） |
| `agent-memory/src/recall/refs.rs` | `Locator` 新增 `archive_fallback: Option<ArchiveLocator>`（serde skip_if none，既有 locator 序列化与 `hit_id` 输入不变）；`ArchiveLocator { record_sha256: Option<String> }` |
| `agent-memory/src/recall/contract.rs` | 新增 `ORIGIN_LIVE`/`ORIGIN_ARCHIVE` 常量 |
| `agent-memory/src/recall/fulltext/mod.rs` | 索引 schema 2→3：`INDEX_SCHEMA_VERSION=3`；`units` 增 `origin UNINDEXED` 列、`doc_keys` 增 `origin` 列（默认 `'live'`）、新增 `archive_sessions(session_id PK)`；`FALLBACK_RULE_VERSION="fallback-1"`；`Unit.origin` 字段；`IndexMeta.fallback: Option<FallbackMeta>`（rule_version/inventory_fingerprint/archive_root/archive_only_sessions，load/store 支持）；`open_writable` 旧布局迁移（meta schema_version 不等于当前即 DROP 已知表重建，FTS5 不能加列，索引本就是可删除缓存） |
| `agent-memory/src/recall/fulltext/build.rs` | `Writer`：insert 写 origin（units+doc_keys）与 `locator` 列（此前恒空）；新增 `remove_archive_session`（origin 感知删除，只删 `origin='archive'` 行）、`archive_rows_present`、`remember/forget_archive_session`、`live_sessions`（`doc_keys WHERE origin='live'`）；`scan_jsonl_file` 泛化出 `scan_jsonl_reader<R: BufRead>`（origin 参数：archive 行注入 `origin` + `archive_fallback` 标记，`record_sha256` = 行字节 sha256）；`codex_marker_from_reader`/`codex_marker_from_value` 抽取复用；`FallbackIdentity`（rule_version/archive_root/inventory_fingerprint）与 `rebuild_reason` 扩展（fallback 规则/归档根变化 → 重建；`fallback_removed` 同样重建；清单指纹变化只增量）；`index_source` 准备 `FallbackContext`，`build_jsonl`/`build_opencode` 在活源扫描后调用 `apply_*`，`meta.fallback` 与 `detail.archive` 落盘 |
| `agent-memory/src/recall/fulltext/row.rs` | `ROW_COLUMNS` 增 `origin`；`Row.origin`；`into_hit(…, mark_origin)`：仅补缺索引（meta 有 fallback）在 `ExactMeta.origin` 标 `live`/`archive`，其余索引命中不带 origin 键（字节级不变） |
| `agent-memory/src/recall/fulltext/query.rs` / `hybrid.rs` | 读取 meta 的 fallback 标志传给 `into_hit`；query 的 score 列序号改为 `ROW_COLUMNS_COUNT`（20，跟随后置 bm25 列） |
| `agent-memory/src/recall/search.rs` | `SearchResult.origin: Option<&'static str>`（skip_if none） |
| `agent-memory/src/recall/read.rs` | 回读路由：`memory_read_with` 在源身份/schema 校验后、活根读取前，对 `locator.archive_fallback.is_some()` 的 ref 走 `fallback::read_fallback`（strict-snapshot 显式拒绝）；`read_opencode` 拆出 `read_parts_after_resolve(&mut Reader, request)`、`read_codex`/`read_claude_code` 拆出 `scan_codex_stream`/`scan_claude_stream`（活源与归档共用扫描体）；`parse_ref` 的 locator 解析接受 sqlite-v2 / codex-rollout-v1 / claude-jsonl-v1 三个代际的可选 `archive_fallback` 键（其余代际与 `md-v1`/`archive-record-v1` 仍拒绝），`record_sha256` 做 64 位小写十六进制校验 |
| `agent-memory/src/recall/fulltext/coverage.rs` | `meta.fallback` 存在时 coverage 增 `archive_only_sessions` 与 `archive_root` |
| `agent-memory/src/recall/providers/mod.rs` | `ExactMeta` 增 `origin: Option<&'static str>` |
| `agent-memory/src/lib.rs` | `pub mod evidence` |
| `agent-memory/tests/recall_archive_fallback.rs`（新增） | 12 条集成用例（§5） |
| `agent-memory/README.md` | 「Live full-text index」新增「Archive fallback (stage-2 S5)」小节 |
| `CHANGELOG.md`、`assets/references/agent-memory-archive.md`、`assets/scripts/64-install-recall.ps1` | CHANGELOG 顶部新条目；参考文档 4.8 节 + `-ArchiveCaptureOnly` 行更新；64 `-ArchiveRoot`/`-ArchiveCaptureOnly` 帮助写明「归档内容经原源 id 进入结果，独立 archive 检索保持停用（启用会对同一会话重复回答）」 |

## 2. 设计决定

1. **清单缓存（可删除、以指纹为键）**：manifest 是 append-only 且 `capture_id` 唯一，清单指纹 = 有序去重 capture_id 集合的 sha256——只读 manifest 文件（JSON 解析，零对象解压）即可判定证据集合变化。每个 carrier 的会话清单再按内容指纹（选中 manifest 的 capture_id/generation/range_start/记录数/对象 sha）缓存于 `sessions/<fp>.json`，新采集只重新推导变化的 carrier。会话 id 规则与活源构建器一致：Claude 逐行 `record_meta.session_id`（无 sessionId 的行存在时经 `needs_infer` 在应用期做 rel 路径推断，避免文件名 stem 与记录 id 并存产生幻影会话）；Codex 首行 `session_meta.payload.id`（无 marker 才做文件名推断）；OpenCode 取 `session_v2` 行。`journal.jsonl` 与活源一致地排除。缓存整体可删除重建。
2. **OpenCode 按会话物化（不整库重放）**：清单只解压 `session_v2` 表（`replay_sqlite_tables` 表级过滤）；应用期对「待补会话」解压 `session_v2`+`session_message` 的基线块与 delta，物化进 `<cache_root>/archive-inventory/rows/<基线digest>.sqlite` 镜像（暂存 + 原子改名），后续运行按 `last_capture_seq` 只解压新 delta 批次增量推进；基线重做（digest 变化）才整镜像重建。单元生成直接复用 `opencode_units`（`data` 列原文字节做 `row_sha256`，投影/区间与活源行完全一致）。诚实限制：镜像体积趋近该源白名单 V2 载荷，可删除。
3. **locator 形态**：补缺行 locator = 活源形态坐标（OpenCode `session_id+session_message_id+pointer`；JSONL `session_id+message_id?+record_index+pointer`）+ `archive_fallback` 标记；JSONL 标记内含归档记录内容地址（= 行字节 sha256，与 `state/records.json` 同一摘要），回读用它在归档侧定位提交的 carrier 组并校验 §3.2 carrier 一致性（rel_path 必须映射到同一组）；OpenCode 无逐行记录对象，坐标在重放状态内寻址，标记本身即回读路由。`hit_id` 计算规则与活源完全一致（输入含 locator，故补缺命中与活源命中天然不同键；同一会话在索引内只有一份行集，不存在双命中）。`ref` 解析端只对三个可补缺代际接受该键，未知键依旧 fail-closed。
4. **身份与去重**：`IndexMeta.fallback` 携带补缺规则版本 + 清单指纹 + 归档根 + 补缺会话数。规则版本或归档根变化 → 整源重建（旧行按其他规则/其他证据解释毫无价值）；清单指纹变化 → 增量（新会话补入、回活会话的归档行按 origin 感知删除并清登记、登记会话行丢失时自愈重补）。同一会话（活源与归档都在）只保留活源行；`origin='live'` 行集是「活源现有」的判定来源（`doc_keys WHERE origin='live'`）。
5. **回读不改缓存根**：`memory_read`/`memory_search` 全程只读 `<cache_root>`；OpenCode 回读需要行状态时物化到系统临时目录（`%TEMP%/s5-readback-<pid>-<ts>/`，不用共享的 `%TEMP%\opencode`）用后即删；JSONL 回读在内存拼字节。测试 10 以运行时根逐文件哈希不变锁死该性质。
6. **N5**：不在 `RecordRef` 里加字段（旧 `records.json` 无迁移路径），而是按记录所属 capture 的 manifest `source.kind == "sqlite"` 判定批次载荷语义——对既有生产证据同样生效；普通 JSONL 记录切片路径字节不变。
7. **边界 tripwire 合规**：recall 层对采集引擎的访问经 `src/evidence.rs` facade（re-export 读投影），全文无 `crate::capture*`、`canonicalize(`、`fs::write`、`File::create`、`OpenOptions`、`atomic_write` 字样；缓存 JSON 经 `cache_store::write_atomic`（既有白名单通道），镜像 SQLite 由 rusqlite 落盘（tripwire 未禁止，行为上是缓存写）。

## 3. 测试用例表（`tests/recall_archive_fallback.rs`，合成 fixture：Claude 两转录 + Codex 普通/`.zst` rollout + OpenCode V2（含 V1 表）+ 临时 Archive 与 cache 根；经 `capture-all` 库 API 采集、`index_all` 建索引）

| 用例 | 覆盖 |
|---|---|
| `claude_deleted_session_is_indexed_from_archive_and_reads_back` | 删除活源转录 → index：exact 命中 `origin=archive`、活源命中 `origin=live`、缺省模式（auto→hybrid，fake 嵌入）命中；`memory_read` 回读逐字节等于原文且 `range_sha256` 校验通过；coverage 出现 `archive_only_sessions`/`archive_root` |
| `codex_deleted_and_compressed_sessions_are_answered_from_archive` | 删除普通 rollout + 从未可活的 `.jsonl.zst` 会话：两条都 `origin=archive` 且回读完整（`decoded` 对象拼回） |
| `opencode_deleted_session_and_sqlite_record_read` | V2（含 V1 表）库：先基线采集，再插入新行采集出 delta，然后删除会话 b 的行（不采集删除）→ index：exact+hybrid 命中 `origin=archive`、回读逐字节一致、活源会话 `origin=live`；`agent-memory read --sha256 <delta 记录>` CLI 退出 0、输出哈希校验通过且内容是 delta 行 JSON（N5） |
| `session_on_both_sides_appears_once_and_replacement_replaces` | 会话回归活源后：命中唯一且 `origin=live`，`archive_only_sessions` 归零（补缺行被替换） |
| `appended_session_reads_back_across_the_append_boundary` | append_range 增长过的 Claude 会话删除活源后：两条命中均 `origin=archive`，`context_after`/`context_before` 读取的上下文跨拼接边界完整 |
| `without_archive_source_behavior_is_unchanged` | 配置无 archive 源（capture 走 `--root`）：无 `detail.archive`、命中无 `origin` 键、coverage 无归档键 |
| `archive_source_enabled_flag_does_not_change_fallback` | archive 源 `enabled:false` 与 `true` 的补缺行为一致 |
| `cache_rebuild_is_byte_identical_under_the_fake_backend` | 删除整个 cache 根后重建：命中结果 JSON 一致、清单指纹一致 |
| `new_capture_changes_the_inventory_fingerprint` | 新采集（新文件）后清单指纹变化（缓存失效条件） |
| `mcp_calls_never_write_the_cache_root` | `memory_search` + `memory_read`（含补缺回读）前后 `<cache_root>` 逐文件 FNV 哈希不变 |
| `forged_fallback_ref_fails_closed` | 伪造 `record_sha256` 的补缺 ref → `REF_LOCATOR_MISSING`（carrier 校验 fail-closed） |
| `repeated_index_runs_stay_deduplicated` | 连续三轮 index：命中 hit_id 无重复、数量恒定 |

既有断言：**零修改**（基线 691 passed / 3 ignored 全过；其中 `units.locator` 列开始真实写入，活源 ref 与原坐标重建逐字节一致，等价性由既有 fulltext/fixture 测试锁定）。

## 4. 命令与退出码

| 命令 | 退出码 |
|---|---|
| `cargo check --locked`（worktree `agent-memory/`） | 0（0 warning） |
| `cargo test --locked --no-fail-fast` | 0：**703 passed / 3 ignored / 0 failed**（基线 691/3 + 新增 12） |
| `cargo test --locked --test recall_archive_fallback --no-fail-fast` | 0：12 passed |
| `python -I -m unittest discover -s . -p "test_recall_*.py"`（`assets/scripts/`） | 0：**73 passed**（test_recall_onboarding / test_recall_embedding / test_recall_eval；未改 Python） |
| `powershell`/`pwsh` + `Parser::ParseFile` 对 `assets/scripts/64-install-recall.ps1` | parse errors: 0（PS 5.1 与 PS 7 各一轮） |
| `powershell -File assets/scripts/check-docs.ps1` | 0：6 项全 PASS |
| `git diff --check` | 0（干净） |

说明：`test_desktop_source` / `test_memory_runtime` 两个文件存在与本次改动无关的既有环境性失败（需要 `assets/packages/*` 安装包与 desktop 公共脚本，工作区本就不全）——已在干净基线（`git stash` 后）复现同样的 7 条失败后恢复工作区，非 S5 引入。

## 5. 限制（已写入 README/参考文档）

- 补缺行只在「新会话进入补缺集、其行丢失、或整源重建」时刷新；待补会话的证据是冻结的（活源文件已消失或为不可增长的压缩载体），跳过是安全的。
- OpenCode：一旦某次采集**记录了删除**（delete delta 行被应用），该会话同时离开证据重放状态——补缺不再将其复活（字节仍在基线块/delta 对象层面可取证）；因此测试按「delta 先采集、删除后不采集」编排。
- OpenCode 行镜像体积趋近该源白名单 V2 载荷（生产 opencode V2 载荷约 GB 量级），位于 `<cache_root>/archive-inventory/rows/`，可删除重建。
- JSONL carrier 的提交字节区间不连续时跳过并计数（`detail.archive.carriers_skipped`），不猜测拼接。
- 独立 `kind=archive` 检索 provider 与活源命中身份不同：保持停用（`64 -ArchiveCaptureOnly`），启用会对同一会话重复回答（64 帮助与参考文档已同步）。

## 6. 未做

- 未做真实数据评估（S5 验收线「真实数据评估不低于活源基线 exact 0.864 / 合并 1.000」属 orchestrator 在生产根上的验证，真实数据=否）。
- 未重建 `assets/dist`、未刷新分发物谱系（由 orchestrator 统一执行）。
- 未运行真实 61/62/64/90；未注册/修改任何真实计划任务。
- 未删除独立 Archive provider（按要求保留并文档化停用建议）。
- 未执行 git commit / 分支操作（orchestrator 负责）。


## 返工 1（orchestrator 复核要求，2026-10-31）

初版 §5「限制」第 2 条（「一旦采集记录了删除，该会话离开重放状态，补缺不再复活」）与 S5 目标「会话可找回不再依赖各框架自己保存」冲突，本节以实现修正取代该限制；初版还有 OpenCode V1 表未覆盖的缺口，一并修正。前文 §1–§6 为初版记录，与本文冲突处以其为准。

### R1. 实现

| 文件 | 变更 |
|---|---|
| `src/recall/fulltext/fallback.rs` | 行镜像升级 `agent-memory/opencode-row-mirror@2`：① 表集合扩为 V1+V2 五张白名单表（`session_v2`/`session_message`/`session`/`message`/`part`，各加 `deleted_at_ms INTEGER NOT NULL DEFAULT 0` 列；extra 列不影响 V1/V2 读取器的 require 探测）；② **delete-keeping 语义**——`replay_state_keep_deletes`（基线块 + delta 批次的整状态回放）与 `apply_batches`（镜像增量推进）共用 `apply_delta_line_keep_deletes`：delete 行用 `before` 内容原样落行并记 `deleted_at_ms = delta.observed_at_ms`（`before` 缺失才物理删行，本引擎不产生该形态），insert/update 清零该戳；③ 清单 `extract_sqlite_group` 改为 V2∪V1 会话并集 = 基线会话行 ∪ delta 触及的会话 id（delete 按键保留），只解压会话表；④ `MirrorReader` 增 `v2_session_exists`/`v1_session`（含 `deleted_at_ms`），`session()` 带删除戳；⑤ `index_session_rows` 把 `deleted_at_ms > 0` 写入单元 locator 的 `archive_fallback.deleted_at_ms`；新增 `index_v1_session_rows`（V1Reader `parts_for_session` + provider 角色规则 + 共享块级投影器）；⑥ `apply_opencode` 会话分派：V2 行存在（含被删保留行）→ V2 路径，否则 V1 路径（同 id 以 V2 为准）；OpenCode「活源现有」改用 `Writer::opencode_live_sessions`；⑦ 读侧 `ReadEvidence::evidence_db`：优先只读使用当前行镜像（`MirrorState::is_current`：schema/源/基线 digest/无待应用批次），否则把删除保留状态物化到临时文件（用后即删，仍不写 `<cache_root>`），`read_opencode` 按 ref 代际走 V2Reader/V1Reader |
| `src/recall/fulltext/build.rs` | `opencode_units` 抽出共享块级投影器 `opencode_block_units`（text → 文本单元；tool → input/output 两单元，`range_*` 恒为整块投影，回读可验），V2 行构建器与新 V1 路径同源；`Writer::opencode_live_sessions`（doc_keys 活源行 ∪ `sessions` 表 − 登记表） |
| `src/recall/fulltext/row.rs` | 命中 ref 的 `schema_gen` 按 locator 形态判定：OpenCode 补缺行且 locator 带 `part_id` → `sqlite-v1`（V1 读取器可解析），其余仍按活源 kind |
| `src/providers/opencode.rs` | `sqlite_v1_message_roles(conn, session_id)`（provider 角色规则 `message.data.role`，missing → `"unknown"`）自 `load_messages_sqlite_v1` 抽取共用——补缺的角色赋值与移植解析同源，不另写解析器 |
| `src/recall/refs.rs` | `ArchiveLocator` 增 `deleted_at_ms: Option<i64>`（skip_if none，既有序列化不变） |
| `src/recall/read.rs` | ref 解析：`archive_fallback` 标记允许的代际扩为 sqlite-v2/sqlite-v1/codex-rollout-v1/claude-jsonl-v1（V1 分支键白名单加该键）；`deleted_at_ms` 整数校验 |
| `src/recall/search.rs` | 命中输出 `deleted_at`（RFC3339 UTC 秒精度，取 ref locator 的 `deleted_at_ms`；仅删除会话的补缺命中带） |
| `src/evidence.rs` | facade 增 `read_delta_lines`、`DeltaLine` |
| `tests/recall_archive_fallback.rs` | fixture 增 V1-only 会话（`v1-solo`：文本 + 工具 part）与 V1/V2 同 id 会话（`ses-both`）；12→14 条（见 R3） |
| `agent-memory/README.md`、`assets/references/agent-memory-archive.md`、`CHANGELOG.md` | 删除会话可找回（带 `deleted_at`）、V1-only 覆盖、限制同步 |

### R2. 设计决定

1. **删除找回 = delete-keeping 镜像**：delete delta 的 `before` 即删除前最后状态，镜像对 delete 「保留内容 + 记 `deleted_at_ms`」而不是删行；insert/update（会话同 id 重建）清零该戳并覆盖内容。清单随之把 delete 触及的会话 id 保留在补缺集合内（基线行 ∪ delta 触及 id）。命中 `deleted_at` 直接取该戳（RFC3339 UTC）；测试断言它等于该 delete delta manifest 的 `observed_at_ms`。
2. **活源优先的判定口径**：OpenCode 的「活源现有」= 活源 DB 会话表（索引 `sessions` 表，由活源扫描刷新）∪ doc_keys 活源行，再减登记表——只用 doc_keys 会把「消息全部被删但会话本体仍在活源」的会话误判为 archive-only（返工前的合成缺陷场景）。部分子行删除的会话因此永不产生归档行、不带 `deleted_at`；删除后同 id 重建的会话由活源行覆盖（活源扫描刷新 sessions 表 + 活源行入 doc_keys，登记表当轮清理）。
3. **V1 复用边界**：会话/行状态来自镜像的 V1 白名单表；单元生成走移植的 V1Reader（`parts_for_session`/`load_part`，即扫描与回读共用的移植 provider 读取器）+ 抽取出的共享块级投影器（与 V2 行构建器同一份 block→unit 解析）+ provider 抽取的角色规则；locator 为 V1 形态（`session_id`+`message_id`+`part_id`），命中 ref `schema_gen=sqlite-v1`（row.rs 按 locator 形态 + origin 判定），`memory_read` 以 V1Reader 回读并照常做 range/row 校验。V1/V2 同 id（已迁移 1918 个同形）以 V2 为准，V1 副本永不入索引。
4. **镜像版本守卫**：`mirror_meta.schema` 不等于当前 `MIRROR_SCHEMA_VERSION` 即整体重建（镜像可删除，无就地迁移）。
5. **读侧零写入**：回读优先只读打开当前镜像；镜像缺失/过期时把删除保留状态物化到 `%TEMP%/s5-readback-*` 临时文件并在读取后删除——`<cache_root>` 在 search/read 全程逐文件哈希不变（测试 10 锁定）。

### R3. 测试（14 条全过；基线 691/3 一条未改全过；全量 705/3/0）

| 用例（新增/修改） | 覆盖 |
|---|---|
| `opencode_deleted_session_and_sqlite_record_read`（升级） | delete delta 已提交：exact+hybrid 仍命中 `origin=archive`、`deleted_at` 存在且**等于该 delete delta 的 `observed_at_ms`**（ISO 秒 UTC）、`memory_read` 回读逐字节等于删除前内容；同 id 在活源重建后 → 唯一命中 `origin=live` 且无 `deleted_at`；N5 CLI `read --sha256` 用例不变 |
| `opencode_v1_only_sessions_are_covered`（新增） | V1-only 会话（文本 + 工具 part）exact/hybrid 命中 `origin=archive`、ref `schema_gen=sqlite-v1`、locator 为 `session_id+message_id+part_id`、`memory_read` 回读逐字节一致；V1/V2 同 id 会话唯一命中且 `origin=live`，其 V1 副本文本零命中 |
| `opencode_partial_child_deletion_keeps_live_priority`（新增） | 只删除活源会话的部分子行并采集（delete delta 已提交）：该会话 `origin=live`、无 `deleted_at`、不产生归档行（`archive_only_sessions` 只含 V1-only 会话） |
| 其余 11 条 | 不变，全过（含 MCP 前后 cache 根哈希不变、缓存重建一致、伪造 ref fail-closed、三轮无重复） |

既有断言：**零修改**（初版 S5 的 12 条中仅 `opencode_deleted_session_...` 按返工要求升级；基线 691/3 全过）。

### R4. 命令与退出码

| 命令 | 退出码 |
|---|---|
| `cargo check --locked` | 0（0 warning） |
| `cargo test --locked --no-fail-fast` | 0：**705 passed / 3 ignored / 0 failed** |
| `cargo test --locked --test recall_archive_fallback --no-fail-fast` | 0：14 passed |
| `python -I -m unittest discover -s . -p "test_recall_*.py"`（assets/scripts） | 0：73 passed |
| `git diff --check` | 0 |
| `assets/scripts/check-docs.ps1`、64 脚本 PS5.1/PS7 `Parser::ParseFile` | 通过 / 0 错误（返工未再改 .ps1，复核通过） |

### R5. 限制（取代初版 §5 第 2 条；其余不变）

- **基线前删除不可恢复**：会话在本源首个基线之前已删除的，证据中没有任何行内容，补缺无能为力（字节层面也不存在）。
- 部分子行删除的活源会话：活源优先，不把被删子行混回活源会话；被删子行本身在活源索引中随之消失（与活源一致），其历史内容仍在镜像/证据层可按 record 取证。
- 行镜像体积接近该源白名单 V1+V2 载荷，可删除；`deleted_at` 取证粒度为 delete delta 的观测时间（采集周期对齐，非精确删除瞬间）。


## 返工 2（独立验收 B1–B4 修复 + N1–N4，2026-10-31）

验收报告 `STAGE2-S5-VERIFICATION.md`（DeepSeek-flash 跨模型族 verifier，对象 6e5294c）判定「阻断需修复」。本节逐条对应修复；前文与本文冲突处以本文为准。

### B1 OpenCode 增量删除对账（`src/recall/fulltext/build.rs`）

- **会话级**：`refresh_sessions` 从「仅 full 时清空」改为**每轮重建**（`DELETE FROM sessions` + 重插活源会话清单）——被删会话当轮离开 `sessions` 表（N4 的 `coverage.sessions` 随之正确）。行小且全程单事务，代价 O(活源会话数)。
- **行级**：增量路径（`!full`）新增对账——对索引中每条 `origin='live'` 且 `message_id` 非空的行，向活源库做一次 `EXISTS(SELECT 1 FROM session_message WHERE id = ?)` 主键探测，不在活源的行当轮移除（`remove_rows`）。**取舍**：每已索引行每轮一次主键探测（线性、无排序、无大内存），替代「整表主键集合载入内存」或「对索引表 ORDER BY 排序归并」；50k 行实测见 §B4（探测成本被镜像构建淹没，秒级总量）。
- 效果：被删会话当轮落入补缺（`origin=archive` + `deleted_at`，同一轮）；被删子行不再以 live 死命中返回（修复 L2 起既有缺陷「被删行仍 live 命中且 `memory_read` REF_LOCATOR_MISSING」，verifier 基线对照确认该缺陷自 d47f526 即存在）。
- 测试：`opencode_incremental_session_deletion_with_growth`（删会话 + 其它会话增 2 行、总行数上升 → 仍切 archive + `deleted_at` + 回读一致；增长会话新行 `origin=live`）、`opencode_incremental_child_row_deletion`（删子行 + 另一会话增 1 行、总行数不变 → 该行零命中、会话仍 live、`coverage.sessions` 正确）。

### B2 回到活源立即切回 live（`build.rs`）

每轮（`!full`）用活源会话主键集合（`load_sessions`）与补缺登记表求交，交集中的会话立即 `remove_archive_session` + `forget`，并**按 session_id 定向重扫**活源行（`WHERE session_id = ?`，与时间水位无关），投影走与水位扫描共享的 `project_opencode_buffer`（本函数自原增量投影体抽取共用）。恢复行的旧时间戳不再影响切回。测试：`opencode_return_to_live_with_old_timestamps`（原 `time_created=3` 低于水位恢复 → 唯一命中 `origin=live`、无 `deleted_at`）。

### B3 多 session carrier（`build.rs` + `fallback.rs`）

取报告建议的第一案：`scan_jsonl_reader` 增 `session_filter: Option<&BTreeSet<String>>` 参数（活源路径传 `None` 行为不变），`apply_jsonl` 重建 carrier 时传入**该 carrier 的待补会话白名单**——白名单外会话的单元跳过，carrier 重建不可能给仍由活源服务的会话写归档行，不变量「同一会话只有一份」闭合。测试：`multi_session_carrier_only_missing_session_archived`（shared.jsonl 含 sha-ses-a/b 两 session、solo.jsonl 只含 sha-ses-a；删 shared → sha-ses-b `origin=archive`、`multi-shared-a` 零命中、solo 副本 `origin=live`、直接查索引 `sha-ses-a` 归档行数 = 0）。

### B4 镜像构建/回读物化成本（`fallback.rs`）

- `replay_state_keep_deletes` 改为 `&mut Connection` + **单事务包住全部基线行与 delta 行**（原逐行自动提交 = 每行一次 fsync，verifier 实测 ~7.4 ms/行）；新增 `tune_rebuildable_cache`（`PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF`）——镜像是可删除重建的缓存，重建中途崩溃只丢弃 staging 文件；`journal_mode=OFF` 只作用于构建/物化连接，改名后的镜像被增量 `apply_batches` 打开时回到默认 journal（小事务保留回滚能力）。回读缺镜像时的临时物化同一路径（同一函数）。
- 实测（测试 `opencode_row_mirror_scale_is_seconds`，50 会话 × 1000 行 = 50 000 行 `session_message`，删 1 会话 + capture 后首轮 index；同机同 fixture 形态）：

| 场景 | 验收报告（修复前） | 修复后实测 |
|---|---|---|
| 首次 index（镜像构建） | 363 242 ms | **6 515 ms**（≈56×） |
| 缺镜像回读（一个归档 ref） | 41 720 ms | **1 369 ms**（≈30×） |

- 测试断言两者 < 60 s（防机器波动），实测数字由 `--nocapture` 输出。

### N1–N4

- **N1**：`fallback_temp_file` 返回 `(file, dir)`；新增 `cleanup_readback_temp`（`remove_file` + `remove_dir_all`，错误打到 stderr 不吞、不影响读结果）；成功/失败路径都清目录，Windows 上不再遗留 `s5-readback-*` 空目录。
- **N2**：CHANGELOG S5 条目的「703 passed」标注为初版值（最终值见返工条目），消除同条目双终值。
- **N3**：三处表述复核——README「deleted and re-created … answers from its live rows」、完成记录 §R5「被删子行在活源索引中随之消失」、参考文档 4.8「已删除会话的补缺命中仍为 origin=archive」在 B1/B2 修复后均与实测一致（分别由 B2/B1-b/B1-a 测试锁定）；README 与 4.8 增补「每轮增量删除对账 + 回活定向重扫」的行为说明。
- **N4**：`coverage.sessions` 随 `refresh_sessions` 每轮重建而与活源一致（B1-b 测试断言）。

### 命令与退出码

| 命令 | 退出码 |
|---|---|
| `cargo check --locked` | 0（0 warning） |
| `cargo test --locked --no-fail-fast` | 0：**710 passed / 3 ignored / 0 failed**（基线 691/3 与既有断言零修改；S5 测试 14→19 条） |
| `cargo test --locked --test recall_archive_fallback -- --nocapture` | 0：19 passed，B4 实测输出见上 |
| `python -I -m unittest discover -s . -p "test_recall_*.py"`（assets/scripts） | 0：73 passed |
| `git diff --check` | 0 |
| `check-docs.ps1` / 64 脚本解析 | PASS / 0 错误（返工 2 未改 .ps1） |

### 限制

- 行级对账成本为「每已索引活源行每轮一次主键探测」，随索引行数线性（50k 行总量仍在秒级）；若未来生产索引行数达到数亿量级，可改为按水位读取 delete delta 键（需活源构建器读归档，当前刻意解耦）。
- `journal_mode=OFF`/`synchronous=OFF` 只用于镜像构建与临时物化（可重建缓存，崩溃丢弃重建）；镜像增量 `apply_batches` 保留默认 journal。
- B4 断言阈值 60 s 为防机器波动放得较宽；实测远低于该值。


## 返工 3（对账/清单/镜像成本达标 + N1/N4/N3/N2/NB-5，2026-10-31）

复验报告 `STAGE2-S5-REVERIFICATION.md`（verdict：通过）留下一组 non_blocking 成本与文档项：P1（22 万行无变化 index 12× 基线）、N1（镜像命中路径遗留空临时目录）、N4（`coverage.sessions` 混入归档会话）、N3（文档断言与实测不符）、N2（CHANGELOG 数值指针断裂）、NB-5（崩溃残留 `.building.<pid>` 不回收）。返工 3 指令目标：**清单 O(N) 冷构建 ≤3 min、无变化检查 ≤1 s、稳态 index ≤5 s、行镜像只物化待补会话、N1/N4/N3/N2/NB-5**。本轮工作曾因宿主会话重启中断一次，在既有未提交改动基础上续作完成；前文与本文冲突处以本文为准。

### R3.1 实现

| 文件 | 变更 |
|---|---|
| `src/recall/fulltext/fallback.rs` | ① `EvidenceBatch`（新增）：manifest 快照 + 一次计算的严格访问交集（复用 `archive_provider::object_access`），`read_object` 语义与 `ReadHandle::read_object` 逐条一致（无引用/域不可见即 fail-closed），对象批量读取从 O(carriers × manifests) 降到 O(carriers)；② 无变化检测改 **manifest 文件名目录列举**（`Archive::manifest_names`，文件名内嵌唯一 capture_id 且 append-only → 排序名集合即证据集合指纹），证据集合不变时零 manifest 正文解析；③ `extract_sqlite_group` 重写为流式：只解压会话表 chunk（id 即取，不物化行）+ delta 只看会话表行，经 `_in`/`_fetch` 批量读取器；④ **行镜像只物化待补会话**（schema 升 `agent-memory/opencode-row-mirror@3`，`MirrorState` 增 `sessions` 持有清单）：`ensure_row_mirror(…, wanted)` 生命周期 = 全新构建（wanted 会话的基线行 + 该代际 delta 过滤应用）/ 回填（新待补会话 = 基线行抽取 + 全代际 delta 过滤重放，行内容完备故幂等）/ 收缩（离开 wanted 的会话行删除）/ 推进（待应用 delta 过滤应用），全部单事务；基线选择按 group digest 查找、证据前移时回落最新代际并把实际 digest 记入镜像文件名与状态；⑤ 读侧 `evidence_db` 缺镜像时按**该会话**物化临时库（`insert_baseline_session_rows` + `apply_session_deltas`，不再整状态回放）；`replay_state_keep_deletes`/`apply_batches` 整库物化路径删除；⑥ **N1**：`cleanup_readback_temp(&temp, &temp_dir)` 移到成功/失败路径外无条件执行（镜像命中路径不再遗留 `s5-readback-*` 空目录）；⑦ **NB-5**：`clean_stale_staging` 在镜像构建前清理同基线 digest 的 `.building.<pid>` 残留 |
| `src/recall/fulltext/build.rs` | **P1**：增量删除对账从「每已索引行一次 `EXISTS` 主键探测」改为「单表扫描一次载入活源 `session_message` 主键集合 + 内存 diff」（线性、无逐行查询；见 R3.3 实测） |
| `src/recall/fulltext/coverage.rs` | **N4**：`coverage.sessions` 排除 `archive_sessions`（只计活源会话；补缺会话继续单独记在 `archive_only_sessions`） |
| `src/capture_sqlite.rs` | 批量读取器：`sqlite_baselines_in`/`sqlite_delta_batch_metas_in`（预解析 manifest 切片）、`read_chunk_rows_fetch`/`read_delta_lines_fetch`（调用方提供对象 fetcher）、`ObjectFetch` 类型、`BaselineRowLine` 转 `pub`；`sqlite_baselines*` 增 `capture_seq > 0` 过滤（基线判定更严） |
| `src/archive/mod.rs` | `manifest_names()`（目录列举，不读对象、不解析正文） |
| `src/evidence.rs` | facade 收敛为 recall 实际消费的读投影（`read_chunk_rows_fetch`/`read_delta_lines_fetch`/`sqlite_baselines*`/`sqlite_delta_batch_metas*` 及配套类型）；移除 recall 不再使用的 `replay_sqlite*`/`sqlite_delta_batches_after`/`SqliteDeltaBatch`/`SqliteReplay`/`read_delta_lines` re-export |
| `src/recall/refs.rs` + `src/recall/read.rs` | **新发现修复**（见 R3.4）：`Locator::same_coordinates`（忽略 `archive_fallback` 路由标记的坐标比较）；上下文窗口的命中定位检查改用它 |
| `tests/recall_archive_fallback.rs` | `read_with_context` 辅助 + Claude 拼接边界与断言（带上下文回读须命中）；N4 严格钉死（`coverage.sessions == 3`，此前 `>= 2` 覆盖不到） |
| `agent-memory/README.md`、`assets/references/agent-memory-archive.md` | **N3**：镜像体积（只物化待补会话 + 实测数字）、对账成本（内存 diff）、`coverage.sessions` 口径、清单指纹来自文件名列举、镜像构建前清理残留、带上下文回读行为 |
| `CHANGELOG.md` | **N2**：S5 条目补记返工 2 的 710 终值（此前只存在于本文件）、修正「最终值见下方返工条目」指针；新增返工 3 条目（当前终值） |

### R3.2 设计决定

1. **成本三案归一**：P1 对账（逐行探测）、清单冷构建（每对象全清单重扫）、无变化检查（每次解析全部 manifest 正文）共享同一形态病——把「批量读取」退化成「逐次全局扫描」。修复也归一：manifest 快照只解析一次（`EvidenceBatch`）或完全不解析（文件名指纹），对象访问映射一次计算，主键集合一次载入内存。三者都保持 O(N)（N = 证据字节 / 行数），不引入排序或大内存（主键集合 ≈ 220k × ~30 B ≈ 10 MB 量级）。
2. **按需镜像的幂等回填**：delta 行是内容完备的（insert/update 带 `after`、delete 带 `before`），从基线 capture_seq 起按序重放该代际全部 delta（过滤到 wanted 集合）对已知会话是幂等的（`INSERT OR REPLACE` + delete 保留语义重放产生相同终态），因此「新待补会话回填」不需要记录每会话已应用的 seq——镜像状态只需 `last_capture_seq`（推进水位）+ `sessions`（持有清单）两个量。
3. **镜像收缩即删除行**：会话回活源后从 wanted 集合离开，其镜像行删除（索引侧归档行本就由 `apply_opencode` 移除）；镜像文件保留（空表），下一次该 digest 需要时走回填路径。镜像整体仍可删除重建。
4. **读侧零 `<cache_root>` 写入不变**：镜像只读打开；缺镜像/镜像未持有该会话时，临时物化只取该会话的行（千行会话秒级、~MB 以下），用后即删（N1 修复后成功路径也无残留）。
5. **`same_coordinates` 而非放任相等**：上下文定位检查需要「同一坐标」，而 `archive_fallback` 是路由元数据（含 record_sha256/deleted_at_ms），不描述坐标；比较前剥离该键，既修复上下文回读，又不变 `hit_id`/序列化（输入与规则未动）。

### R3.3 规模实测（专用目录 `C:\wt\s5r3-work\`，合成 fixture `r3-*` needle，debug 构建，`python -I` 驱动 CLI，长任务前台等待；world-220k = 220 会话 × 1000 行 = 220 000 行 WAL，world-440k = 440 000 行）

| 场景 | 实测 | 目标 | 结论 |
|---|---|---|---|
| 清单冷构建（删缓存后冷 `index`，1 manifest / 22 万行） | **24.18 s** | ≤3 min | 达标 |
| 清单冷构建（42 manifest / 222 039 行，41 次增量采集后） | **27.48 s** | ≤3 min | 达标；42× carrier 数只增 ~3 s（旧形态为 carriers × manifests 二次病） |
| 冷 `index` @ 44 万行（数据翻倍） | **49.65 s**（≈2.05×） | O(N) | 随数据量线性 |
| 无变化检查（无任何变化的 `index` ×3 @ 22 万行） | 0.758 / 0.786 / 0.841 s，中位 **0.786 s** | ≤1 s | 达标（返工 2 复验同规模 4.28 s 中位） |
| 无变化检查（1 个补缺会话登记后 ×3） | 0.812 / 0.811 / 0.829 s | ≤1 s | 达标 |
| 稳态 index（+2000 行增长 + delta 证据 @ 22 万行） | **1.08 s** | ≤5 s | 达标 |
| 稳态 index（44 万行 + 2000 行增长） | **1.82 s** | ≤5 s | 达标（随行数线性） |
| 无变化 `index` @ 44 万行 ×3 | 1.55 / 1.52 / 1.53 s | （参考） | 线性（2× 行数 ≈ 2× 耗时） |
| 行镜像只物化待补会话 | 删除 1 个千行会话 → 镜像 **311 296 B**、`sessions_held=1`、行数恰为该会话 1+1000（活源 219 会话 0 行） | 只物化待补 | 达标 |
| 镜像增量推进 | 40 次增量采集后镜像 311 296 B 不变、`last_capture_seq` 42 | 增量 | 达标 |
| 镜像随待补集增长 | 删除第 2 个会话后 **544 768 B**、`sessions_held=2` | 只物化待补 | 达标 |
| 整源白名单载荷（对照，插值自复验 B4 的 6.05 MB @ 5 万行） | ≈26 MB | — | 按需镜像 ≈ 整源载荷的 ~1.2% |
| NB-5 崩溃残留 | 预置 `<digest>.building.999` → 触发镜像工作轮后消失 | 回收 | 达标 |
| 删除会话端到端 | `memory_search` 命中 `origin=archive` + `deleted_at`；`memory_read` 带上下文（±1）返回 2 段、needle 在文中 | 回读 | 达标 |

说明：本轮与复验非同机同 fixture，绝对值不可直接跨表比较；同机同世界的形态对比以本表为准（对账 + 清单 + 检查合计中位 0.786 s，对照同机基线形态下复验测得的 0.357 s 量级，S5 增量开销收敛到约 2× 以内，不再有 12× 形态）。

### R3.4 新发现并修复（返工 3 端到端检查）

**归档 ref 带上下文的 `memory_read` 必然 `REF_LOCATOR_MISSING`**：上下文窗口的「命中部分在其会话内」定位检查把 `archive_fallback` 路由标记计入 locator 相等比较，而镜像/拼回读取器产出的候选 locator 是活源形态（无该标记），等值永不成立——S5 起带上下文的补缺回读全部失败（无上下文的回读正常，既有 19 条用例均未覆盖带上下文路径，故未暴露）。修复：`Locator::same_coordinates`（剥离 `archive_fallback` 后比较），上下文定位检查改用之；新增带上下文回读断言（Claude 拼接路径 + OpenCode 镜像路径经 CLI 端到端复核：`origin=archive` + `deleted_at` + 上下文段齐全）。

### R3.5 命令与退出码

| 命令 | 退出码 / 结果 |
|---|---|
| `cargo check --locked`（`agent-memory/`） | 0（0 warning） |
| `cargo test --locked --no-fail-fast` | 0：**710 passed / 3 ignored / 0 failed**（基线 691/3 与既有断言零修改；S5 用例 19 条 + 带上下文/严格 N4 断言升级） |
| `cargo test --locked --test recall_archive_fallback` | 0：19 passed |
| `python -I -m unittest discover -s . -p "test_recall_*.py"`（`assets/scripts/`） | 0：73 tests OK |
| `git diff --check` | 0（干净） |
| `powershell -File assets/scripts/check-docs.ps1` | 0：6 项全 PASS |
| `python -I C:/wt/s5r3-work/scale_steps.py …` / `e2e_check.py` | 各步 0；数据见 R3.3 |

### R3.6 限制与未做

- 中断续作说明：返工 3 曾因宿主会话重启中断，`EvidenceBatch`/按需镜像/N1/N4/NB-5 的主体改动已在工作区，续作完成了 `row_matches_wanted` 会话表按键过滤修复（中断遗留的 5 条 OpenCode 用例失败根因：V1 `session` 行被误按 `session_id` 过滤）、P1 内存 diff、基线代际回落与状态一致性、上下文回读修复、文档与 CHANGELOG。
- 行镜像 `MIRROR_SCHEMA_VERSION` 升为 @3：旧 @2 镜像按可删除缓存处理，下一轮补缺自动重建（无就地迁移，与前两版一致）。
- 未做真实数据评估、未重建 `assets/dist`、未运行真实 61/62/64/90、未执行 git commit（同前几轮，由 orchestrator 统一执行）。
- NB-6（活源时间戳回填低于水位的行需 `--rebuild`）属既有活源扫描模型，不在本轮范围（复验已判定非本次返工引入）。
