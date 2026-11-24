# 累计回归清单（§G.6）

每个批次上线前，由独立 verifier 逐项勾选；勾选必须附**实跑证据**（命令、退出码、结果数字），
不得只凭本批新增功能通过就整单放行。清单只增不删：新增场景 / 变异 / 已知缺口各占一行，
修复后把「已知缺口」移到「已修复」并把对应场景改为等价判据。

最近更新：2026-11-10（构建修复批：重活闸门的两项 GUI 调用改为运行时解析（`LoadLibraryW` + `GetProcAddress`，不新增依赖），产物导入表不再含 `USER32.dll` / `SHELL32.dll`，解析失败照旧「可用但披露」；同时修正日志锁「删除中」的错误分类（verify-006-r2 O-1）：预算耗尽的 `Io(PermissionDenied)` 与 `LockHeld` 同样归入保尾、`save_records` 重试一轮后报 `LockHeld`，消除全量并行下的偶发失败）。

## 1. 历次独立验收的 P1 / P2 与遗留项

- [ ] **verify-001 P2-1 多载体零行遮蔽**：同一会话跨两个载体、零行载体排序在前时，后续载体的真实内容不得被跳过 — `tests/regression_matrix_archive.rs::archive_multi_carrier_session_projects_every_carrier`（两种顺序）、变异 `tests/mutations/a-multi-carrier-zero-row.patch`
- [ ] **O(K×M) manifest 读放大**：每个源每轮只解析一次 manifest 集，不随载体数增长 — `tests/archive_manifest_snapshot.rs::one_manifest_set_is_parsed_per_source_build_not_per_carrier`（K=3/12）、变异 `tests/mutations/b-manifest-per-carrier.patch`、`cost.counters.manifest_parse_count`
- [ ] **投影版本误触发全源重建**：投影代际按源区分，`claude_code` 换代只重建该源 — `tests/recall_claude_queued_command.rs::only_the_claude_index_rebuilds_when_the_projection_generation_changes`、变异 `tests/mutations/c-global-projector-version.patch`
- [ ] **第七阶段中途消息（verify-004）**：中途消息投影、去重状态机、`memory_read` 回读、`memory_recent` 尾窗、`audit-grounding` 冲突侧 — `tests/recall_claude_queued_command.rs`（7 条）、`src/providers/claude.rs` 单测（6 条）
- [ ] **第七阶段遗留 P3-1 / P3-2 / P3-3（⚠️ 未修复，仅需确认仍登记）**：词法回退预算截断丢命中、跨增量区间重复去重、投影统计口径不对称 — 套件 `CHANGELOG.md` ⚠️ 段；verifier 只核对「仍登记」，不要求本批修复
- [ ] **评测暂停标记**：pause 生效时 `index` 只跳过可索引源、报 `skipped_eval_in_progress`，不删标记 — `src/eval_pause.rs` 单测、`tests/eval_recall.rs`
- [ ] **`refresh-history.jsonl` 遗留（verify-001r2 P3-6 / P3-7）**：`tmp-<PID>` 残留与合法但缺 `finished_at` 的行 — `assets/scripts/test-refresh-cost-audit.ps1`、`assets/scripts/agent-memory-refresh.ps1`
- [ ] **多源结果按源轮流排列 / exact 从新到旧排序**（第七阶段 S4 基线发现）：登记在 `CHANGELOG.md` ⚠️，属检索排序计划，本清单只核对仍登记
- [ ] **`test_recall_onboarding.py` 在 OneDrive canonical 下清理失败**：仅环境性；非 OneDrive 工作树全过 — `assets/scripts/test_recall_onboarding.py`
- [ ] **verify-004a-b P1（降级写入后再升级）**：窄表缺值必须被检测并修复，`newest_unit_ts` 不回退，增量与重建等价 — `tests/regression_matrix.rs::index_repairs_narrow_columns_after_a_downgraded_writer`（两种检测路径：指纹形不符 / 记录缺失；整表重放，无重扫、向量不重算）
- [ ] **verify-004a-c P2（降级窗口内改名）**：旧二进制原地改 `units.title` 留下的旧窄标题与陈旧摘要必须自愈，增量（`doc_keys` + 向量）与重建等价 — 同上的第三种毁形（`narrow_relabel_sessions == 1`、`session_reprojections == 1`）与 `dsh_repairs_a_title_an_older_writer_changed_in_place`（`index.full_rescan.narrow_relabel == 1`，下一轮不再重扫）
- [ ] **verify-004a-b P3-1 / P3-2**：仅改名轮必须与重建等价（opencode 重投影、DSH 标题变更重投影）；`index-digest` 的向量状态区分 `unreadable` 与 `corrupt` — `opencode_rename_only_is_equivalent`、`dsh_title_change_reprojects_the_session`、`vector_digest_distinguishes_unreadable_from_corrupt`
- [ ] **verify-004a P3×6（上一轮返工修复，需逐条复核）**：身份瞬时失败不得擦除已记录身份、未提交尾部补全只读尾部、盲区措辞（中段永不探、与长度无关）、`index.mtime_only_refresh` 定义为「探针意义下未变」、降级轮计数描述尝试、DSH 行数倒退计入 `index.full_rescan.dsh_line_regression` — 见 §2 的「水位与身份边界」与「索引稳态读盘」两节
- [ ] **第三轮返工 P1（窄列回填二次方读盘）**：`index_migration` 的读盘必须与 `fts.sqlite` 体积呈线性 — `tests/regression_matrix.rs::index_migration_read_volume_is_proportional_to_the_file`、变异 `tests/mutations/f-narrow-backfill-per-row.patch`、`cost.counters.index.narrow_backfill_pages_read`
- [ ] **004b P1（opencode 每轮整表读）**：一轮不得再执行 `COUNT(*)` / `MAX(<ts>)` / 全表 `id` 集合；源文件与 WAL 未变时整轮不读宿主表（`index.opencode_unchanged == 1`、`index.opencode_host_rows_read == 0`）— `opencode_unchanged_round_skips_the_host_and_wal_changes_are_seen`、`opencode_round_cost_depends_on_the_change_not_the_corpus`
- [ ] **004b P1（新增行立即可见）**：水位新增 `max_rowid`；尾行按 rowid 读，不依赖时钟 — `opencode_tail_insert_is_visible_in_one_round`（`index.opencode_tail_rows == 1`、行的时间戳比全表都旧）、`index.opencode_max_rowid_queries == 1`
- [ ] **004b P1（原地更新 / 子行删除 / 整会话删除的替代保证）**：按会话滚动切片，窗口 `ceil(sessions / slice)` 轮；窗口内必须被发现 — `opencode_in_place_text_update_without_timestamp_is_equivalent`（第 1 轮不可见、第 2 轮可见）、`opencode_sub_row_delete_without_new_row_is_caught_by_the_rolling_slice`、`opencode_rolling_slice_covers_every_session_within_the_window`（96 会话，2/轮，第 46 轮收敛）
- [ ] **004b P1（`MAX(rowid)` 倒退）**：VACUUM 重排 / 库替换必须保守全量重建，不得从中间续读 — `opencode_max_rowid_regression_rebuilds_conservatively`（`reason == watermark_rowid_regression`）、`opencode_rowid_reuse_vacuum_and_touch_are_equivalent`
- [ ] **004b P1（旧水位升级首轮）**：水位缺 `max_rowid` 时旧路径只跑一次、不漏行，随后开一轮轮转 — `opencode_watermark_upgrade_round_is_conservative_and_equivalent`（`legacy_rounds == 1`、`rotation_open == 1`、旧路径看不见的原地编辑在轮转内补齐、下一安静轮走快速路径）
- [ ] **004b 返工 P2（原地清空投影的旧单元永久残留）**：宿主仍持有该消息行、但它已不可投影（`data = NULL`、非法 JSON、`content: []`、空文本块、涨过 64 MiB 上限）时，本轮必须删除该 message 的旧 live 单元（等价于 `prune_message(session, message, &[])`），全量 / 尾部 / 滚动核对三条路径一致；只匹配 `origin = 'live'`，不得误删 archive 行 — `opencode_in_place_emptied_projection_is_equivalent`（每轮 `unprojectable_units_removed == 1`、`stale_rows_removed == 0`、`host_rows_read == 2`、窗口内收敛且与重建等价）、`opencode_tail_row_that_cannot_project_drops_the_reused_message_units`（同 id 删后重插、新 rowid 走尾部、该会话不在本片）、`opencode_row_grown_oversize_in_place_is_equivalent_and_counted_currently`、`opencode_prune_never_drops_archive_origin_rows`、变异 `tests/mutations/i-opencode-unprojectable-not-pruned.patch`
- [ ] **005 P1（每个对象读取都全量解析 manifest）**：同一轮内 N 次对象读取只解析一次 manifest 集，稳态不解析；另一进程新提交的 manifest 立即可见（新对象不得被误判为孤儿），被改写 / 被删除的 manifest 必须重建快照而不是继续用旧内容 — `tests/archive_manifest_cache.rs`（4 条：解析计数、跨实例可见、删后重建、负例仍拒绝）、`cost.counters.manifest_read.parsed` / `manifest_snapshot.copies`、变异 `tests/mutations/j-manifest-per-object-read.patch`
- [ ] **005 返工（manifest 构建成本）**：一次构建 = 一次列举 + 每个 manifest 文件一次解析 + 每条对象引用一次表插入；记录索引只在首次 `check_record` 时构建；纯追加只解析新 manifest — `tests/manifest_build_cost.rs`（两规模：`manifest_index.object_refs == 提交对象引用数`、插入探测 ≤ 2×引用 且不随表增长（0.50 → 0.64 槽/条）；`manifest_index.record_refs == 0` 直到首次记录检查、之后 `record_builds == 1`；追加 3 个 manifest 只解析 3 个、`builds` 不增）、`src/archive/snapshot.rs` 单测（碰撞不改判定、扩表后 manifest 顺序恢复、记录索引惰性构建）、`archive::snapshot_stats()`
- [ ] **005 返工（访问顺序不变）**：索引判定前按 `(manifest, entry)` 恢复 manifest 顺序；哈希碰撞由候选 sha 比较过滤 — `src/archive/snapshot.rs::a_hash_collision_never_changes_the_decision` / `manifest_order_survives_table_growth`、`tests/archive_manifest_cache.rs::access_stays_fail_closed_for_wrong_domain_empty_tags_and_orphans`
- [ ] **005 P1（访问控制不得因缓存放宽）**：domain 不匹配、空 access 标签、无提交引用三种负例的拒绝与错误文案必须与父提交一致；快照只共享「解析结果」，不共享「判定结果」 — `tests/archive_manifest_cache.rs::access_stays_fail_closed_for_wrong_domain_empty_tags_and_orphans`（空标签用**追加**一个无 access 的提交来构造，严格交集仍拒绝）、`tests/p4_attachments.rs::access_mapping_is_required_and_fail_closed`（原地改写 manifest 抽掉 access 后必须重新读取并拒绝）
- [ ] **005 P1（`fill_before` 按键重复读同一 delta 批）**：多表多键共享同一批对象时，每个 baseline 块 / delta 批至多读一次；before 语义、capture 顺序、delete/update 处理、legacy 无 range 的保守读取不变 — `tests/capture_round_cost.rs::multi_table_updates_share_one_delta_batch_read`（两表更新共享一个批：`evidence_read.delta_batches <= 1`、`object_reads == distinct_objects`、`object_reads == delta_batches + baseline_chunks`）
- [ ] **005 P1（records 账本 / reconcile 按文件重复）**：`capture-all` 一轮内账本读一次写一次、reconcile 一次；单源命令每命令一次且仍报告自己的 `orphan_objects`；无变化轮不碰账本 — `tests/capture_round_cost.rs::one_records_read_and_one_write_per_capture_all_round` / `one_reconcile_per_capture_all_round` / `a_single_source_capture_still_reconciles_and_reports_orphans` / `record_readback_reuses_the_ledger_snapshot`、`report.ledgers`、`cost.counters.reconcile.calls`
- [ ] **005 P1（重活闸门）**：空闲 / 全屏 / 电源三信号的组合、48 小时强制、探针失败可执行但披露、显式 `--verify` 不闸门、被推迟的整表核验不推进 `lastVerifyAtMs`、闸门簿记写失败必须披露（`gate_state_unwritable` reason + `detection.gate_state`）、轻量路径不经过闸门 — `tests/heavy_gate.rs`（5 条，含 `an_unwritable_gate_state_file_is_disclosed`）、`src/gate.rs` 单测（6 条）、`assets/scripts/test_recall_onboarding.py::test_heavy_gate_wraps_only_the_daily_grounding`

- [ ] **006 P1（账本追加写不改变读者视图）**：常规轮只追加、不重写 canonical；`load_records` 合并 canonical + 追加尾且 canonical 键优先、尾部首行优先；缓存失效同时看两个文件戳 — `tests/records_journal.rs::a_capture_round_appends_and_leaves_the_canonical_alone`（canonical 字节逐一比对不变、追加尾 1 行、`recordsAppends == 1`、`recordsCompactions == 0`、追加 ≤ 384 B）、`the_journal_is_merged_first_writer_wins`（canonical 覆盖尾、尾部重复行不改映射、`records_tail_duplicate_lines ≥ 1`、账本仍等于回放）、变异 `tests/mutations/k-records-tail-ignored-on-read.patch`
- [ ] **006 P1（追加崩溃窗口）**：追加尾末尾半行只忽略不丢账，计数披露；下一次追加先截掉半行；重采后账本收敛到 manifest 回放 — `an_unterminated_last_line_is_ignored_and_repaired`（半行被截断 + 水位回退：合并只 1 条、`records_tail_incomplete_lines == 1`；修复轮 `recordsNew == 1`、尾部以换行结尾、`assert_ledger_matches_manifests`）
- [ ] **006 P1（坏完整行 fail-closed）**：完整行里的坏 JSON / 非法 sha / 缺字段必须报 `Corrupt` 且 CLI 退出 3，绝不静默丢条目 — `a_bad_complete_line_fails_closed`（三种形态 + `status` 退出 3 + stderr 含 `corrupt archive state`）
- [ ] **006 P1（压实崩溃窗口与写序）**：canonical 写成功、追加尾尚未删除时崩溃 → 账本不变（重复行忽略）；canonical 写失败时追加尾必须仍在（顺序断言）；先删尾再写 canonical 必须被抓 — `a_compaction_crash_after_the_canonical_write_is_safe`、`a_failed_canonical_write_keeps_the_journal`（把 canonical 换成目录让原子重命名失败；`recordsTailDuplicateLines == 1` / 尾部字节逐一相等）、变异 `tests/mutations/l-records-compact-deletes-tail-first.patch`
- [ ] **006 P1（阈值压实与显式命令）**：追加尾 > 1 MiB 在轮末自动压实（canonical 重写、尾删除、合并不变）；`compact-records` 的 dry-run 不写、真跑写、二次运行报 `nothing_to_compact`、并发第二个报退出 4 — `the_journal_threshold_compacts_at_the_end_of_the_round`（`recordsCompactions == 1`、`recordsCompactBytes > recordsAppendBytes`）、`the_explicit_compaction_command_writes_the_canonical_and_drops_the_journal`、`concurrent_compactions_never_lose_records`（4 线程、全部 Ok 或 `LockHeld`、最终 3 条且等于回放）
- [ ] **006 P1（成本：追加与总量无关、压实与总量成正比）**：常规轮追加字节 ≤ 常数 × 新增条数且与账本总条数无关；压实写入随总量 — `a_regular_round_writes_only_the_change_whatever_the_ledger_size`（40 / 4 000 条：两次追加字节相等且 ≤ 384 B）、`a_compaction_writes_the_whole_ledger`（两规模 `bytesWritten` 相差 > 10×）
- [ ] **006 P1（降级前必须压实）**：未压实时旧二进制（只读 canonical）看不到追加尾；压实后可见 — `a_downgrade_without_a_compaction_misses_the_journal`（`recordsBefore == 1` / `recordsAfter == 2`、压实后 canonical 2 条）
- [ ] **006 P1（读取点逐一冒烟）**：`records_contains` / `record_ref` / `read_record` / CLI `read --sha256` / CLI `status` totals 全部走合并视图；learning gate、learning qualify 的 counterexample ref、audit-grounding 的 archive 证据同样 — `every_read_path_serves_the_merged_ledger`、`tests/learning_gate.rs::a_record_that_only_lives_in_the_journal_passes_the_gate`、`tests/learning_qualify.rs::counterexample_ref_resolves_a_record_that_only_lives_in_the_journal`、`tests/audit_grounding.rs::learning_claim_grounds_against_a_record_that_only_lives_in_the_journal`
- [ ] **006 P1（005 轮级语义不变）**：一轮一次 flush、单源语义、`save_records` 整份替换（写 canonical 后清尾）— `the_005_round_semantics_are_unchanged`（首轮 `recordsAppends == 1` + `recordsCompactions == 1`；追加轮 1/0；安静轮 0/0）、`save_records_replaces_the_whole_ledger_and_drops_the_journal`、`tests/capture_round_cost.rs::one_records_read_and_one_write_per_capture_all_round`
- [ ] **006 P1（并发「追加 vs 压实」不丢**已提交**的账本条目，verify-006 P1-1 / verify-006-r P2-1）**：每一次日志写入（半行修复 + 写入 + fsync）与每一次删尾（比戳 + 内容判据 + 删尾）都持 `locks/records-tail.lock`；压实只为这个窗口持锁（一次 `stat` + 一次整读追加尾（≤ 1 MiB）+ 一次 `remove_file`），不覆盖 canonical 重写，采集不会被一次压实阻塞 — `tests/records_journal.rs::an_append_that_cannot_lock_the_journal_promotes_instead_of_writing_it`（预置锁文件后 120 ms 内不得完成、尾部不增长；有界等待后条目进 canonical、尾部仍不增长；清锁后追加路径恢复）、`tests/records_journal.rs::a_concurrent_append_and_compaction_never_loses_a_committed_ledger_entry`（400 次追加 × 120 次压实：合并后 401 条、逐条映射与首写者一致）、`src/archive/mod.rs::journal_concurrency_tests::an_append_cannot_land_inside_a_compactions_removal_window`（注入点把压实停在「比戳后、删尾前」，断言追加在该窗口内不落盘、放行后两条都在）、变异 `tests/mutations/m-records-unlocked-append-in-the-degraded-path.patch`
- [ ] **006 P1（降级路径：拿不到日志锁不写日志，verify-006-r P2-1）**：flush 等一个有界预算（250 ms，进程内只付一次）后**不无锁写追加尾**，改为在 `locks/records-compact.lock` 下把暂存条目提升写入 canonical（`records_promotions` / `records_write.promoted`；不删尾，与删尾互斥）；连压实锁也拿不到时本轮失败、条目留在暂存、水位不推进 → 下一轮重采并重新暂存 — `src/archive/mod.rs::journal_concurrency_tests::a_flush_without_the_journal_lock_promotes_the_entries_and_never_writes_the_journal`（残留锁 → 日志字节零增长 + 条目在 canonical + `records_promotions == 1` + 清锁后恢复追加）、`...::an_entry_flushed_while_a_compaction_removes_the_journal_is_never_lost`（注入点扣住删尾窗口：flush 有界失败、窗口内日志与 canonical 都不变、放行后重试提交且三条映射都在）、`...::a_failed_ledger_flush_leaves_the_source_watermark_unadvanced`（两把锁都占住 → `finish()` 失败、`sources.json` 未写；放锁重试后账本与水位都落盘）、`...::a_compaction_that_cannot_lock_the_journal_keeps_it`（外部持锁 → `tailRemoved=false` / `tailKeptBecauseLocked=true` / 尾部仍在；放锁后重试删尾成功）
- [ ] **006 P1（自动压实整轮持压实锁，verify-006-r）**：`maybe_compact_records` 的守卫必须覆盖「合并读 → canonical 重写 → 删尾」整轮，不能在 `match` 臂结束时释放（否则两个自动压实交错，旧 canonical 可覆盖新的、丢掉已被另一压实合并并删尾的条目）— `src/archive/mod.rs::journal_concurrency_tests::an_automatic_compaction_holds_the_compaction_lock_for_its_whole_run`（钩子在删尾窗口内起第二个自动压实 → 必须 `LockHeld`）、变异 `tests/mutations/o-records-auto-compaction-drops-the-lock.patch`
- [ ] **006 P1（并发「压实 vs 读者」不撕裂，verify-006 P2-1 / verify-006-r P2-2）**：合并读先取两文件戳、读完再取一次，两次不等则重试（8 次 / 1 ms），因此戳描述的字节范围等于合并读到的范围，**除非追加尾被同长度、同毫秒改写**（该形态在删尾侧由 `journal_is_the_one_merged` 的内容判据挡住；缓存侧可能多看一次旧视图，任一真戳变化即自愈）；重试耗尽返回旧戳下的**尽力合并**并计 `records_read.unstable`（不保证是某一历史时刻的完整状态，但其中每条都真实在盘上）。读者不拿锁，不会被写者阻塞 — `src/archive/mod.rs::journal_concurrency_tests::a_read_that_spans_a_compaction_retries_instead_of_mixing_two_states`（注入点让「另一进程压实」落在两次读之间：断言 `records_read.retries >= 1` 且结果含两条记录，不是「旧 canonical + 已消失的尾部」）、`tests/records_journal.rs::a_reader_never_sees_a_half_compacted_ledger`（2 000 条 canonical + 200 次追加 + 连续压实，40 次读：每次条目数 ≥「读之前已完成的追加数 + 种子」）、变异 `tests/mutations/n-records-read-accepts-first-attempt.patch`
- [ ] **006 P2（降级往返的映射差异如实描述，verify-006 P2-2）**：UPGRADE 不再声称「再升级后账本重新等于 manifest 回放」——可见性自动恢复，但降级期整份重采过的记录会保留降级期 `capture_id`，与回放的差异仅在 `(object, offset, len, capture_id)`，压实后固化 — `UPGRADE.md` §3.6「再升级后能自动恢复什么 / 恢复不了的差异（如实）」；台账见 `tests/records_journal.rs::a_downgrade_without_a_compaction_misses_the_journal`（可见性一侧）
- [ ] **006 P1（日志锁「删除中」按竞争分类，verify-006-r2 O-1）**：日志锁预算耗尽时，`Io(PermissionDenied)`（Windows 上 `create_new` 撞上正在删除的锁文件）必须与 `LockHeld` 同样归入「保尾、下次再删」，不得硬失败；压实 → `tailKeptBecauseLocked` + `records_compact.tail_kept_denied`；`save_records` 不能保尾（会复活要删的条目），重试一轮后返回 `LockHeld`，锁未取到前不写任何文件；真正的 I/O 失败（`NotFound` / `InvalidInput`）仍是硬失败 — `src/archive/mod.rs::journal_concurrency_tests::a_lock_that_is_being_deleted_counts_as_contention`（分类表）、`...::a_denied_journal_lock_keeps_the_journal_instead_of_failing_the_compaction`（锁路径占成目录 → 压实仍 `performed`、保尾、两条记录都在、清障后删尾）、`...::a_denied_journal_lock_refuses_the_replacement_and_writes_nothing`（`LockHeld`、canonical 字节不变、清障后替换成功）
- [ ] **005 构建修复（闸门不得静态链接 GUI，P6）**：`GetLastInputInfo`（`user32`）与 `SHQueryUserNotificationState`（`shell32`）必须**运行时解析**（`LoadLibraryW` + `GetProcAddress`，两者均在 `kernel32`），解析结果缓存（失败同样缓存）；库缺失 / 符号缺失 / 非 Windows → 该信号 `null` + `detection.<signal>` 的 `unavailable: …` 标签，可执行但披露，不静默跳过、不 panic；`GetSystemPowerStatus` 仍静态；release 产物导入表不得出现黑名单里的 GUI DLL，也不得出现动态 CRT — `src/gate.rs` 单测 `resolved_symbols_are_the_ones_the_probes_then_call` / `a_missing_gui_library_is_disclosed_and_never_fatal` / `a_missing_gui_symbol_is_disclosed_and_never_fatal` / `the_system_loader_resolves_both_gui_calls`、`agent-memory/build/build-release.ps1`（`Assert-AgentMemoryBinaryHardened`：`KERNEL32.dll` / `ntdll.dll` / `bcryptprimitives.dll` / `api-ms-win-core-synch-l1-2-0.dll`，双构建 SHA256 一致）、`tests/heavy_gate.rs`（闸门语义不变）

## 2. 本批场景矩阵（每项都断言等价判据 + 成本计数器）

### 索引：JSONL（Claude Code / Codex）

- [ ] 追加 — `tests/regression_matrix.rs::jsonl_append_is_equivalent_and_does_not_rescan`、`codex_append_is_equivalent`
- [ ] 只刷新修改时间 — `jsonl_mtime_only_refresh_keeps_the_content`（`index.full_rescan.* == 0`、`index.mtime_only_refresh == 1`、`index.probe_reads == 1`）
- [ ] 同长度改写 — `jsonl_same_length_rewrite_is_reread_and_equivalent`（`index.full_rescan.probe_mismatch == 1`、总和 == 1）
- [ ] 尾部窗口内（锚点窗口）的同长度改写 — `jsonl_tail_window_rewrite_is_detected`（头部探针不变，只有锚点探针不匹配；`probe_mismatch == 1`）
- [ ] 前缀改写（伴随增长） — `jsonl_prefix_rewrite_with_growth_is_reread_and_equivalent`（**原已知缺口，现断言等价**；`probe_mismatch == 1`）
- [ ] 截断 — `jsonl_truncation_is_rebuilt_and_equivalent`（`size_regression == 1`）
- [ ] 删除 — `jsonl_deletion_and_rename_are_equivalent`（删除轮 `index.full_rescan.* == 0`）
- [ ] 重命名 — 同上（改名轮 `index.full_rescan.new_file == 1`、总和 == 1）
- [ ] 同大小同修改时间的文件替换（轮转） — `jsonl_same_size_same_mtime_replacement_is_detected_and_equivalent`（**原已知缺口，现断言等价**；`file_identity_changed == 1`、总和 == 1；非 Windows 上该信号缺失，测试显式返回）
- [ ] 修改时间倒退且文件增长 — `jsonl_mtime_regression_with_growth_is_reread`（`mtime_regression == 1`）
- [ ] 升级后首轮（水位缺新字段，文件未变） — `jsonl_upgrade_round_fills_the_probes_without_a_rescan`（`index.full_rescan.* == 0`、`mtime_only_refresh == 0`、下一轮水位带探针与文件身份）
- [ ] 升级后首轮（水位整体不可信，例如缺 `mtime_ns`） — `jsonl_upgrade_round_without_watermark_fields_rebuilds`
- [ ] 旧格式水位条目反序列化（`#[serde(default)]` 三个新字段） — `tests/recall_index_stamp.rs::a_watermark_without_the_new_fields_deserializes_with_empty_probes`

### 索引稳态读盘（implementation-004a 返工；正文体积不得进入每轮读盘）

- [ ] 无变化轮不读 FTS5 正文 — `index_no_change_round_does_not_read_the_fts5_body`（同一单元数的两个 opencode 夹具：0.5 MB 与 24 MB 正文；`vectors.units_read == 0`、`sessions_reused == 1`、无重扫、`finalize` 阶段在位；两个规模的 `cost.read_bytes` 差值 < 2 MB；`EXPLAIN QUERY PLAN` 对 `COUNT_UNITS_SQL` / `NEWEST_UNIT_TS_SQL` / `CONVERSATION_METADATA_SQL` 都不得出现 `units`）
- [ ] JSONL 无变化轮零载体字节 — `jsonl_no_change_round_reads_no_carrier_bytes`（0.26 MB 与 12 MB 载体：`index.scan_bytes == 0`，两次 `read_bytes` 相同）

### 窄表与降级自愈（verify-004a-b P1 / P3）

- [ ] 降级写入后再升级（插入毁形）：窄表缺值被检测并修复 — `index_repairs_narrow_columns_after_a_downgraded_writer`（`index_repair` 阶段；`narrow_repair_rows` == 索引行数（整表重放）、无重扫、`sessions_recomputed == 0`、`newest_unit_ts` 不回退、窄值被修好、与重建等价；夹具 5000 行 × 1 KiB）
- [ ] 降级写入后再升级（改名毁形，verify-004a-c P2）：旧的非默认窄标题 + 陈旧 `doc_keys.digest` 被自愈 — 同上第三种毁形（`narrow_relabel_sessions == 1`、`session_reprojections == 1`、`full_rescan.* == 0`、窄标题跟上、与重建等价）+ `dsh_repairs_a_title_an_older_writer_changed_in_place`（载体未变；`index.full_rescan.narrow_relabel == 1` 且总和 == 1、下一轮为 0）
- [ ] 旧索引文件一次性加宽与回填（幂等，只跑一次） — `index_narrow_columns_are_backfilled_once`
- [ ] 回填读盘与 FTS5 体积呈线性（不是行数 × 体积） — `index_migration_read_volume_is_proportional_to_the_file`（5000 行 × 1 KiB 正文的合成旧布局索引；`index.narrow_backfill_pages_read × 页大小 ≤ 4 × fts.sqlite 体积`；实测 8 765 页引用 = 2.22×、53 ms（含迁移收尾写指纹的那次窄表扫描），修复前同夹具 1 266 679 页 = 321×；值真的回填、下一轮不再迁移或修补）
- [ ] 修补读盘与文件体积线性（与 `index_migration` 同口径） — `index_repairs_narrow_columns_after_a_downgraded_writer`（`index.narrow_repair_pages_read × 页大小 ≤ 8 × fts.sqlite 体积`；实测 5000 行 18 699 页 = 4.7×（24 ms）、15000 行 57 235 页 = 4.5×、61 单元 114 页 = 0.92×；常数比迁移的 4× 大，因为多一趟改名检测并连接）
- [ ] 仅改名轮（opencode） — `opencode_rename_only_is_equivalent`（`index.session_reprojections == 1`、`session_reprojection_rows == 2`、无重扫、与重建等价）
- [ ] 仅改名轮（DSH，`session/title` 后到） — `dsh_title_change_reprojects_the_session`（`index.full_rescan.dsh_title_changed == 1` 且总和 == 1、与重建等价）
- [ ] 向量缓存读失败 vs 解析失败 — `vector_digest_distinguishes_unreadable_from_corrupt`（独占句柄 → `unreadable`；非缓存字节 → `corrupt`；不存在 → `missing`）

### 水位与身份边界（verify-004a P3-1 / P3-2）

- [ ] 身份查询瞬时失败保留已记录身份 — `jsonl_a_failed_identity_query_keeps_the_recorded_identity`（用 `stamp::set_identity_unavailable` 测试缝；`mtime_only_refresh == 1`、无重扫、水位里的 `file_identity` 不变）
- [ ] 未提交尾部原地补全 — `jsonl_incomplete_tail_completion_is_equivalent`（等长、mtime 前进；`index.tail_completion == 1`、`index.scan_bytes == 尾部长度`、无重扫、与重建等价）

### 索引：DSH

- [ ] 追加 + 只刷新修改时间 + 代际切换 — `dsh_append_and_generation_switch_are_equivalent`（追加轮与只刷新时间轮 `index.full_rescan.* == 0`、`mtime_only_refresh == 1`；代际切换轮 `index.full_rescan.file_identity_changed == 1` 且总和 == 1）
- [ ] 增长但逻辑行数倒退 — `dsh_line_regression_is_a_counted_rescan`（`index.full_rescan.dsh_line_regression == 1` 且总和 == 1、与重建等价）

### 探针与文件身份（单元级）

- [ ] 探针覆盖头部与「截止到已提交偏移」的尾部窗口，追加不移动它们，两个窗口各自可被单独改写检出 — `tests/recall_index_stamp.rs::probes_cover_the_head_and_the_window_ending_at_the_offset`
- [ ] 空前缀 = 空摘要；文件不存在 = 无探针 — `probes_of_an_empty_prefix_are_the_empty_digests`
- [ ] 文件身份稳定、替换后改变、缺失路径无身份 — `file_identity_is_stable_and_changes_on_replacement`

### 索引：opencode（合成 SQLite）

- [ ] 新增行 — `opencode_new_row_title_change_and_deletes_are_equivalent`（每轮 `index.full_rescan.* == 0`、`manifest_parse_count <= 1`）
- [ ] 新增行立即（尾部）可见 — `opencode_tail_insert_is_visible_in_one_round`（新行时间戳比全表都旧，只靠 rowid 命中；`index.opencode_tail_rows == 1`、`tail_sessions == 1`、`unchanged == 0`、`slice_sessions <= 1`）
- [ ] 单元文本原地更新（同 doc_key） — `opencode_in_place_text_update_without_timestamp_is_equivalent`（**原已知缺口，现断言等价**；第 1 片不含该会话时仍不可见，第 2 轮收敛；`tail_rows == 0`、`slice_sessions == 1`、`reconcile_window == 2`、`reconcile_sessions == 2`、`reconcile_rows == 4`）
- [ ] 会话标题变更 — `opencode_new_row_title_change_and_deletes_are_equivalent`、`opencode_rename_only_is_equivalent`（`session_reprojections == 1`）
- [ ] 删除子行（尾部之外） — `opencode_sub_row_delete_without_new_row_is_caught_by_the_rolling_slice`（`tail_rows == 0`、`stale_rows_removed == 1`、`reconcile_sessions == 1`、不重建）
- [ ] 删除子行 / 删除会话（既有路径） — `opencode_new_row_title_change_and_deletes_are_equivalent`
- [ ] 整会话删除 — 同上（`sessions` 镜像差集，当轮删除其 live 行后交 archive fallback）
- [ ] rowid 复用 — `opencode_rowid_reuse_vacuum_and_touch_are_equivalent`
- [ ] VACUUM / `MAX(rowid)` 倒退 — 同上 + `opencode_max_rowid_regression_rebuilds_conservatively`（`reason == watermark_rowid_regression`、1 次 `max_rowid` 索引探测）
- [ ] 水位列推进（内容随之变化） — `opencode_rowid_reuse_vacuum_and_touch_are_equivalent`
- [ ] 未变化快速路径 — `opencode_unchanged_round_skips_the_host_and_wal_changes_are_seen`（`unchanged == 1`、`host_rows_read == 0`、`max_rowid_queries == 1`；WAL 写入与 checkpoint 都不得误判为未变）
- [ ] WAL 有新增内容 — 同上（写入后 `unchanged == 0`、`tail_rows == 1`）
- [ ] checkpoint 后 — 同上（`unchanged == 0`、`tail_rows == 0`、`db_stamp.mtime_ns` 前进）
- [ ] NULL `data` / 超大行 / 空会话 — `opencode_null_data_oversize_and_empty_session_are_equivalent`（`skipped_oversize == 1`、`tail_rows == 0`、`sessions == 3`、空会话无 stale 行、与重建等价）
- [ ] 原地清空投影（不推进时钟） — `opencode_in_place_emptied_projection_is_equivalent`（4 种形态：`data = NULL` / 非法 JSON / `content: []` / 空文本块；每轮恰好删 1 个旧单元、`unprojectable_rows == 1`、`stale_rows_removed == 0`、`reconcile_sessions == 1`、`host_rows_read == 2`；窗口内与重建等价）
- [ ] 原地涨过 64 MiB 上限 — `opencode_row_grown_oversize_in_place_is_equivalent_and_counted_currently`（删旧单元；`skipped_oversize == 1` 在**下一个**（另一会话的）轮次仍为 1，证明是「当前计数」而非「本轮目击」；缩回后窗口内归零；水位 `oversize_rows` 同步）
- [ ] 剪除只匹配 live 来源 — `opencode_prune_never_drops_archive_origin_rows`（同 message 的 `origin = 'archive'` 行在宿主行被原地清空后仍在）
- [ ] 尾部路径的不可投影行（同 id 删后重插） — `opencode_tail_row_that_cannot_project_drops_the_reused_message_units`（新 rowid 进尾部、该会话不在本片；`tail_rows == 0`、`unprojectable_rows == 1`、`unprojectable_units_removed == 1`、`stale_rows_removed == 0`、与重建等价）
- [ ] 旧水位升级首轮 — `opencode_watermark_upgrade_round_is_conservative_and_equivalent`
- [ ] 子行 `session_id` 迁移（rowid 不变） — `opencode_child_row_session_migration_is_equivalent`（`tail_rows == 0`、`slice_sessions == 1`、两轮后收敛、`stale_rows_removed == 1`、等价）
- [ ] 滚动切片跨轮覆盖全集，窗口内更新/删除被发现 — `opencode_rolling_slice_covers_every_session_within_the_window`（96 会话 / 2 每片 / 窗口 48；第 46 轮收敛，`stale_rows_removed == 1`，一轮共读 96（±2）会话、≤98 行；轮转结束后 `unchanged == 1`）
- [ ] 两规模成本 — `opencode_round_cost_depends_on_the_change_not_the_corpus`（48 会话 × 1 KB 与 × 200 KB：无变化轮 `read_bytes` 相差 < 2 MB 且 `host_rows_read == 0`；变化轮 `host_rows_read <= 3`）
- [ ] 内容不变触碰（同值 `UPDATE seq = seq`，不改变任何列值） — `tests/regression_matrix_capture.rs::opencode_capture_scenarios_verify_and_ledger` 的 7a（采集侧）

### 向量（fake 嵌入后端，两个会话）

- [ ] 冷缓存 / 无变化轮 — `vectors_follow_unit_text_title_and_context_changes` 第 1–2 步（冷缓存 `sessions_recomputed == 2`、`units_read == conversation_units.count`；无变化轮 `sessions_recomputed == 0`、`sessions_reused == 2`、`units_read == 0`）
- [ ] 单元文本更新 — 同上第 3 步（`sessions_recomputed == 1`、`sessions_reused == 1`、`units_read == 该会话单元数`）
- [ ] 会话改名 — 同上第 4 步（只重算该会话）
- [ ] 前一条用户发言变化（上下文漂移） — 同上第 5 步（整会话重算：`units_read` = 整个会话的单元数）
- [ ] 单元删除 — 同上第 6 步（`sessions_recomputed == 1`、`units_read == 1`）
- [ ] 每轮守恒 — `vectors.rows_reused + vectors.embedded == conversation_units.count`、`vectors.cache_bytes_read` 为数值、`index.full_rescan.* == 0`
- [ ] 旧格式向量文件（版本 2，无会话块）升级 — `vectors_previous_layout_is_upgraded_without_re_embedding`（`sessions_recomputed == 2`、`units_read == 全部单元`、`embedded == 0`、`rows_reused == 全部单元`、`status == updated`，随后一轮恢复为无变化）

### Archive 回填

- [ ] 只在归档中的会话 — `tests/regression_matrix_archive.rs::archive_only_session_is_projected_and_registered`
- [ ] 多载体同会话（零行在前 / 在后） — `archive_multi_carrier_session_projects_every_carrier`
- [ ] 零行会话 — `archive_zero_row_session_is_registered_and_not_rescanned`
- [ ] 行丢失 — `archive_lost_rows_are_rebuilt_from_the_frozen_evidence`

### 采集（每项后跑 `--verify` + 账本判据）

- [ ] initial / append / 只刷新时间 / 同长度改写 / 前缀改写 / 截断 / 改名 / 删除 — `tests/regression_matrix_capture.rs::jsonl_capture_change_classes_and_ledger`
- [ ] 升级后首轮（水位缺探针） — `jsonl_capture_legacy_watermark_without_probes`
- [ ] Codex 追加 — `codex_capture_append_and_ledger`
- [ ] opencode：新增行 / 文本更新 / 标题变更 / 删除子行 / rowid 复用 / VACUUM / 内容不变触碰 / 仅内容变化（verify 纠正）/ 删除会话 — `opencode_capture_scenarios_verify_and_ledger`
- [ ] CLI `capture --verify` 与账本判据 — `capture_cli_runs_verify_and_ledger`

### 采集轮成本（implementation-005，计数器断言；与具体机器无关）

- [ ] manifest 解析不随对象读取数增长 — `tests/archive_manifest_cache.rs::repeated_object_reads_parse_the_manifest_set_once`（3 对象 → 3 次解析；再来 30 次读取 → 0 次解析）
- [ ] 另一实例提交可见 + 删后重建 — `a_manifest_committed_elsewhere_is_visible_immediately`、`a_removed_manifest_rebuilds_the_snapshot_instead_of_going_stale`
- [ ] 多表共享一个 delta 批只读一次 — `tests/capture_round_cost.rs::multi_table_updates_share_one_delta_batch_read`
- [ ] 账本 / reconcile 每轮一次 — `one_records_read_and_one_write_per_capture_all_round`、`one_reconcile_per_capture_all_round`
- [ ] 闸门组合与 48 小时强制 — `tests/heavy_gate.rs`、`src/gate.rs` 单测

## 3. 变异检查（`assets/scripts/mutation-check.ps1`）

**逐批维护约定（必须与每批报告一起核对）：**

- 每批把 `-BaseCommit` 指向**新的父提交**（即本批工作树所基于的提交）；脚本无默认值，缺失即报错并列最近提交。
- 凡本批修改了某个补丁的目标区域，必须在同一批内重写该补丁，并在报告中重跑 `mutation-check.ps1`。
- 因上下文漂移导致 `git apply` 失败时**不得绕过**（不加大 fuzz、不跳过、不手工套用 hunk）：重写补丁后重跑。
- 本批记录：补丁 (d) 钉在 `build.rs::decide_jsonl` 的等长分支（返工时该分支新增了未提交尾部补齐，补丁已同批重写），补丁 (e) 钉在 `vectors.rs::build` 的会话复用判定；补丁 (f) 钉在 `mod.rs::backfill_narrow_columns`，恢复逐行写的缺陷形态。均已在 `ef2e7c6` 上重跑。implementation-004b 在 `b4d0afe` 上重跑全部 8 个补丁，并新增 (g)（opencode 滚动核对只追加：`work` 不含本片会话）与 (h)（滚动核对只读不删：去掉 `remove_stale_session_rows`）；返工新增 (i)（不可投影就跳过、不剪除：把 `drop_units` 变成空操作），在 `cc0b5ad` + 返工工作树上重跑全部 9 个。implementation-005 在 `1029f37` + 本批工作树上重跑全部 10 个：新增 (j)（`Archive::manifests()` 每次调用前丢弃快照，恢复每对象全量解析，钉 `archive/mod.rs::manifests`）；**重写 (b)**——本批把 manifest 解析下沉到 `Archive::manifests` 的共享快照后，只改 `fallback.rs::manifest_snapshot` 已不再产生解析放大（首轮实测为 `HOLE`），因此 (b) 改为两段：fallback 每载体重建 **+** 丢弃 archive 快照，仍由原用例 `one_manifest_set_is_parsed_per_source_build_not_per_carrier` 抓住。implementation-006 在 `6620620` + 本批工作树上重跑全部 12 个，新增 (k)（`records.rs::merge_first_writer` 直接返回 `MergeStats::default()`，尾部解析照旧但不再并入）与 (l)（`compact_records_locked` 把 `remove_tail` 提到 `atomic_write` 之前）；(k) 的 hunk 落在 `merge_first_writer`，(l) 的 hunk 落在压实写序，二者都由本批新增用例钉住。(k) 的失败面是「账本视图丢条目」，(l) 的失败面是「写失败后尾部已丢」；两者的 `ExpectStillPassing` 专门证明变异不是把构建或解析一起打坏。**005 返工**在 `0b27547` + 返工工作树上重跑全部 10 个（(j) 与 (b) 的 `manifests()` hunk 随新加的 `note_list()` / `cost::counter` 上下文重写）：**10/10 CAUGHT**。返工新增的哈希表内部保证（碰撞按 sha 过滤、扩表后顺序恢复、惰性记录索引完整）由 `src/archive/snapshot.rs` 的 5 条单测钉住，未新增补丁：碰撞与顺序缺陷无法用可移植补丁稳定翻转为集成用例失败。 **006 返工**在 `b7e0e2f` + 返工工作树上重跑全部 14 个：**重写 (l)**（压实写序的 hunk 随「比戳 + 删尾」移进 `locks/records-tail.lock` 与新增的 `tail_kept_because_locked` 报告字段重写）；**新增 (m)**（追加不取日志锁 → 集成用例「追加必须等锁」FAILED）与 **(n)**（合并读无条件接受首次尝试 → lib 单测「跨压实的读必须重试」FAILED，(n) 由新增的 `Target = 'lib'` 跑法驱动）；(m)(n) 的 `ExpectStillPassing` 分别钉住「合并规则未变」与「追加/删尾串行化未变」。**006 返工 2** 在 `9c576f0` + 返工工作树上重跑全部 15 个：**重写 (m)**（降级路径恢复「无锁写追加尾」→ lib 单测 `an_entry_flushed_while_a_compaction_removes_the_journal_is_never_lost` FAILED，该用例正是把压实扣在删尾窗口里、让无锁写入被一并删掉的交错）与**新增 (o)**（自动压实提前放掉压实锁 → lib 单测 `an_automatic_compaction_holds_the_compaction_lock_for_its_whole_run` FAILED）；两者的 `ExpectStillPassing` 分别钉住「内容判据未变」与「显式压实路径未变」。
- 补丁文件按 `.gitattributes` 固定 LF 检出（`agent-memory/tests/mutations/*.patch text eol=lf`）：`git apply` 的 hunk 解析不接受 CRLF 上下文，CRLF 检出会让所有补丁报 `corrupt patch`。
- **`Target`（可选）**：默认按 `Suite` 跑集成用例（`cargo test --release --test <Suite> <filter> -- --exact`）；`Target = 'lib'` 改跑 crate 内单测（`cargo test --release --lib <filter> -- --exact`），用于只有 `#[cfg(test)]` 接缝（如账本并发注入点）才能确定复现的缺陷。单测名按完整路径写（如 `archive::journal_concurrency_tests::…`）。**注入点是进程级的**：`ledger_probe::enter(point, root)` 把当前 runtime root 传给 hook，lib 测试并行时其它根的压实/合并读也会经过同一个注入点，hook 必须先按 `root` 过滤再置「已触发」，否则用例会因别的根的事件而假通过或假失败。

- [ ] (a) 恢复多载体零行遮蔽 — 期望 `archive_multi_carrier_session_projects_every_carrier` FAILED
- [ ] (b) 每个载体重读全量 manifest（O(K×M)） — 期望 `one_manifest_set_is_parsed_per_source_build_not_per_carrier` FAILED
- [ ] (c) 投影版本改为全局 — 期望 `only_the_claude_index_rebuilds_when_the_projection_generation_changes` FAILED
- [ ] (d) 同大小一律判未变 — 期望 `jsonl_same_length_rewrite_is_reread_and_equivalent` FAILED，且 `jsonl_append_is_equivalent_and_does_not_rescan` 仍 PASS（变异只打在等长分支）
- [ ] (e) 向量按 doc_key 复用陈旧行（忽略会话指纹） — 期望 `vectors_follow_unit_text_title_and_context_changes` FAILED
- [ ] (f) 回填恢复「边遍历 FTS5 边逐行 `UPDATE doc_keys … WHERE rowid = ?`」 — 期望 `index_migration_read_volume_is_proportional_to_the_file` FAILED，且 `index_narrow_columns_are_backfilled_once` 仍 PASS（变异只改成本、不改回填正确性）
- [ ] (g) opencode 滚动核对退化为「只追加」（本片会话不再读、不再删） — 期望 `opencode_in_place_text_update_without_timestamp_is_equivalent` FAILED，且 `opencode_tail_insert_is_visible_in_one_round` 仍 PASS（尾部路径未被改动）
- [ ] (h) opencode 滚动核对「只读不删」（去掉 `remove_stale_session_rows`） — 期望 `opencode_sub_row_delete_without_new_row_is_caught_by_the_rolling_slice` FAILED，且 `opencode_tail_insert_is_visible_in_one_round` 仍 PASS
- [ ] (i) opencode 不可投影行「跳过不剪除」（`drop_units` 变空操作） — 期望 `opencode_in_place_emptied_projection_is_equivalent` FAILED，且 `opencode_null_data_oversize_and_empty_session_are_equivalent` 仍 PASS（新增行本来就没有旧单元可剪）
- [ ] (j) 每次 `manifests()` 前丢弃快照（恢复每对象全量解析） — 期望 `repeated_object_reads_parse_the_manifest_set_once` FAILED，且 `a_manifest_committed_elsewhere_is_visible_immediately` 仍 PASS（变异只改复用、不改失效与访问判定）
- [ ] (k) 合并账本时丢弃追加尾（`merge_first_writer` 变成空操作） — 期望 `the_journal_is_merged_first_writer_wins` FAILED，且 `a_bad_complete_line_fails_closed` 仍 PASS（该用例的 canonical 由测试直接写入、只验证尾部解析与 fail-closed，不受合并缺陷影响；用 capture 型用例做对照会被「压实写出空 canonical」连带打挂，不构成有效对照）
- [ ] (l) 压实先删追加尾再写 canonical — 期望 `a_failed_canonical_write_keeps_the_journal` FAILED（Windows 上把 canonical 设为只读让原子重命名失败，此时尾部已被删），且 `a_compaction_crash_after_the_canonical_write_is_safe` 仍 PASS（顺序变异下成功压实的结果不变）
- [ ] (m) 降级路径恢复「无锁写追加尾」（拿不到日志锁时直接把行写进尾部，而不是提升进 canonical） — 期望 `archive::journal_concurrency_tests::an_entry_flushed_while_a_compaction_removes_the_journal_is_never_lost` FAILED（压实扣在删尾窗口时无锁写入被一并删掉，重试后条目仍不在盘上），且 `archive::journal_concurrency_tests::a_journal_rewritten_to_the_same_length_is_not_the_one_that_was_merged` 仍 PASS（内容判据未变异）
- [ ] (n) 合并读只取一次戳就接受（`read_ledger` 首次尝试无条件返回） — 期望 `archive::journal_concurrency_tests::a_read_that_spans_a_compaction_retries_instead_of_mixing_two_states` FAILED（`records_read.retries == 0` 且只读到 1 条），且 `archive::journal_concurrency_tests::an_append_cannot_land_inside_a_compactions_removal_window` 仍 PASS（追加/删尾串行化未变异）
- [ ] (o) 自动压实提前放掉 `locks/records-compact.lock`（`maybe_compact_records` 取锁后立即释放再跑整轮） — 期望 `archive::journal_concurrency_tests::an_automatic_compaction_holds_the_compaction_lock_for_its_whole_run` FAILED（钩子里的第二个自动压实会成功而不是 `LockHeld`），且 `archive::journal_concurrency_tests::a_compaction_that_cannot_lock_the_journal_keeps_it` 仍 PASS（显式命令的锁未变异）

## 3b. 计划 §G.2 中尚未实现的场景（登记，后续批次补齐）

- [ ] **并发写入中途读取** — 采集侧已有 `set_post_read_hook` 与 `source_shrank_during_read` / `source_rewritten_during_read` 降级路径，但矩阵尚未把它做成显式场景；由 implementation-005（尾部观测/滚动对账）补齐。
- [x] **records 账本压实中途崩溃（两种先后）** — 由 implementation-006 补齐：`tests/records_journal.rs::a_compaction_crash_after_the_canonical_write_is_safe`（canonical 已写、尾未删）与 `an_unterminated_last_line_is_ignored_and_repaired`（追加中途崩溃），外加 `a_failed_canonical_write_keeps_the_journal` 钉住「先写 canonical 再删尾」的顺序。

## 4. 已知缺口（必须仍在失败侧；修复后移出清单）

（无。implementation-004b 关闭了最后一条；`tests/regression_matrix.rs::no_scenario_is_a_known_gap_anymore` 钉住空清单。）

## 4b. 已修复的已知缺口（保留登记，改为等价判据）

- [x] `opencode.in_place_text_update_without_timestamp`（implementation-004b）：opencode 水位改为 `max_rowid` 尾部 + 每会话滚动切片；原地文本更新（同 rowid、时钟不前进）在 `ceil(sessions/48)` 轮内被发现。`opencode_in_place_text_update_without_timestamp_is_equivalent` 断言「第 1 片不可见、第 2 轮收敛、与重建等价」。仍然存在的边界：窗口是声明窗口（48 轮 / 24 h，小库为会话数轮），窗口内的可见性由 `detail.opencode.window_rounds` 披露。
- [x] `jsonl.same_size_same_mtime_replacement`（implementation-004a）：索引水位新增头部 / 锚点探针与 Windows 文件身份（卷序列号 + 文件 ID）。同大小同修改时间但**文件身份改变**（轮转 / 写临时文件后改名覆盖）现在整份重扫，`jsonl_same_size_same_mtime_replacement_is_detected_and_equivalent` 断言等价。仍然存在的盲区：同大小、同修改时间、同文件身份的**原地**改写（见 README「Carrier watermarks」的声明）。
- [x] `jsonl.prefix_rewrite_with_growth`（implementation-004a）：增长时先比较头部探针与截止旧偏移的锚点探针，任一不一致即整份重扫（`index.full_rescan.probe_mismatch`），`jsonl_prefix_rewrite_with_growth_is_reread_and_equivalent` 断言等价。

## 5. 固定命令（每批至少跑一次）

- [ ] `cargo test --release --no-fail-fast`（只增不减，基线数字写进报告）
- [ ] `cargo clippy --release --all-targets`（不新增告警）
- [ ] `assets/scripts/check-docs.ps1`
- [ ] `assets/scripts/mutation-check.ps1 -BaseCommit <本批父提交>`（无默认值）
- [ ] `agent-memory index-digest --config <recall.json> --json`（真实数据影子期对比增量与重建；差异按豁免清单机器判定）
