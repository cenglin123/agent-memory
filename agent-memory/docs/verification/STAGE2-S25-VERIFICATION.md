# STAGE2-S25 独立验收报告：证据对象压缩存储、`compact` 迁移与运行时根凭据扫描

- 验收对象：隔离检出 `C:\wt\s25-verify`，detached HEAD = `603358f`（父提交 `db15538`），工作区干净（`git status --short` 为空）。
- 验收角色：独立 Verifier（全新上下文，与实现者 GLM 不同模型族），只读被检仓库。
- 环境：Windows；`cargo 1.99.0` / `rustc 1.99.0`（与 `rust-toolchain.toml` 一致）；Python 3.13.17；PowerShell 5.1；`zstd` CLI 仅用于构造/核对合成 fixture。
- 隔离：全部实验在专用临时目录 `%TEMP%\s25-verify-377df159` 内完成（结束后已删除）；旧版二进制由 `git -C C:\wt\s25-verify archive db15538 agent-memory` 导出后独立构建，未改动被检仓库。未读取任何真实会话/数据库/生产 Archive/真实配置（`~/.local/share/opencode`、`~/.claude`、`~/.codex`、`~/.session-evidence`、`~/.agent-memory`、`~/.config/opencode` 全程未读）；未执行真实 61/64/90；未运行生产回填。
- 说明：本报告写入 `C:\wt\stage2-s2`（另一个 worktree，HEAD = `410b2e3`）。`410b2e3` 是 `603358f` 之后的一条测试清理提交（`p7_storage_compaction.rs` 删掉一行重复绑定），不在验收对象内；本报告结论只针对 `603358f`。

## 结论（verdict）

**阻断需修复**

功能实现（压缩存储、`compact` 迁移、崩溃收敛、`audit-scan`、90 审计语义）经端到端实测全部符合规格；**唯一阻断项是规格第 8 条要求的文档提示缺失**：`README` / `assets/references/agent-memory-archive.md` / `UPGRADE.md` 都没有「升级须先换二进制再 compact、无降级路径」的说明。该项不改代码，只补文档（约 2–3 句 + 建议的能力版本防护）。

---

## Blocking

### B1. 缺少规格要求的「升级须先换二进制再 `compact`、无降级路径」提示

- 位置：`agent-memory/README.md:534`（`## Object storage encoding and compact` 整节，最接近的一句是 `:577` “Legacy stores … run it once after upgrading”）；`assets/references/agent-memory-archive.md:317`（4.7 节）；`UPGRADE.md` 完全未提及 `compact`/`.zst`。
- 核实命令与证据：
  - `grep -rn "降级\|旧二进制\|旧版本\|downgrade\|先换" agent-memory/README.md assets/references/agent-memory-archive.md UPGRADE.md` → 无一条与对象编码兼容性相关。
  - 用 `db15538` 构建的旧版二进制读取新二进制 `compact` 后的运行时根（同一合成 fixture）：
    - `agent-memory read --root <root> --sha256 <zst 对象 sha> --domain default` → **exit 6**，`{"ok":false,"error":"not found: object <sha>"}`
    - `agent-memory status --root <root>` → **exit 0**，但 `reconcile.missing_objects` 6 条、`reconcile.orphan_objects` 6 条（名字带 `.zst` 后缀）。
  - 对照：新版二进制同一 root `status` 的 `missing_objects`/`orphan_objects` 均为 0。
- 为何列为阻断：这是规格第 8 条明确点名的交付内容；且是可逆性风险——生产 869 MB 证据库一旦 compact，回退旧二进制后 `read`/覆盖/审计会整体失真（missing objects），而当前用户文档没有任何提示。S4 生产回填前必须先看到这条约束。
- 建议（最小修复）：
  1. `agent-memory/README.md` 的 storage 节、`assets/references/agent-memory-archive.md` 4.7、`UPGRADE.md` 各加一段：**先升级二进制，再运行 `compact`**；旧二进制无法解码 `<sha>.zst`（会报 `missing_objects` / `orphan_objects` 带 `.zst`）；**不提供降级路径**，回退旧版本前需把 `.zst` 解回明文（`62-verify-agent-memory-offline.ps1` 的 S2.5 用例演示了逆操作）。
  2. 可选增强（非本次必需）：在 `state/schema.json` 记录“对象编码能力版本”，让旧二进制遇到 `.zst` 时给出「需升级」而不是把它误报成 missing/orphan。

---

## Non-blocking

### N1. 双编码且 `.zst` 损坏时，读取不回落明文（与完成记录“保留明文兜底”的措辞不一致）

- 位置：`agent-memory/src/archive/mod.rs:993`（`object_encoding` 恒优先 `.zst`）、`:1463`（`read_object_bytes` 解码失败即 `Integrity`）；`agent-memory/src/compact.rs:120`（校验失败只计数、不删任何文件）。
- 证据：在 compacted fixture 中人为制造 plain+`.zst` 并存，并翻转 `.zst` 尾部 5 字节：
  - `read --sha256 <sha>` → **exit 6** `object <sha> is not valid zstd: Data corruption detected`；同名明文文件仍在且内容正确（`sha256` 匹配）。
  - `compact` → **exit 3**，`corrupted=1`、`converged=0`，两个文件都保留。
- 为何不是阻断：数据未丢失，fail-closed 与仓库既有完整性语义一致；且 `compact` 只有在解压回读校验通过后才 rename，正常流程不会产生损坏的 `.zst`。问题只在于 `STAGE2-S25-COMPLETED.md §2.4` 写“保留明文兜底”，读者会以为可自动回读。
- 建议：读取 `.zst` 解码失败时回退明文并报 WARN；或把文档改为“保留明文文件供人工恢复，读取仍 fail-closed”。

### N2. 一个不可解码的“孤儿 `.zst` 对象”会让 `status` / `reconcile` / `coverage` / `capture` 全部失败

- 位置：`agent-memory/src/archive/mod.rs:1058`（`storage_stats`）→ `:1997`（`decoded_len_of`）；`reconcile` 在 `:1685` 调用它；`capture` 每次收尾都会跑 `reconcile`。
- 证据：在无 manifest 引用的位置放一个合法名字的 `.zst` 并破坏首字节后：
  - `status` / `reconcile` / `coverage` → **exit 3**，`io error: <root>\archive\objects\<aa>\<bb>\<sha>.zst: …`
  - `capture`（正常采集另一来源）→ **exit 3**，同一错误。
  - 旧行为（仅枚举目录）只会把它列入 `orphan_objects`，不会阻塞采集。
- 为何不是阻断：正常流程不会产出不可解码的 `.zst`（capture/compact 均在 rename 前做解压+sha 校验），且错误显式指向文件、无数据删除；属“外部损坏/手工放置”下的可用性退化。
- 建议：`storage_stats` 对未被 manifest 引用的对象解码失败时降级计数（新增 `unreadable` 或有界告警），让 `reconcile` 继续工作并把它作为 orphan 报告；不要让统计失败升级为整个采集循环失败。

### N3. 文档措辞与崩溃窗口语义矛盾

- 位置：`assets/references/agent-memory-archive.md:23` —— “同一对象的 zstd 编码（level 3；**同一 sha 两种编码不并存**）”；同文件 4.7 又说 compact 崩溃窗口会短暂并存，`reconcile.dual_encoding` 会列出。
- 建议：改为“常态下不并存；`compact` 崩溃窗口为瞬态，由下一次 `compact` 收敛”。

### N4. `audit-scan` 只匹配、不校验内容地址

- 位置：`agent-memory/src/audit_scan.rs:94`（`scan_object` 直接按显式编码流式解码，不比对 sha）。
- 证据：把一个结构合法但内容与文件名不符的 `.zst`（尾部改动导致帧内追加，解码 24008 B ≠ 原 17000 B）交给 `audit-scan`：**exit 0**、`filesWithHits=1`，不报错；同一对象 `read` 则因 sha 不匹配 fail-closed。
- 影响：仅影响“不可读对象告警”的覆盖面（完整性由 `scrub` 负责），90 的 WARN 计数语义不受影响。
- 建议：若要 audit 也承担完整性提示，可对扫描对象做流式 sha 校验并写入 `errors`；否则在 README 明说“audit-scan 只匹配内容、不验完整性”。

### N5. 预存在缺陷（非本次引入，勿误归因）：OpenCode SQLite 记录的 `read --sha256 <record>` 在 `db15538` 与 `603358f` 上同样失败

- 位置：`agent-memory/src/archive/mod.rs:1856-1885`（`read_record` 直接对对象原始字节切片）；`agent-memory/src/capture_sqlite.rs:2614`（`decompress_object` 说明 sqlite 对象字节是 zstd 载荷，记录 offset 指向解压后的 batch）。
- 证据：合成 SQLite fixture 的 88 条记录中，8 条 sqlite 记录（基线块与 delta 各若干）：
  - `read --sha256 <record>` → exit 3 `corrupt archive state: record … span exceeds object …`，或 exit 6 `integrity check failed …`
  - 旧/新二进制输出逐字相同（同一 root、同一命令）。
- 影响：与 S2.5 无关（SQLite 载荷对象本次仍保持明文，行为不变），但会被误当作本次回归；建议另立任务（读取 sqlite 记录前先解压载荷）。

### N6. `reconcile` 每次多解析一遍全部 manifest

- 位置：`archive/mod.rs:1685`（reconcile 已 `self.manifests()`）与 `:1058`（`storage_stats` 内再次 `self.manifests()`）。
- 影响：capture 每次收尾都会跑 reconcile，manifest 多时是常数级重复 I/O（非随增量增长），可接受。建议把已解析的 manifests 传入 `storage_stats`。

### N7. `compact` 被硬杀后遗留 `locks/capture.lock`，下一次运行 exit 4，需人工删锁

- 证据：kill 后 `locks/capture.lock` 内容 `pid=… acquired_at_unix_ms=…`；`compact` → **exit 4** `capture lock already held … (a hard-killed run leaves this file behind; remove it only after confirming no capture is running)`；删除该锁后再运行 `compressed=5` 正常收敛。
- 评价：与既有 `capture` 锁语义一致，错误信息明确，4.7 已指向 4.6 的锁处理；非阻断。建议在 869 MB 生产迁移的操作手册里显式写“中断后先确认无进程再删锁”。

### N8. 索引类记录

- `agent-memory/docs/verification/README.md` 未加本阶段条目（完成记录声明交由 orchestrator 统一整理）；`CURRENT.md` 未更新（沿用 S1/S2 先例）。仅记录状态。

---

## 必查项逐条结论

| # | 要求 | 结论 | 关键证据 |
|---|---|---|---|
| 1 | `cargo test` 677/3/0；Python 71 | **通过** | Rust：30 个 test binary 汇总 `passed=677 failed=0 ignored=3`，exit 0；Python：`71 passed`，exit 0 |
| 2 | 迁移正确性（旧二进制采集 → 新二进制 compact → 全部读取方逐字节一致；幂等） | **通过** | 13 个对象 read 全部逐字节一致；88 条记录读取结果一致；9 个 manifest 与 `state/records.json` 哈希不变；replay 5 个来源 artifact sha 全等（resume 全 `reused`；删 derived 后同 identity 全量重放全 `replayed`、artifact sha 全等、messages 80）；derived-read 5 个 ref 字节一致；coverage/status/reconcile/scrub/recall 检索与回读归一后无差异；第二次 compact `compressed=0 savedBytes=0`、运行时根树哈希不变 |
| 3 | 崩溃语义 | **通过** | 0.4s 硬杀：15/20 已迁移、1 个 staging 残留、0 双编码；删锁后再次 compact `compressed=5`、20/20 可读；人工制造双编码：read 正确、`reconcile.dual_encoding` 命中、再次 compact `converged=1` 且明文删除、读取仍正确；残留 staging 被持续报告且不影响读取/compact |
| 4 | 嗅探防护 | **通过** | 真实 `.jsonl.zst` 整文件对象：编码 plain、首 4 字节 `28 b5 2f fd`、与源文件逐字节一致、compact 后仍是 plain（`compressed` 未动它）；`read`/`derived`/recall 均按后缀解码 |
| 5 | 绕过检查 | **通过** | 全仓 `grep`：`archive/objects` 路径拼接仅 `archive/mod.rs:970/1015/1686`；对象读取全部收敛到 `read_object_bytes` / `verify_object_stream`（见下节清单）；无 `fs::read` 直读对象文件的旁路 |
| 6 | 90 凭据扫描 | **通过** | 压缩对象内合成密钥：`audit-scan` `filesWithHits=1`、`hitsByRule={"known-key-prefix":1}`，stdout/stderr 无值无片段；`90 -AgentMemoryOnly` 该检查为 **WARN**（`1/13 … known-key-prefixx1`），Detail 仅计数；交付物注入同值 → 该检查 **FAIL**（`凭据模式命中 1 处：assets\references\conventions.md:212 [assigned-secret]`），全程无值泄露；≥200 MB 解码对象 audit-scan 峰值工作集 **9.7 MiB** |
| 7 | 存储统计与磁盘一致；去重 | **通过** | plain 7/67197 B、zstd 6/1671 B 与磁盘逐文件求和一致；`zstdOriginalBytes=23569` 与逐对象解压长度之和一致；同内容第二个来源采集 `objectsNew=0 objectsDeduped=1`、对象文件数不变、无新增双编码 |
| 8 | 文档一致 / 仓库卫生 | **部分不通过（B1）** | `git diff --check` exit 0；新增行无真实用户目录路径（命中 0）；新文件 LF 无 BOM、三个 `.ps1` 保留 BOM；三个 `.ps1` 在 PS 5.1 `Parser::ParseFile` 均 0 错误；**缺升级/无降级提示（B1）**；N3 措辞矛盾 |

### 对象读取点清单（对应必查 #5）

| 读取方 | 调用点 |
|---|---|
| `read`（object / record） | `bin/agent-memory.rs:341` → `archive/mod.rs:1850/1856` → `read_object_bytes` |
| `attachment-read` / attachment 校验 | `archive/mod.rs:1937`（`read_attachment` → `read_object_bytes`）；`attachment.rs:304/327`（`verify_object_stream`） |
| replay / derived-read | `replay.rs:476`（`ReadHandle::read_object`） |
| recall archive provider | `recall/providers/archive.rs:828`（`read_object_bytes`） |
| capture JSONL 回读校验 | `capture_jsonl.rs:461` |
| capture_sqlite before 查找 / verify | `capture_sqlite.rs:1674`（`has_object`）、`3754/4374/5778/6049`（`read_object_bytes`）；载荷解码 `capture_sqlite.rs:2614` |
| scrub | `scrub.rs:270`（`has_object`）、`282`（`verify_object_stream`） |
| reconcile / status / coverage 统计 | `archive/mod.rs:1685` → `storage_stats`（`object_files` + 元数据；仅未被引用的 zstd 走 `decoded_len_of`） |
| audit-scan（新增，流式例外） | `audit_scan.rs:66/101`：按 `object_files()` 得到的显式编码流式解码（内存有界），不经过 `read_object_bytes`（有意为之，见 N4） |
| compact（写者） | `compact.rs:104/120/305`：`object_files` / `verify_object_stream` / `place_object_file` / `verify_staged_zstd` |

`grep -rn '"objects"' src/` 仅 `archive/mod.rs:970/1015/1686`；`compact.rs`、`audit_scan.rs` 均通过 `object_path_for(sha, encoding)` 取路径，无文件名猜测。

---

## 已执行命令与退出码

> 除最后写入本报告外，未写入被检仓库（`git status --short` 前后均为空）。

| 命令 | 结果 |
|---|---|
| `cargo test --locked --no-fail-fast`（`agent-memory`，`CARGO_TARGET_DIR` 指向临时目录） | exit 0；677 passed / 3 ignored / 0 failed |
| `python -I -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`（`assets/scripts`） | exit 0；71 passed |
| `git -C C:\wt\s25-verify archive db15538 agent-memory \| tar -x` + `cargo build --locked --offline`（临时目录） | exit 0；生成 db15538 旧二进制 |
| 旧二进制 `init` + `capture`（Claude 两段增量、Codex `.jsonl`、Codex `.jsonl.zst`、OpenCode SQLite 基线+delta、2 个附件、凭据样本） | 全部 exit 0；13 个 plain 对象、0 个 zstd、9 个 manifest |
| 旧二进制读取面基线：每个对象 `read`、88 条记录 `read`、`replay --parser-version s25v-p1`、5 个 `derived-read`、`coverage`/`status`/`reconcile`/`scrub`、`memory_search`+`memory_read`（archive provider） | 除 8 条预存在 sqlite 记录失败（N5）外全部 exit 0 |
| 新二进制 `compact --dry-run`（前后逐文件树哈希） | exit 0；`compressed=6 notWorth=7 savedBytes=21898`；树哈希完全一致 |
| 新二进制 `compact` | exit 0；13 扫描/13 候选/6 压缩/7 不划算；`storedBytesBefore=90766 → After=68868`，与磁盘求和一致 |
| 新二进制重复全部读取面（含 resume replay 与删除 derived 后同 identity 全量重放） | 全部 exit 0；与基线逐字节一致（详见必查 #2） |
| 新二进制第二次 `compact` | exit 0；`compressed=0 converged=0 savedBytes=0`；运行时根树哈希不变 |
| 同内容第二来源 `capture` | exit 0；`objectsNew=0 objectsDeduped=1`，对象文件数 13→13 |
| 硬杀 `compact`（0.4 s）→ 旧锁 `compact` → 删锁 → `compact` 收敛 | 硬杀后 15 zstd/5 plain/1 staging；带锁 exit 4；删锁后 exit 0（`compressed=5`）；20/20 `read` 校验通过 |
| 人工双编码 → `read` / `reconcile` / `compact` | read exit 0 且内容一致；`dual_encoding` 命中；compact exit 0、`converged=1`，明文消失、读取仍一致 |
| 损坏明文对象 `compact` | exit 3；`corrupted=1`；文件未被改动、未生成 `.zst` |
| 损坏 `.zst`（双编码）`compact` / `read` | compact exit 3（`corrupted=1`、两个文件都保留）；read exit 6（N1） |
| 不可解码孤儿 `.zst` 下的 `status`/`reconcile`/`coverage`/`capture` | 均 exit 3（N2） |
| `audit-scan`（迁移后 root，规则来自 `Get-AgentMemoryCredentialRules` 同源 JSON） | exit 0；13 对象、`decodedBytes=90766`、`filesWithHits=1`、`known-key-prefix=1`、无值/片段泄露 |
| `audit-scan` 退出码负向：缺 `--patterns` / 非法 JSON / 非法正则 / 不可解码对象 | 2 / 2 / 2 / 3 |
| `compact` 退出码负向：缺 `--root` / 未初始化 / 锁被占用 | 2 / 3 / 4 |
| `90-audit.ps1 -AgentMemoryOnly -UserRoot <临时> -AllowArchivedData`（干净套件副本，含压缩凭据对象） | exit 1（仅因缺少分发物谱系/清单等预期 FAIL）；「运行时根凭据扫描」= **WARN** `1/13 … known-key-prefixx1`；「凭据不入交付物与运行时配置」= PASS（85 文件）；输出无假值 |
| 同 90，套件副本 `conventions.md` 注入同源假值 | exit 1；「凭据不入交付物与运行时配置」= **FAIL** `凭据模式命中 1 处：assets\references\conventions.md:212 [assigned-secret]`；输出无假值 |
| ≥200 MB 解码对象 `audit-scan` 峰值内存采样（psapi `PeakWorkingSetSize`） | exit 0；`decodedBytes=209715367`、命中 1；峰值工作集 9.7 MiB |
| `capture-all`（新 store，3 个 codex 文件含 `.jsonl.zst`）+ 全部对象 `read` | exit 0；1 plain + 2 zstd；读取失败 0 |
| `attachment-read`（压缩附件） | exit 0；`verified=true`、4200 B、与源文件逐字节一致 |
| 旧二进制读取 compacted root（降级模拟，B1 证据） | `read` exit 6 `not found`；`status` `missing_objects=6`、`orphan_objects=6`（`.zst`） |
| `git diff --check db15538 603358f` | exit 0（无空白错误） |
| `git diff db15538 603358f \| grep -cE '^\+.*C:\\Users\\'` | 0（无真实用户目录路径） |
| PS 5.1 `[Parser]::ParseFile`（90 / agent-memory-common / 62） | 3 个文件均 0 错误 |
| `git ls-files --eol`（新文件与改动文件） | index 均 `i/lf`；新文件 UTF-8 无 BOM |

---

## 未覆盖 / 边界

- **未复跑 62 离线验收**：需以分发物同款 `RUSTFLAGS`（静态 CRT）构建 release 并同步临时分发物清单，不在本次必查清单内；完成记录声明的 43/43 未独立复核。
- **未执行生产回填 / 真实 869 MB 迁移**（按边界禁止）；`compact` 的 >2 GiB `--dry-run tooLarge` 分支仅代码审查（`compact.rs:223-226`）。
- **未运行真实 61/64/90**；90 的验证在临时 UserRoot + 临时套件副本中完成，未触碰真实安装。
- 90 的输出在 PS 5.1 重定向时中文按 GBK 编码写出，验收时以 `Result` 列（ASCII）与关键英文/数字片段判定，不影响结论。
