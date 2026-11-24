# STAGE2-S25 完成记录：证据对象压缩存储、`compact` 迁移与运行时根凭据扫描调整

- 状态：**完成**（2026-10-31，工作区 `C:\wt\stage2-s2`，基于 HEAD=db15538（S1+S2 已合并）；未 git commit，由 orchestrator 提交）
- **真实数据 = 否**：全部测试与端到端验证使用合成 fixture、临时目录与临时套件副本；未读取任何真实会话/数据库/生产 Archive/真实配置（`<user-home>/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory`、`~/.config/opencode` 全程未读）；未运行真实 61/64/90；未重建 `assets/dist`；未修改 `src/capture_sqlite.rs` 的采集语义（仅既有 `put_object` 调用点保持 Raw 语义）。

## 1. 实现

| 文件 | 变更 |
|---|---|
| `agent-memory/src/archive/mod.rs` | 对象存储编码：`ObjectEncoding`（Plain/Zstd）、`StorageHint`（Raw/Compressible）、`StorageStats`、`ObjectFileEntry`；常量 `ZSTD_COMPRESSION_LEVEL=3`、`ZSTD_OBJECT_SUFFIX=".zst"`、`ZSTD_WORTH_PERCENT=95`；`put_object_encoded`/`put_object_stream_encoded`（压缩 → 暂存 → 解压回读校验 → 原子落位）、`read_object_bytes`/`verify_object_stream` 单点解码、`has_object`/`object_encoding` 双编码判定、reconcile 双编码+存储统计、`storage_stats()`；`RestartableRead` trait；新增 6 条单元测试（`s25_object_encoding_tests`） |
| `agent-memory/src/compact.rs` | 新增：`compact(archive, dry_run)` 原地迁移（约 300 行含文档注释）；`CompactReport` |
| `agent-memory/src/audit_scan.rs` | 新增：`audit-scan` 对象解码扫描（64 KiB 分块 + 64 KiB 跨块窗口，`regex::bytes`，报告只含计数） |
| `agent-memory/src/lib.rs` | 注册 `pub mod compact`、`pub mod audit_scan` |
| `agent-memory/src/bin/agent-memory.rs` | 接线 `compact`、`audit-scan` 子命令；usage 文本与文件头退出码说明更新 |
| `agent-memory/src/capture.rs` | 整文件对象按 `SourceKind` 选 hint（`CompressedJsonlFile` → Raw，其余 → Compressible）；decoded rollout 对象 → Compressible |
| `agent-memory/src/capture_jsonl.rs` | JSONL 段对象 → Compressible |
| `agent-memory/src/attachment.rs` | 附件流式写入 → `put_object_stream_encoded(Compressible)`（不可压自动 rewind 回退明文） |
| `agent-memory/src/coverage.rs` | `CoverageReport` 新增 `storage` 块 |
| `agent-memory/tests/p7_storage_compaction.rs` | 新增：12 条 S2.5 验收用例 |
| `agent-memory/tests/support/mod.rs` | 新增 `read_object_file`（按显式编码定位对象文件并返回解码字节） |
| `agent-memory/tests/p4_attachments.rs` 等 4 个既有测试 | 对象文件直接定位的辅助改为显式编码解析（篡改/删对象/改 access 标签用例的语义不变） |
| `assets/scripts/agent-memory-common.ps1` | 凭据规则单一来源化：`Get-AgentMemoryCredentialRules`；`Get-AgentMemoryCredentialHits` 改为引用 |
| `assets/scripts/90-audit.ps1` | 「凭据不入交付物与运行时根」→「凭据不入交付物与运行时配置」（仍 FAIL）；新增「运行时根凭据扫描（压缩对象解压）」WARN 项（调 `audit-scan`，Detail 只给计数） |
| `assets/scripts/62-verify-agent-memory-offline.ps1` | 新增 6 条 S2.5 用例（37 → 43 项） |
| `agent-memory/README.md` | 新增「Object storage encoding and `compact`」一节；CLI/布局/原子性契约更新 |
| `assets/references/agent-memory-archive.md` | 新增 4.7「对象压缩存储与 `compact`」；运行时根布局更新 |
| `CHANGELOG.md` | 顶部新条目（2026-10-31） |

## 2. 设计决定

1. **存储编码的判定方式 = 文件名后缀，绝不为内容嗅探**：`<sha256>` 明文、`<sha256>.zst` 压缩。这是硬要求——整文件采集的 `.zst` 源恰好以 zstd 魔数（`28 B5 2F FD`）开头，嗅探必然误判（有专项测试：魔数开头的原样对象存取、compact 后仍不被解码）。读取优先级：同 sha 双编码并存（仅 compact 崩溃窗口）时取 `.zst`——它落位前经过解压回读校验，明文是待清理残留。
2. **「是否值得压缩」双轨制**：写入端按来源性质显式选 hint（已知已压缩的 `.jsonl.zst` 整文件与 SQLite zstd 载荷对象 → `Raw`，永不二次压缩）；`Compressible` 路径再过 95% 尺寸门槛（`compressed*100 < raw*95` 才压缩），不可压内容（附件二进制、高熵数据）自动落明文。compact 对存量原样对象只用尺寸门槛判定——已压缩内容天然过不了门槛，无需 manifest 元数据。
3. **内容地址不变**：sha256/去重/回读校验/manifest `bytes` 全部按解码后字节；manifest 提交顺序、崩溃窗口语义（孤儿对象可复用/报告）与既有一致。压缩写顺序：压缩 → 暂存文件 → 解压回读核对 sha256 → 原子落位 → 既有 manifest 提交点。
4. **compact 崩溃语义**：先落 `.zst`（已校验）再删明文；崩溃窗口内读取取 `.zst` 恒正确。下一次 compact 校验 `.zst` 后删明文（`converged` 计数）收敛到单编码；`.zst` 校验失败（外部损坏）时保留明文兜底、只计数不删任何文件。compact 与 capture 共用 `capture.lock`；`--dry-run` 也持锁（对一致视图出报告）。
5. **`--dry-run` 零写入**：压缩在内存估算（缓冲约等于压缩输出大小，候选 >2 GiB 记 `tooLarge` 跳过），不建暂存文件；测试证明运行时根逐文件哈希前后一致，且投影数与真实运行一致。
6. **存储统计的取数路径**：`stored_bytes` 来自文件元数据；zstd 组 `original_bytes` 取自 committed manifest 的 `bytes`（采集事实），仅未被任何 manifest 引用的孤儿 zstd 对象才流式解码计数——reconcile 每次采集都会跑，统计必须 O(元数据)，解码会破坏「采集成本与增量成正比」。
7. **凭据 WARN 语义**：运行时根证据对象命中只 WARN（会话正文中出现的密钥按用户决定如实保留，仅提醒），Detail 只给命中对象数与模式类别计数；交付物/运行时配置文本命中仍 FAIL。解压匹配在 `audit-scan`（Rust 端流式、内存有界）完成——PS 无 zstd；规则单一来源在 `Get-AgentMemoryCredentialRules`，90 传入 JSON，`audit-scan` 报告永不携带对象内容。
8. **audit-scan 不做访问检查的边界**：只回答「(对象，规则) 是否命中」这一布尔计数——本地进程本可直接读对象文件计算同样的信息，命令不新增任何能力；相较之下 `read` 保持 fail-closed 领域检查不变。
9. **62 的人工降级模拟**：新写入已是压缩形态，为覆盖「存量原样对象迁移」，用例把 `.zst` 对象经 CLI `read` 解回明文、按内容地址落位并删除压缩文件（完整模拟 S2.5 之前的存量形态），再 compact 并断言迁移数/节省字节/回读一致/幂等。

## 3. 测试

Rust `cargo test --locked --no-fail-fast`：**677 passed / 3 ignored / 0 failed**（基线 659/3；新增 6 条单元 + 12 条集成）。

`src/archive/mod.rs` 单元用例：

| 用例 | 覆盖 |
|---|---|
| `compressible_bytes_take_the_zstd_encoding_and_round_trip` | 新 JSONL 类内容以 `.zst` 存储且回读/流式校验逐字节一致 |
| `raw_hint_and_incompressible_content_stay_plain` | Raw hint 与高熵内容（Compressible）都落明文 |
| `content_starting_with_the_zstd_magic_is_plain_and_never_decoded` | 魔数开头原样对象被显式编码识别，compact 后仍不被误解码 |
| `dedupe_hits_either_encoding_and_never_duplicates` | 同内容跨 hint 去重，对象数恒 1 |
| `reconcile_reports_storage_stats_and_dual_encoding` | 存储统计分组数字、双编码清单、读取正确 |
| `streamed_compressed_object_round_trips_and_restarts_when_not_worth_it` | 流式压缩对象回读一致；不可压时 reader rewind 二次写明文、无暂存残留 |

`tests/p7_storage_compaction.rs` 集成用例：

| # | 用例 | 覆盖 |
|---|---|---|
| 1 | `jsonl_capture_stores_zstd_and_reads_back_byte_exact` | JSONL 采集对象 `.zst` 编码；对象/记录/CLI read 回读逐字节一致 |
| 2 | `compressed_rollout_whole_file_stays_plain` | `.jsonl.zst` 整文件对象不二次压缩（明文+原字节）；decoded 对象压缩且回读一致 |
| 3 | `compact_migrates_and_all_readers_agree` | 人工降级为存量原样 → compact 后 `read`/replay（retrieval artifact sha）/derived-read/recall 检索与回读全部逐字节一致；迁移后全部 `.zst` |
| 4 | `compact_is_idempotent` | 第二轮 compressed=0、saved=0；第三轮运行时根树哈希不变 |
| 5 | `dual_encoding_crash_window_reads_and_converges` | 双编码并存：读取正确、`dual_encoding` 报告、再 compact 收敛（converged=1、明文消失、读取正确） |
| 6 | `dry_run_writes_nothing` | `--dry-run` 运行时根前后逐文件哈希一致；投影数与真实运行一致 |
| 7 | `duplicate_content_second_capture_dedupes` | 同内容两个源采集只落 1 个对象、无双编码 |
| 8 | `sqlite_objects_are_never_double_compressed` | SQLite 基线块与 delta 对象全部明文（zstd 载荷不二次压缩）且回读长度一致 |
| 9 | `attachments_compress_by_worth_not_by_media_type` | 文本附件 `.zst`、高熵附件明文，回读逐字节一致 |
| 10 | `status_reports_storage_statistics` | status 的 `storage` 分组数字正确；`compact --dry-run` CLI 指纹一致 |
| 11 | `audit_scan_counts_hits_without_leaking_values` | 压缩对象内合成密钥：filesWithHits=1、规则计数正确、报告不含原值也不含对象正文；干净对象不误报 |
| 12 | `compact_cli_exit_codes` | 锁占用 exit 4、成功 exit 0、未初始化 exit 3、缺参 exit 2 |

PowerShell / Python / 文档：

- **62 离线验收 43/43 通过**（临时套件副本，PS 5.1 一轮 + PS 7.6.9 一轮）。新增 6 条：`.zst` 显式编码与命名规范、`compact --dry-run` 指纹一致、compact 迁移+幂等+回读一致、status 存储统计分组、压缩对象内凭据 `audit-scan` 计数命中且不打印原值、90 对象凭据 WARN 不 FAIL 且 Detail 无原值。既有 37 条（含全部反向用例与真实根指纹）不受影响。
- 修改的 `.ps1`（90/62/agent-memory-common）在 PS 5.1 与 PS 7 `Parser::ParseFile` 均 **0 错误**；三文件 BOM 保留（`EF BB BF`）。
- Python 三文件 `test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`：**71 passed**（未改 Python）。
- `assets/scripts/check-docs.ps1`：**exit 0**（8 项全过，178 个 Markdown）。
- `git diff --check` 干净（无空白错误）；新文件 UTF-8 无 BOM、LF。

62 运行方式（复现）：`cargo build --release --locked`（带分发物同款 `RUSTFLAGS="-C target-feature=+crt-static -C link-arg=/Brepro"`，否则静态 CRT 检查失败）→ 按任务约束在 `%TEMP%\s25-work\` 建专用目录复制套件脚本/许可/构建输入 → 新二进制复制进副本 `assets\dist\agent-memory\` 并同步清单（`binarySha256`/`binaryBytes`/`treeSha256`/`treeDirty=true`/`commit=null`）→ `62 -SuiteRoot <副本>`。仓库内 `assets/dist` 未动。

## 4. 命令与退出码

```
agent-memory compact --root <runtime-root> [--dry-run]
agent-memory audit-scan --root <runtime-root> --patterns <json|@file>
```

| 命令 | 退出码 | 条件 |
|---|---|---|
| `compact` | 0 | 全部成功（含 `--dry-run`） |
| `compact` | 2 | 用法（缺 `--root`） |
| `compact` | 3 | 运行时根未初始化，或对象校验失败/I/O 错误（`ok=false`；未核实的数据绝不删除，报告 `errorSamples`） |
| `compact` | 4 | 锁被占用（`capture.lock`） |
| `audit-scan` | 0 | 扫描完成（命中与否不影响退出码；`ok=false` 时 3） |
| `audit-scan` | 2 | patterns 缺失/非法 JSON/规则非法正则 |
| `audit-scan` | 3 | 有对象无法读取/解压（报告 `errors`） |

`compact` 报告字段：`ok/dryRun/root/finishedAtMs/objectsScanned/candidates/compressed/notWorth/alreadyCompressed/converged/tooLarge/corrupted/errors/storedBytesBefore/storedBytesAfter/savedBytes/errorSamples`（camelCase）。

## 5. 限制

- 存储统计的 zstd `originalBytes` 依赖 manifest（引用对象的采集事实）；未被引用的孤儿 zstd 对象按流式解码计数（瞬态，量级为崩溃窗口残留）。双编码瞬态内两组各计一次（磁盘实况如此）。
- `--dry-run` 对 >2 GiB 候选记 `tooLarge` 跳过（内存估算的界限）；真实 compact 无此限制（流式暂存）。
- compact 不重排分片目录、不回收空目录；对象数不变，只换编码。
- `audit-scan` 的跨块窗口为 64 KiB：长于窗口的单条匹配会漏报（现有凭据规则最长数十字节，不受影响）；规则由调用方传入，新增超长模式时需同步调大 `MATCH_WINDOW_BYTES`。
- 90 的对象凭据扫描依赖已安装二进制含 `audit-scan` 子命令：旧版本二进制会以「对象扫描未完成」WARN 提示升级；扫描故障一律 WARN，不改变审计退出码。
- 兼容性：S2.5 之前的旧二进制无法读取 `.zst` 编码对象（旧 `read_object_bytes` 直接按内容地址校验磁盘字节）。升级须先换二进制再跑 compact；证据库「只追加」不受影响（compact 原子替换、sha 不变），但**不提供降级路径**——回退旧版本前需把 `.zst` 对象解回明文（62 用例演示了逆操作）。

## 6. 未做（按边界与分工）

- 未重建 `assets/dist`、未刷新分发物谱系：源码树与二进制已变，真实 90 的「构建谱系与源码一致」与「已安装且与分发物一致」在 orchestrator 重发分发物（`build-release.ps1`）前会 FAIL——这是预期的分发物闭环步骤，不属于本任务。
- 未运行真实 61/64/90；未注册真实计划任务；未做生产回填（S4）与 S3 存量 869 MB 对象的实际迁移（`compact` 的目标场景，由 orchestrator 在授权环境执行，先 `--dry-run` 后正式跑）。
- 未实现按 manifest 记录的「来源已压缩」标签持久化（当前写入端 hint 是调用点常量，compact 用尺寸门槛兜底；如未来需要逐对象元数据再立 manifest 字段）。
- 未 git commit（orchestrator 执行）；未更新 `CURRENT.md`（沿用 S1/S2 先例，由 orchestrator 合并时统一记账）；`docs/verification/README.md` 索引未加条目（S1/S2 亦未加，索引整理统一交 orchestrator）。

## 7. [UNCERTAIN]

- 无未决设计问题。唯一环境注记：本机 `cargo build --release` 默认不带分发物的 `RUSTFLAGS`，62 首轮因此报静态 CRT FAIL；已按 `build-release.ps1` 的旗标重建后全过——orchestrator 重建分发物走 `build-release.ps1`，不会遇到此问题。
