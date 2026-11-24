# Archive P4 完成记录 · 附件与完整内容引用

范围：计划 `20261024-agent会话原始证据归档.md` 的 **P4** 阶段——归档本地图片、
附件与持久化工具输出，建立 `ContentRef` → 内容哈希对象的原始映射；缺失、越界、
无权限、超限对象全部给出显式状态；不访问外部 URL。**P5/P6 未做**（无 provider 级
覆盖对账、无派生重放、无套装离线安装）。

真实数据：**否**。全部用例只使用 `tempfile` 临时根与合成文件/数据库，未指向、未
读取任何真实会话目录、真实数据库或真实附件。

Git：**未提交**（按任务要求 executor 不执行 `git commit`；工作区改动待
orchestrator 提交）。

## 1. 实施文件

| 文件 | 动作 | 职责 |
|---|---|---|
| `agent-memory/src/attachment.rs` | 新增 | P4 引擎：ContentRef 解析（JSONL / SQLite / JSON part file / sidecar）、路径规范化与根边界、外部 URL 分类、data URI 解码、大小限制、流式入档与哈希复核、显式状态、manifest 映射提交、单写者锁、last-run 健康 |
| `agent-memory/src/archive/mod.rs` | 修改 | `Manifest.attachments[]` / `ManifestAttachment`（原始 ContentRef + 对象地址 + 状态）；`SourceState.attachment` / `AttachmentState`；`AttachmentCounts`；`AttachmentSummary`；`put_object_stream` / `verify_object_stream` / `finalize_staged_object`（分块写入与校验，`OBJECT_CHUNK_BYTES = 64 KiB`）；`ArchiveError::LimitExceeded`；`ReadHandle::read_attachment`；`attachment_entries` / `find_attachment`；空 access domain fail-closed；`source_captures` 排除 attachment 清单 |
| `agent-memory/src/coverage.rs` | 修改 | 每源 `attachments` 块（manifest 派生的 lifetime 计数 + 最近一轮健康）；总量 `attachments` / `attachmentBytesTotal`；清单来源并入 coverage 行（状态丢失时仍可见，报异常）；`ok` 只看最近一轮 |
| `agent-memory/src/capture.rs` | 修改 | `Manifest.attachments` 空初始化；`canonical_key` 供 P4 复用 |
| `agent-memory/src/capture_jsonl.rs` / `src/capture_sqlite.rs` | 修改 | `Manifest.attachments` 空初始化（向后兼容，行为不变） |
| `agent-memory/src/lib.rs` | 修改 | 模块注册；Claude sidecar 格式矩阵说明指向 `src/attachment.rs` |
| `agent-memory/src/bin/agent-memory.rs` | 修改 | 新增 `attach` / `attachments` / `attachment-read`；`status` 增加 attachment 计数；usage 更新 |
| `agent-memory/tests/p4_attachments.rs` | 新增 | P4 验收用例 24 项（全部合成临时根） |
| `agent-memory/src/archive/mod.rs`（`#[cfg(test)]`） | 修改 | 新增 2 项流式对象单元用例（分块一致性、超限不落盘） |
| `agent-memory/README.md` | 修改 | P4 模型、状态、路径规则、限制、权限、覆盖口径、CLI、布局与原子性 |
| `CURRENT.md` | 修改 | 任务状态 |

未触碰：`src/providers/**`、`src/recall/**`、`src/mcp.rs`、`src/codex_*`、
`src/model.rs` 的模型定义、`tests/p0_formats.rs`、`tests/p1_archive.rs`、
`tests/p2_jsonl_incremental.rs`、`tests/p3_sqlite_consistency.rs`、`tests/recall_*`、
`tests/support/**`、安装脚本与分发物、Obsidian 库。

## 2. 数据模型

- **映射是采集事实**：每次 attachment 采集提交一个 `source.kind = "attachment"`
  的 manifest，`attachments[0]` 保存**原始 `ContentRef`（逐字段原样）**、target
  （`content|local_file`）、readMode、规范化后的绝对路径（仅文件类）、mediaType、
  显式 status、对象内容地址（仅 archived）、archived/observed 字节数、冻结的
  access 标签、dedupe 标志、当轮上限与错误原因。清单格式仅追加字段，P1–P3 清单
  继续可读。
- **被拒引用同样提交清单**：每次尝试（包括 `unauthorized`）都落一条 attachment
  manifest；被拒条目没有对象、保留原因，`unauthorized` 条目 access 为空。因此
  拒绝结果可审计、可计数，回读对非 archived 条目一律 fail-closed。
- **两种物化方式**：
  - `content`：归档引用**定位到的字节**。JSONL 指针解析到字符串值（超长工具
    输出、内联 base64 文本）；空 JSONL 指针归档整条记录字节；SQLite 列归档
    text（`sqlite_text`）或 blob（`sqlite_blob`）；空 `file` 指针归档 part 文件
    本身；`data:` URI 解码为二进制（`inline_base64`）。
  - `local_file`：把定位到的字符串当作本地路径，归档该文件字节（持久化工具
    输出、图片、附件）。
- **对象去重**：相同字节只存一个内容寻址对象；后续引用在清单中记
  `deduplicated = true`。引用数不等于证据副本数；coverage 的 `bytesStored`
  按去重后的实际新增字节计。
- **源删除不影响回读**：映射在对象写入并复核后才提交；源文件（甚至会话源）删除
  后，`read_attachment` 仍按引用逐字节回读（用例 3、19）。
- **崩溃顺序**：解析（只读）→ 流式写入 staging → 内容哈希 + 复核 → manifest
  提交（映射提交点）→ last-run 健康状态。提交前崩溃最多留下孤儿对象（
  `reconcile` 上报，不自动删除），不会出现“有状态无证据”的映射。

## 3. 显式状态（不静默省略）

每个引用恰好得到下列之一，并写入清单；被拒引用无对象、保留原因：

| status | 含义 |
|---|---|
| `archived` | 对象已落盘并通过流式哈希复核；映射可回读 |
| `missing` | 引用指向的本地文件不存在，或 SQLite 行/列为空 |
| `path_traversal` | 相对引用的 `..` 离开解析基准（root） |
| `out_of_bounds` | 绝对路径（或符号链接/junction 目标）落在所有授权根之外；UNC 网络路径同样拒绝 |
| `unauthorized` | 请求没有 access 标签，或路径解析没有授权根（fail-closed） |
| `over_limit` | 文件大小或定位内容超过上限；未写入任何对象字节 |
| `external_reference` | `scheme://` 外部 URL：只记录、永不抓取 |
| `unsupported_reference` | 指针不存在/非字符串、SQL 标识符非法、表/列未知、非 base64 data URI、sidecar 当作 content 等 |
| `read_failed` | 权限、IO、SQLite 锁定/不可读、目标是目录等 |
| `source_missing` | in-source 引用的物理源已不存在 |
| `record_mismatch` | 提供了 `recordSha256` 且与记录字节不符（fail-closed，不入档） |

## 4. 路径与边界规则

- `source`、`roots`、绝对引用必须是绝对路径；无当前目录兜底（相对路径在
  `capture_attachment` 入口即 `Corrupt` 拒绝）。
- `roots[0]` 是**所有相对引用**的解析基准（sidecar、file 内指针值、JSONL/SQLite
  解析出的路径值一致）。先用组件级规范化（`..` 在基准深度为 0 即
  `path_traversal`），再要求结果位于至少一个授权根之下；存在时再 `canonicalize`
  一次，阻止符号链接/junction 把读取带出根外。
- 绝对引用只做词法规范化后判定根边界；不在任何根下 → `out_of_bounds`，**不打开
  目标文件**（用例 6、7 断言对象库无该内容、源文件未被读取归档）。
- 根比较在 Windows 上大小写不敏感，并剥掉 `\\?\` verbatim 前缀；以 `\` 结尾的
  根与非结尾根等价。
- 外部 URL 识别：`data:`、`file://`、以及其他任意 `scheme://`。`data:` 仅
  `content` 目标接受（base64 解码）；`file://` 仅 `local_file` 目标接受（不做
  百分号解码、不做 UNC 映射）；其余 scheme 一律 `external_reference`，无任何
  网络代码路径。

## 5. 大小限制与分块

- 默认上限 `DEFAULT_MAX_ATTACHMENT_BYTES = 32 MiB`，请求可用 `maxBytes` 覆盖
  （`0` = 默认）。文件先按 metadata 预检；流式写入时再次强制，超限立即中止并
  删除 staging，不产生对象、不推进任何水位。
- `Archive::put_object_stream` 以 64 KiB 块读写：哈希增量计算，内存占用与对象
  大小无关；落盘后再用 `verify_object_stream` 分块重读复核内容地址，然后才
  commit manifest。
- `file` 引用的 JSON part 文件需要在内存中解析，因此整文件也受同一上限约束
  （超限为 `over_limit`，不是静默截断）。

## 6. 权限与回读（唯一鉴权核心）

- 回读入口只有 `ReadHandle`（绑定单一 domain）。`read_attachment` 依次检查：
  条目自身 access 标签非空且包含请求域 → 该对象在**所有**已提交引用中的最严交集
  仍包含请求域 → 对象字节哈希。空请求域在查找前即拒绝。
- 跨域去重不共享回读句柄：同一内容在两个权限域各有一个引用时，两个域都无法
  读取（用例 13）；单域对象只对其域可读。
- 清单映射缺失/损坏、条目非 archived、access 标签为空均 fail-closed（
  `NotFound` / `AccessDenied`），不按残留状态放行（用例 8 手工清空 access 后
  回读被拒）。
- 源始终只读：文件 `File::open`；SQLite `SQLITE_OPEN_READ_ONLY` + 250 ms busy
  timeout；用例断言源字节不变、目录无新增 `-wal`/`-journal`。

## 7. 覆盖与健康

- 每源 `attachments` 块：`total`（由已提交清单派生，唯一权威）、`lastRun`、
  `lastRunAtMs`、`status`（`archived|partial|failed`）、`bytesStored`（去重感知）、
  `objectsNew` / `objectsDeduped`、`limitBytes`、`anomalies`。
- 总量 `totals.attachments`（13 个显式状态计数 + refs + archived + deduplicated）
  与 `totals.attachmentBytesTotal`。
- `coverage.ok` 只由**最近一轮** attachment 的拒绝计数/异常决定；历史失败保留在
  lifetime 计数中但不让健康永久变红（用例 18）。
- 缺失附件只影响 attachment 块与该轮 `ok`：会话源 status/水位不变，记录仍可
  回读（用例 5）。
- 只有清单、没有 state 的源也会出现在 coverage（`status=attachment_only`），并把
  当前健康显式报为未知（`ok=false` + “last-run health unknown”，用例 24）；反之
  state 有 attachment 记录但清单缺失 → `ok=false` + 显式异常（证据丢失）。两种情况
  都不静默少报。
- 单写者锁覆盖整个 attachment 运行；第二实例 `LockHeld`（用例 12），CLI 退出 4。

## 8. CLI（机器可读 JSON）

- `attach --root <root> --request <json|@file>`：输出完整 report；退出 `0`
  archived、`5` unauthorized、`7` 其他显式拒绝（report 仍在 stdout，错误摘要
  在 stderr，与 `capture` 一致）。
- `attachments --root <root>`：列出全部映射（attachmentId、contentRef、status、
  sha256、bytes、mediaType、access、readable 等）。
- `attachment-read --root <root> --domain <d> --capture-id <id> --seq <n>
  [--out <file>]`：与 `read` 相同契约（`--out` 写文件并输出 JSON；否则原始字节
  到 stdout、JSON 报告到 stderr），返回前校验对象哈希。
- `status` 增加 `attachments` 计数；既有命令字段只增不改。

## 9. 测试

```sh
cd agent-memory
cargo test
```

实测（2026-10-26，worktree `C:\wt\archive-p4-20261026`）：**exit 0，
346 passed / 0 failed / 1 ignored**。

| 目标 | 数量 |
|---|---|
| lib（单元；含新增 2 项 P4 流式对象用例） | 163 |
| `boundary_no_default_roots` | 5 |
| `p0_formats` | 4 |
| `p1_archive` | 8（未改） |
| `p2_jsonl_incremental` | 17（未改） |
| `p3_sqlite_consistency` | 33（未改） |
| **`p4_attachments`** | **24（新增）** |
| `recall_boundary` / `recall_cli_mcp` / `recall_contract` | 6 / 7 / 9 |
| `recall_kb_device` / `recall_opencode` / `recall_read` | 25 / 22(+1 ignored) / 18 |
| doctests（5 个 compile-fail 反向用例） | 5 |

P4 用例（全部合成临时根）：

1. 本地图片：内容寻址二进制对象、原始 ContentRef 映射、逐字节回读、跨域拒绝
   `local_image_is_archived_and_reads_back_byte_exact`
2. 超长内联工具输出（150 KB，含 CRLF/中文）：指针定位、字节入档、记录证据独立
   `over_long_tool_output_content_ref_is_archived_byte_exact`
3. 持久化工具输出文件：删源后仍可逐字节回读
   `persisted_output_survives_source_deletion`
4. SQLite 列 + 指针：入档内容精确、源库字节与文件集不变
   `sqlite_content_ref_is_archived_and_source_is_unchanged`
5. 缺失附件：显式 `missing`、coverage 异常/计数，会话记录不受影响
   `missing_attachment_is_explicit_and_does_not_affect_other_evidence`
6. 相对路径穿越（`../`、`sub/../../`）：`path_traversal`，目标未被归档
   `relative_path_traversal_is_rejected`
7. 绝对路径越界拒绝、根内绝对路径接受
   `absolute_path_out_of_bounds_is_rejected`
8. 无 access 标签 → `unauthorized`；清单 access 被清空 → 回读/对象均 fail-closed
   `access_mapping_is_required_and_fail_closed`
9. 超限（文件与内联内容）：`over_limit`、未写对象、源未截断
   `over_limit_attachment_writes_no_object`
10. 中文文件名（嵌套目录）+ CRLF 字节原样保留
    `chinese_filename_and_crlf_bytes_are_preserved`
11. 相同内容去重为一个对象、两条引用、覆盖计数正确
    `duplicate_content_is_one_object_with_two_references`
12. 第二实例被单写者锁拒绝
    `second_attachment_instance_is_refused`
13. 跨域去重不共享回读（两个域都不可读）
    `cross_domain_dedupe_does_not_share_readback`
14. 外部 URL 只记录不抓取（两种 target），源未变
    `external_url_is_never_fetched`
15. 内联 base64 图片 data URI 解码入档；data URI 作为路径被拒
    `inline_data_uri_image_is_decoded`
16. 对象被篡改：回读 `Integrity`、流式复核失败
    `tampered_attachment_object_is_detected`
17. 4 MiB 附件：分块写入/复核、逐字节回读
    `large_attachment_is_streamed_and_verified`
18. 混合轮次覆盖计数；历史失败保留，最近一轮成功后 `ok` 恢复
    `coverage_counts_mixed_runs_and_clears_current_health`
19. in-source 引用先入档、删源后仍可逐字节回读；再次尝试显式 `source_missing`，
    已采记录仍可读
    `in_source_reference_with_missing_source_is_explicit`
20. `recordSha256` 不符：`record_mismatch`，不入档；用于非 JSONL 引用是用法错误
    `record_mismatch_is_explicit`
21. 不支持的引用（坏指针、非字符串、非法 SQL 标识符、未知表、未知行、越界范围）
    `unsupported_references_are_explicit`
22. JSON part file：指针取值（content）与根内相对路径（local_file），越界 `..` 拒绝
    `json_part_file_reference_resolves_pointer_and_stays_bounded`
23. CLI JSON 契约：`attach`（0/7/5 退出码）、`attachments`、`attachment-read --out`
    `cli_attach_and_read_are_json_contract`
24. 丢失 `state/sources.json`：映射仍由清单可见、计数完整，当前健康显式标为未知
    （`ok=false` + 异常），回读不受影响
    `lost_collector_state_keeps_mappings_visible_and_reports_unknown_health`

外加 lib 单元：

- `streamed_object_matches_in_memory_hash_across_chunks`（7 字节滴流读取，跨多块
  哈希与整块一致、二次流去重）
- `stream_limit_aborts_without_committing_an_object`（超限中止、无对象、无 staging
  残留）

## 10. 已知限制

1. **P5/P6 未做**：无 provider 级覆盖对账/框架基准比较、无统一 Session/Message
   派生重放、无 CLI 完整契约冻结、无套装离线安装/预编译分发物、无真实数据试采。
   `coverage` 仍是每源投影的扩展，不是 P5 覆盖报告。
2. **不做引用自动发现**：P4 接受显式 `ContentRef`（解析器/派生层负责从记录中
   发现引用）；不解析整个会话去找附件，也没有批量扫描入口。
3. **指针只接受字符串值**：`content` 目标要求指针解析为 JSON 字符串；结构化
   JSON 值（对象/数组/数字）报 `unsupported_reference`，不做重新序列化（避免
   把派生文本冒充原始字节）。
4. **base64 仅标准字母表**：不支持 URL-safe base64、不处理非 base64 的 data URI
   （百分号编码 payload 显式拒绝）。
5. **`file:` URL 不做百分号解码、不做 UNC 映射**：`file:///C:/x` 可用；带空格的
   百分号编码路径会按字面解析（通常得到 `missing`，不猜测）。
6. **回读整体物化**：`read_attachment`/`read_object_bytes` 返回完整 `Vec<u8>`；
   写入与校验是分块的，回读不是流式 API（P5 若需要再扩展）。
7. **默认上限 32 MiB**：`maxBytes` 可按请求调整；`file` 引用的 JSON part 文件
   整文件也受同一上限（内存解析）。
8. **SQLite 主键列名假定为 `id`**：`ContentRef::Sqlite` 只携带主键值；查询使用
   `WHERE "id" = ?1`。主键列名不同的表需要扩展契约（当前会显式
   `unsupported_reference`/`read_failed`，不会静默）。
9. **同域信任边界**：运行时根内任何可写本地进程都能伪造 manifest/对象；归档不含
   签名。`read_attachment` 的哈希校验只能发现非蓄意损坏/篡改（用例 16），不是
   抗同域攻击的边界（与计划一致，本阶段不做防篡改）。
10. **附件清单与采集清单同目录**：attachment manifest 复用 `archive/manifests/`
    的序号文件；`source_captures` 用 `kind` 排除，不会污染水位历史，但人工清点
    目录时需按 `source.kind` 区分。
11. **state 只存最近一轮**：`AttachmentState` 不保留历史；lifetime 计数完全由
    清单派生。若 `sources.json` 丢失，历史计数仍从清单恢复，覆盖行按清单展示并把
    当前健康显式报为未知（`ok=false` + “last-run health unknown”），不静默当作
    健康（用例 24）。
12. **无擦除/墓碑**：引用撤除、派生失效、无引用对象回收、整代退役均未实现
    （计划 §4.3/§9 的 P5+ 轮廓）。删除任何归档对象仍属人工高权限操作。

## 11. 结论

P4 的 5 项目标均有实现与用例证据：内容哈希二进制对象与原始 ContentRef 映射；
缺失/越界/无权限/超限/外部的显式状态与覆盖异常；JSONL、SQLite、JSON part file、
sidecar 四类引用的源内定位与根边界（含穿越/绝对越界/符号链接逃逸）；分块写入与
哈希校验、源删除后回读；只读源、单写者锁、fail-closed 权限、CLI JSON；未实现
P5/P6。`cargo test` 全绿（346 passed / 1 ignored / exit 0），P1–P3 既有断言未改动
且保持通过。

真实数据：**否**。
