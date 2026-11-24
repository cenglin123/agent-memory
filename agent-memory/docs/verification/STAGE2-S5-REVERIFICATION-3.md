# STAGE2-S5 返工 3 独立复验报告（第三轮，跨模型族 verifier）

- 被检对象：`C:\wt\s5-reverify3`，HEAD = `9ba4d1a`（`perf(agent-memory): S5 返工 3……`），返工 3 diff = `git diff 1ed02a9 9ba4d1a`。
- 上一轮复验：`STAGE2-S5-REVERIFICATION.md`（verdict 通过；非阻断 P1/N1/N2/N3/N4/NB-5）。
- 实现者记录：`STAGE2-S5-COMPLETED.md`「返工 3」（以代码为准，本报告逐项对照）。
- **verdict：通过**（无阻断项；6 项硬指标全部达标，6 条非阻断项见 §4）。
- 方法：只读被检仓库（仅 `cargo test/check/build` 与 `git` 只读命令）；不 commit、不改源码；未注册/运行真实 61/62/64/90；未打开任何真实会话、数据库、生产 Archive 或真实配置（`<user-home>\.local\share\opencode`、`.claude`、`.codex`、`.session-evidence`、`.agent-memory`、`.config\opencode` 全程未读）；全部 fixture 自建于 `C:\wt\s5-verify-stage3\`（合成 needle 前缀 `sv3-`、`needleSV3`）；Python 一律 `python -I`；长任务前台等待。

---

## 1. 规模实测（本轮核心）

### 1.1 合成世界形态

| 维度 | 本报告 | orchestrator 真实数据（仅作对照，本轮未读取） |
|---|---|---|
| carrier（唯一 source path 分组） | **4 107** | 4 301 |
| 小 legacy JSON storage 文件（`storage/{session,message,part}/*.json`） | **3 705** | ≈3 636 |
| JSONL carrier（Claude / Codex） | 401（351 / 50） | 其余约 665 |
| OpenCode SQLite source | 1（V2 + V1 表，V1-only 3 会话，V1/V2 同 id 1 会话） | 1 |
| manifest 数 | 4 109（1 基线 + 2 delta + 4 106 文件 carrier） | ≈4 300 |
| `session_message` 行（活源，终态） | 230 001 → **228 007** | 22.9 万单元行 |
| 索引单元行 | OpenCode **236 016**（228 007 活源 + 8 009 补缺）、全库 **243 326** | 229 000 |
| Archive 对象库体积 | 34 MB | — |

构建方式：`opencode.db`（230 会话 × 1 000 行）+ 3 700 个约 0.3 KB 的 legacy JSON 文件 + 350 个 Claude 转录（20 条/个）+ 1 个双 sessionId carrier + 50 个 Codex rollout；经生产同款 `capture-all` 落成 4 107 个 carrier 的 Archive，再经 `index` 建索引。构建脚本与全部计时记录在 `C:\wt\s5-verify-stage3\`（`build_world.py` / `measure.py` / `live.py` / `invariants.py`）。

### 1.2 实测表（release 构建，与本机 d47f526 基线二进制同机对照）

| 场景 | 实测 | 目标 | 判定 |
|---|---|---|---|
| **首次 index（删整个 `cache_root`，4 107 carrier / 236 016 行）** | **17.58 s**（第二次 0.66 s） | ≤3 min | 达标（≈10× 余量） |
| **清单冷推导**（只删 `cache/archive-inventory/`，索引热） | **3.13 s** | ≤3 min | 达标 |
| **无变化 index（含 archive 源、补缺 11 会话登记在案）** ×3 | **0.594 / 0.572 / 0.602 s，中位 0.594 s** | ≤5 s（检查 ≤1 s） | 达标 |
| 同上，`d47f526` 基线二进制（同世界、同配置、预热后 ×4） | **0.315 / 0.314 / 0.304 / 0.317 s，中位 0.315 s** | 对照 | 增量开销 ≈**+0.28 s（1.9×）** |
| 无变化 index @ 4 102 carrier / 230 010 行（换手前） | 0.531 s 中位 | ≤1 s | 达标 |
| **增量清单**（新增 5 个 legacy carrier → capture → index） | **0.647 s** 中位（指纹变化、其余 carrier 走缓存） | — | 达标 |
| 稳态增量（+3 003 行增长，无删除） | 0.692 s 中位 | ≤5 s | 达标 |
| 行镜像：**3 个待补 V2 会话**（各 1 000 行，源 23 万行） | **794 624 B**、`sessions_held=6`（3 V2 + 3 V1）、V2 行恰 3 000 | 只物化待补 | 达标 |
| 行镜像：8 个待补 V2 会话 | 2 080 768 B、8 000 行 + 3 V1、`last_capture_seq=3` | 只物化待补 | 达标 |
| **行镜像构建成本**（+3 个待补会话，增量路径） | 1.717 s − 0.692 s ≈ **1.0 s** | — | 达标 |
| 回读：镜像命中（带上下文 ±1） | **0.232 s** 中位 | — | 达标 |
| 回读：无当前镜像 → 只物化该会话（23 万行源） | **0.857 s** 中位 | — | 达标 |
| 回读：镜像过期（新 delta 已提交）→ 临时物化 | 1.22 s 中位 | — | 达标 |
| NB-5：预置 `<digest>.building.{999,424242}` 后触发镜像工作 | 两个残留**均被清除** | 回收 | 达标 |

**与真实数据对照（非同机同世界，绝对值不可跨表比较，仅看形态）**

| 指标 | 返工前（真实数据） | 本轮（合成，返工 3 后） | 比值 |
|---|---|---|---|
| 首次 index | 2 302 s（≈34 min） | 17.58 s | ≈131× |
| 无变化 index | 25 s | 0.594 s | ≈42× |
| 行镜像 | 3.39 GB / 120 待补会话 ≈ **28.9 MB/会话**（为待补会话物化整个 V1+V2） | 794 624 B / 3 会话 ≈ **0.26 MB/会话** | ≈110×/会话 |

说明：本报告全部为 release 构建（分发物形态）；实现者 R3.3 表为 debug 构建，数值不可直接比。OS 文件缓存为热（capture 后连续测量）；首次 index 距 3 min 目标有约 10× 余量，磁盘冷缓存不足以改变结论。

---

## 2. 必查项逐条结论

### 2.1 测试（必查 1）

| 项 | 结果 |
|---|---|
| `cargo test --locked --no-fail-fast`（`agent-memory/`） | **退出 0：710 passed / 3 ignored / 0 failed**（与声明一致；逐测试二进制汇总） |
| `cargo check --locked` | 退出 0，0 warning |
| `git diff --numstat d47f526 9ba4d1a -- agent-memory/tests` | `1498 0 agent-memory/tests/recall_archive_fallback.rs`——**纯新增、0 删除**；基线 691/3 的既有断言零修改 |
| `python -I -m unittest discover -s . -p "test_recall_*.py"`（`assets/scripts/`） | 退出 0：**73 tests OK** |

返工 3 对 S5 自有用例的两处改动**加强**而非削弱断言（`tests/recall_archive_fallback.rs`）：新增 `read_with_context`（归档 ref 带上下文回读必须成功且文本包含命中内容）；N4 从 `sessions >= 2` 收紧为 `assert_eq!(sessions, 3)`。

### 2.2 规模实测（必查 2）

见 §1。**关键结论：4 107 carrier（3 705 个小 legacy JSON）下清单冷推导 3.13 s、首次 index 17.58 s；无变化 index 0.594 s（同机 d47f526 基线 0.315 s）；行镜像只物化待补会话（3 会话 0.79 MB，同源整库物化路径已被删除）。实现者自有规模用例只有 1–42 manifest，本轮补齐了数千 carrier 形态，结论不变。**

补充证据（清单不含正文解析）：无变化 index 相对 d47f526 基线只多 **+0.28 s**；若每次解析 4 108 个 manifest 正文（34 MB JSON）耗时在秒级，故可判定无变化路径确为「只列文件名」。清单指纹亦经独立复算：对 `manifests/*.json` 排序名集合做 SHA-256，与 `current.json.fingerprint` 逐字相等。

### 2.3 正确性回归（必查 3）

| 场景 | 命令/证据 | 结果 |
|---|---|---|
| 删除会话找回（无 delete delta） | 删活源 3 会话 3 000 行 → index：`origin=archive`，`archive_only_sessions=6`，`memory_read` 返回删除前原文 | 通过 |
| 删除会话找回（delete delta 已提交） | 删 `sv3-ses-0010` 并 capture → index：`origin=archive`、`deleted_at=2026-10-31T14:13:15Z`，且 `archive_fallback.deleted_at_ms=1793455995090` **等于**该 delete delta manifest 的 `captured_at_ms`；带上下文回读 3 段 | 通过 |
| 删除后仍可读（delete-keeping） | 0000 会话的 delete delta 应用后再次回读，仍返回删除前文本 | 通过 |
| V1-only 覆盖 | `needleSV3v1solo1` → `origin=archive`、`schema_gen=sqlite-v1`、locator `session_id+message_id+part_id`；带上下文回读 2 段、range 校验通过 | 通过 |
| V1/V2 同 id | `needleSV3bothv2` → 1 条 `origin=live`/`sqlite-v2`；V1 副本 `needleSV3bothv1` → **0 命中** | 通过 |
| 回活切换（旧时间戳） | 以 `time_created=1.6e12`（低于水位 1.7e12）重建 1 000 行 → index：`origin=live`、无 `deleted_at`、`archive_only_sessions` 7→6、`coverage.sessions=228`（=当时活源会话数）；旧归档 ref 仍可按证据回读 | 通过 |
| 多会话 carrier | 单文件含 2 个 `sessionId`，删除该文件后两个会话各 5 条命中、`origin=archive`，`archive_only_sessions=2`，两者回读均成功（含上下文） | 通过 |
| 无重复 | 连续 3 轮 index：`total_rows` 恒 237 320（当时），同一 needle 每次 1 命中、`hit_id` 唯一稳定 | 通过 |
| MCP/search/read 不写 `<cache_root>` | `memory_search` + `memory_read`（镜像命中、镜像缺失物化、JSONL 归档）前后对 `<cache_root>` 全部 4 112 个文件做 SHA-256 逐文件比对：**changed/added/removed 全 0**；MCP 传输路径由既有用例 `mcp_calls_never_write_the_cache_root` 覆盖并通过 | 通过 |
| N5（SQLite delta 记录按 `--sha256` 回读） | `agent-memory read --root … --sha256 <delta 记录> --out …` 退出 0；输出 390 B，SHA-256 与记录地址一致，内容为 `{"table":"session_message","op":"delete",…}` | 通过 |
| **新修复：归档 ref 带上下文回读** | Claude 拼接路径（±1 → 2 段）、OpenCode 镜像路径（±1 → 3 段，命中段 `range_sha256` 等于 ref）、OpenCode 临时物化路径（±1 → 3 段）、活源 ref 对照（3 段）全部成功；修复点 `src/recall/read.rs:681` + `src/recall/refs.rs:105` 的 `same_coordinates` | 通过 |
| fail-closed 未退化 | 篡改归档 ref 的 `record_sha256` → `REF_LOCATOR_MISSING`，退出 5 | 通过 |

### 2.4 N1 / N4 / NB-5（必查 4）

| 项 | 证据 | 结论 |
|---|---|---|
| **N1** | 系统临时目录在本轮全部 ≥18 次回读（镜像命中 ×4、镜像缺失物化 ×3、镜像过期 ×3、V1 ×2、Claude 拼接 ×3、活源 ×1、其余 ×2）前后比对：**新增 `s5-readback-*` 目录 0 个**。目录内已有 22 个**空** `s5-readback-*`，ctime 均为 2026-10-31 18:06–19:47（本地），**早于**本轮修复提交（`9ba4d1a` 提交时间 21:40），本轮 `cargo test`（21:46 起）与全部回读均未新增；判定修复生效 | 通过 |
| **N4** | 删 7 个活源会话后：`coverage.sessions=224`，独立查询 `session_v2` 活源行 **224**；补缺会话单独计 `archive_only_sessions=11`（8 V2 + 3 V1） | 通过 |
| **NB-5** | 在 `rows/` 预置同 digest 的 `<digest>.building.999` 与 `.building.424242`（各 14 B），触发镜像工作（新增待补会话）后目录只剩 `<digest>.sqlite` | 通过 |

### 2.5 文档、路径、diff（必查 5）

| 项 | 结果 |
|---|---|
| 文档与实现一致 | README「Archive fallback」节、`assets/references/agent-memory-archive.md` 4.8 节、CHANGELOG「返工 3」条目与代码逐条对照：文件名指纹、`EvidenceBatch`、按需镜像、`coverage.sessions` 口径、N1、NB-5、带上下文回读均相符；R3.3 数值与 debug/release 口径已在文中标注 |
| 新增行无真实用户路径 | `git diff 1ed02a9 9ba4d1a` 对真实用户路径模式（Windows 用户主目录、Unix 主目录前缀）检索 **0 命中** |
| `git diff --check` | 退出 0（干净） |
| `assets/scripts/check-docs.ps1` | 退出 0，6 项全 PASS（86 个 Markdown） |
| 被检仓库工作区 | `git status --short` 为空（仅 `target/` 被 ignore） |

---

## 3. Blocking

**无。** 硬指标（清单冷构建 ≤3 min、无变化检查 ≤1 s、稳态 index ≤5 s、镜像只物化待补会话、N1/N4/NB-5、带上下文回读修复）全部由命令实测确认；未发现正确性回归或断言削弱。

---

## 4. Non-blocking

### NB-1 `apply_jsonl` 每个待补 carrier 重新解析一次全部 manifest（O(待补 carrier × manifest)）

- 位置：`agent-memory/src/recall/fulltext/fallback.rs:766`（`rebuild_group_bytes` 内 `EvidenceBatch::load`，`:771`），调用点在 `:856` 的 per-carrier 循环内 `:865`。
- 证据（同机，4 108 manifest / 236 016 索引行）：
  - 30 个 Claude carrier 变待补：index 首次 **4.877 s**，随后无待补 0.818 s → ≈4.0 s / 30 carrier（≈0.135 s/carrier）；
  - 再 60 个（累计 90）：首次 **7.881 s**，随后 0.597 s → ≈7.3 s / 60（≈0.122 s/carrier）；线性。
- 为何非阻断：不违反本轮任何目标（冷构建 3.13 s、无变化 0.594 s 均远低于预算）；仅在「大批 JSONL 会话同时消失」时显形；按生产 665 个 JSONL carrier 外推约 +80 s，仍低于 3 min 冷构建预算。
- 建议（下一轮或顺手）：`apply_jsonl` 循环外加载一次 `EvidenceBatch` 并透传给 `rebuild_group_bytes`（与 `load_inventory`/`apply_opencode` 的既有形态一致），可把该路径降到一次 manifest 解析。

### NB-2 镜像「收缩」推迟到下一次有待补会话的 index 才执行

- 位置：`fallback.rs:955`（`if !pending.is_empty()` 才调用 `:960` 的 `ensure_row_mirror`）。
- 证据：`sv3-ses-0010` 回活源（1 000 行）后 index：`archive_only_sessions` 6、`coverage.sessions` 228、其命中 `origin=live`（行为正确），但镜像仍持 7 个会话、4 000 行；直到下一次出现待补会话（`0004–0006`）才从 `sessions` 清单移除 0010 并删其行（7 000 行 / 1.79 MB）。
- 为何非阻断：镜像只是缓存；未登记会话不会入索引，读路径按 ref 代际与证据仍能回读，故无正确性/可见性影响；README 亦只描述「离开待补集合的行被删除」，未承诺即时。
- 建议：在 `apply_opencode` 中当 `gone` 非空但 `pending` 为空时也调用一次 `ensure_row_mirror`（代价一次事务），或文档明确「下一次镜像工作时收缩」。

### NB-3 旧基线代际的镜像文件不会被回收

- 位置：`fallback.rs:1169` 只清 `<digest>.building.*`（stale staging），`rows/` 下其他 `<old-digest>.sqlite` 无剪枝；镜像按基线 digest 命名（`:1218`）。
- 证据：基线代际变化会生成新 `<digest>.sqlite`（本世界仅 1 个基线，故未观察到堆积）；采集侧基线重建触发条件为「delta 链字节 > 基线 50%」（`src/capture_sqlite.rs:58`），长期使用会多次重建。
- 为何非阻断：每次遗留文件只含当时的待补会话（量级 0.26 MB/会话），且缓存目录可整体删除；不影响正确性。
- 建议：镜像写入成功后删除 `rows/` 下非当前 `inventory.current.json` 引用 digest 的 `*.sqlite`（或保留最近 N 代），与 `prune_session_cache` 的既有形态一致。

### NB-4 legacy JSON storage carrier 在补缺清单中恒为空（3705/4107 个 carrier 只付成本、不产出会话）

- 位置：`fallback.rs:543`（`extract_jsonl_group`）只认 Codex marker 与 Claude `record_meta().session_id`，`:602` 的非 Claude 记录直接忽略；`apply_opencode` 只用 SQLite 组（`:928`）。
- 证据：`current.json` 中 4 107 个 carrier 有 **3 705 个 sessions 为空**，与 3 705 个 legacy JSON 文件一一对应；索引侧这些会话只能来自 SQLite（V1/V2），legacy 文件里只有独立 `id/sessionID` 字段。
- 为何非阻断：文档明确补缺范围是「Claude 转录 / Codex rollout / OpenCode `session_v2`+`session` 行」，活源全文索引也只覆盖 SQLite V2，故不构成相对回归；但生产数据中约 3 636 个此类文件意味着这部分历史在补缺层**不可找回**，且清单要为其付出冷构建成本（本轮已确认成本可接受）。
- 建议：在 S5 限制清单中显式写明「legacy JSON storage（`json-legacy-v1`）会话不进入补缺」，或后续按 rel-path/session 文件 id 为 legacy carrier 推导会话并接入 `LegacyReader`。

### NB-5 删除会话会触发活源索引整源重建（非返工 3 引入）

- 位置：`src/recall/fulltext/build.rs:733`（`current_rows < watermark.rows` → `full=true`）。
- 证据：删 3 个会话（3 000 行）后首个 index **14.5 s**；删 7 个会话后再一次 **15.8 s**；其余无变化运行 0.59–0.69 s。触发条件在 `d47f526` 源码中同样存在（`git show d47f526:agent-memory/src/recall/fulltext/build.rs:548`），非返工 3 引入（基线二进制会先因 schema 2→3 重建，无法用同轮计时隔离该条件，故以源码对照为准）。
- 为何非阻断：删除会话「当轮切 archive + deleted_at」正确；重建是一次性成本且仍远低于 3 min。
- 建议：不属本轮范围；可在后续把「行数减少」细化为「仅当减少量无法由增量对账解释」时再整源重建。

### NB-6 环境中存在 22 个修复前的空 `s5-readback-*` 目录（非本轮产物）

- 证据：全部 ctime 2026-10-31 18:06–19:47，早于 `9ba4d1a`（21:40）；本轮 ≥18 次回读与 `cargo test` 均 0 新增；目录内为空（文件已删、目录未删），符合修复前「镜像命中路径未删目录」的形态。
- 建议：无需代码动作；可手工清理这些历史残留（本轮按要求未删除非自建临时目录）。

---

## 5. 已执行命令与退出码

| # | 命令（工作目录） | 退出码 | 关键输出 |
|---|---|---|---|
| 1 | `git log --oneline -5` / `git status --short`（`C:\wt\s5-reverify3`） | 0 | HEAD `9ba4d1a`；工作区干净 |
| 2 | `git diff --stat 1ed02a9 9ba4d1a` | 0 | 14 文件 +673/−259 |
| 3 | `cargo test --locked --no-fail-fast`（`agent-memory/`） | 0 | 710 passed / 3 ignored / 0 failed |
| 4 | `cargo check --locked` | 0 | 0 warning |
| 5 | `cargo build --locked --release` | 0 | Finished（41.79 s） |
| 6 | `git diff --numstat d47f526 9ba4d1a -- agent-memory/tests` | 0 | `1498 0`（纯新增） |
| 7 | `git diff --check` | 0 | 干净 |
| 8 | `python -I -m unittest discover -s . -p "test_recall_*.py"`（`assets/scripts/`） | 0 | 73 tests OK |
| 9 | `powershell -File assets/scripts/check-docs.ps1` | 0 | 6 项全 PASS |
| 10 | `python -I build_world.py <world>` | 0 | 4 102 carrier / 230 001 行 |
| 11 | `agent-memory init --root <world>/archive` | 0 | schema 写入 |
| 12 | `agent-memory capture-all --config <world>/recall.json` | 0 | 4 102 文件 / 1 158.7 s（采集成本，非本轮目标） |
| 13 | 删 `cache` 后 `agent-memory index --config …`（release，计时） | 0 | 首次 **17.576 s**，再次 0.656 s |
| 14 | 删 `cache/archive-inventory` 后 `index`（计时） | 0 | **3.125 s** |
| 15 | 无变化 `index` ×3（计时） | 0 | 0.594 / 0.572 / 0.602 s |
| 16 | `git archive d47f526` → `cargo build --locked --release`（基线二进制） | 0 | Finished（37.52 s） |
| 17 | `d47f526` 二进制无变化 `index` ×5 | 0 | 0.315 / 0.314 / 0.304 / 0.317 s（首轮建 schema 15.7 s） |
| 18 | 新增 5 个 legacy carrier → `capture-all` → `index` ×3 | 0 | 4.107 s / **0.647 s** |
| 19 | 删 3 会话 → `index`（计时）+ 镜像统计 | 0 | 首次 14.5 s；再 0.600 s；镜像 794 624 B |
| 20 | 删 1 会话并 capture → `index`；`memory_search` / `memory_read` | 0 | `origin=archive`、`deleted_at=…14:13:15Z`；上下文 3 段 |
| 21 | 回活源（旧时间戳）→ `index` → `memory_search` | 0 | `origin=live`、无 `deleted_at`、coverage 228 |
| 22 | 回活源后镜像暂未收缩（仍 7 会话 / 4 000 行）；下一次镜像工作时收缩 + 补入新会话 | 0 | 7 000 行 / 1 794 048 B；镜像过期推进后 8 000 行 / 2 080 768 B |
| 23 | 预置 `.building.{999,424242}` → `index` | 0 | 残留被清除 |
| 24 | 删除 `projm/multi.jsonl` → `index` → 两个 needle 检索/回读 | 0 | 各 5 命中 `origin=archive`；上下文 2 段 |
| 25 | V1-only / V1+V2 同 id 检索与回读 | 0 | `sqlite-v1` 命中 1；V1 副本 0 命中 |
| 26 | 连续 3 轮 `index` + 同一 needle ×3 检索 | 0 | 行数恒定、`hit_id` 唯一 |
| 27 | 隐藏镜像 → `memory_read` ×3（临时物化） | 0 | 0.857 s 中位、结果正确 |
| 28 | 新 delta 后（镜像过期）`memory_read` ×2 | 0 | 1.22 s 中位、结果正确 |
| 29 | `agent-memory read --root … --sha256 <delta 记录> --out …` | 0 | 390 B，哈希一致，`table=session_message` |
| 30 | 篡改 `record_sha256` 的 `memory_read` | 5 | `REF_LOCATOR_MISSING`（fail-closed） |
| 31 | `invariants.py hash/compare`（`<cache_root>` 4 112 文件 ×3 组） | 0 | changed/added/removed 全 0 |
| 32 | `invariants.py temp`（全部回读前后） | 0 | 新增 `s5-readback-*` 0 个 |
| 33 | 30 / 90 个 Claude carrier 变待补 → `index`（计时） | 0 | 4.877 s / 7.881 s（复现 NB-1） |

---

## 6. 方法与边界声明

- 未读取任何真实会话/数据库/生产 Archive/真实配置；本轮全部世界的 needle、路径与内容均为合成。报告中未出现真实用户路径。
- 未注册/修改计划任务，未运行真实 `61/62/64/90`，未重建 `assets/dist`，未执行 `git commit`；被检仓库工作区保持干净（构建产物 `target/` 已在 `.gitignore`）。
- 断言类结论均来自本轮命令实测；代码级结论（N1/N4/NB-5 修复点、`EvidenceBatch` 语义等价、`same_coordinates` 范围）均以源码逐行核对为依据，未以「实现者声明」替代验证。
- 规模数字为**本机合成世界**，与 orchestrator 真实数据仅作形态对照；绝对耗时不可跨机、跨构建（release/debug）比较。