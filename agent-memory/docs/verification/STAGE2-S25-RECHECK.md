# STAGE2-S25 修复复查报告：升级提示文档与不可解码孤儿 `.zst` 的降级计数

- 被验对象：隔离检出 `C:\wt\s25-recheck`，`HEAD = 6d880db`（父 `410b2e3`），复查前 `git status --short` 为空。
- 复查范围：`git diff 410b2e3 6d880db`（6 文件，+242/−5）：`UPGRADE.md`、`agent-memory/README.md`、`assets/references/agent-memory-archive.md`、`agent-memory/src/archive/mod.rs`、`agent-memory/tests/p7_storage_compaction.rs`、上一轮验收报告。
- 参照：上一轮报告 `agent-memory/docs/verification/STAGE2-S25-VERIFICATION.md`（verdict 阻断需修复，B1 + N1–N8）。
- 角色：独立 Verifier（全新上下文，与实现者不同模型族），只读仓库；本报告是唯一写入文件。
- 环境：Windows；`cargo/rustc 1.99.0`；Python 3.13.17；`zstd` CLI 仅用于构造合成 fixture；旧二进制由 `git archive db15538` 导出后独立构建，修复前二进制由 `git archive 410b2e3` 导出后独立构建。
- 隔离：全部实验在专用目录 `C:\wt\vr-s25-a7f3`（`root1` / `root_orph` / `root_refcorrupt` / `root_dual` / `root_audit` / `root_downgrade` / `root_lock`）内用合成 fixture 完成；`CARGO_TARGET_DIR` 全部指向该目录。**未读取** `C:\Users\<user-home>\.local\share\opencode`、`.claude\projects`、`.codex`、`.session-evidence`、`.agent-memory`、`.config\opencode`；未运行真实 61/64/90，未执行生产回填，未 commit。

## 结论（verdict）

**通过**

B1 已在三份文档补齐且与旧/新二进制实测行为逐条一致；N2 的降级计数确实生效，同时被 manifest 引用对象的 fail-closed 语义未被削弱（与修复前二进制逐项对照一致）；新增用例不是空转（在修复前代码上会失败）。剩余 6 条均为非阻断的文档/测试措辞与覆盖面问题，见「新发现」。

---

## 必查项逐条结论

### 1. B1：升级顺序、旧二进制症状、无降级路径与逆操作指引 —— **通过**

三份文档均写入必查要点，且与实际行为一致：

| 文档 | 位置 | 内容核对 |
|---|---|---|
| `agent-memory/README.md` | `:579-585` | 明确「install the binary that understands `.zst` objects **before** running `compact`」；旧二进制「reports those objects as `missing_objects` plus `.zst` `orphan_objects` and `read` fails with `not found`」；「There is no downgrade path」+ 指向 `62-verify-agent-memory-offline.ps1` S2.5 用例的逆操作 |
| `assets/references/agent-memory-archive.md` | `:335` | 「必须先换成支持 `.zst` 对象的新版二进制，再运行 compact」；`missing_objects` + 带 `.zst` 的 `orphan_objects` + `read` 报 `not found`；「不提供降级路径」+ 逆操作指针 |
| `UPGRADE.md` §7 | `:133-139` | 三步固定顺序（61 装新二进制 → `compact --dry-run` → 正式迁移 → 之后回填/定时采集）；「不提供降级路径」+ 逆操作指针；旧二进制症状只写「会把它们报成缺失」（见 F1） |

实测复核（`db15538` 构建的旧二进制读取新二进制 compact 后的根 `root1`，2 个 `.zst` 对象）：

- `read --sha256 7565…e9dc --domain default` → **exit 6**，`{"ok":false,"error":"not found: object 7565…e9dc"}`。
- `status` → **exit 0**，`missing_objects` 2 条（无后缀 sha）、`orphan_objects` 2 条（`7565….zst`、`bdcb….zst`）；`storage` 为 `null`（旧版本无该字段）。
- 新二进制同一 root `status`：`missing=[]`、`orphans=[]`、`dual=[]`。

逆操作指引可执行性复核（按 62 S2.5 用例同一手法）：对新二进制已 compact 的 `root1` 逐对象 `read --out` 解码回明文、落回内容地址并删除 `.zst`（2/2 成功）后，旧二进制 `status` → **exit 0**、`missing=[]`、`orphans=[]`；旧二进制 `read` 同一 sha → **exit 0**，输出 sha256 与内容地址一致。即文档描述的「先解回明文再回退」链路成立。

### 2. N2：不可解码的未引用 `.zst` 只降级计数，不阻塞 —— **通过**

修复点：`agent-memory/src/archive/mod.rs:124-128`（新增 `unreadable_objects` 字段，camelCase 序列化为 `unreadableObjects`）与 `:1086-1097`（`storage_stats` 仅在对象**未被任何 manifest 引用**且编码为 zstd 时调用 `decoded_len_of`；失败改为 `unreadable_objects += 1` 且原始字节记 0）。

复现上一轮场景（`root_orph` = 正常根 + 一个合法 sha 名、内容 `\x00not a zstd frame` 的孤儿 `.zst`）：

- `status` → **exit 0**；`reconcile.storage = {"plainObjects":0,…,"zstdObjects":3,"zstdOriginalBytes":34807,"zstdStoredBytes":459,"unreadableObjects":1}`；`orphan_objects = ["db4a…5624"]`；`missing=[]`、`dual=[]`。
- `reconcile` → **exit 0**；`coverage` → **exit 0**，`coverage.storage` 同样含 `unreadableObjects=1`（`src/coverage.rs:553` 走同一 `storage_stats`）。
- `capture`（新采集第三个来源）→ **exit 0**，`objectsNew=1`，报告 `orphanObjects` 仍列出 `db4a…5624`（说明 capture 收尾的 `reconcile` 正常执行）。
- 修复前对照（`410b2e3` 构建）：同一 `root_orph` 上 `status`/`reconcile`/`coverage` 均 **exit 3**，`io error: …\db4a….zst: Unknown frame descriptor`；`scrub` 亦 **exit 3**。修复后 `scrub` **exit 0**（`objectsChecked=3`、`orphanObjects=[db4a…]`、`corrupted=[]`）。即修复同时解除了 scrub 的连带失败（文档未列 scrub，见 F4）。

被 manifest 引用的对象损坏时仍 fail-closed（`root_refcorrupt` = 正常根 + 翻转一个被引用 `.zst` 中部字节）：

- `read --sha256 7565…e9dc` → **exit 6**，`integrity check failed: object 7565…e9dc is not valid zstd: Data corruption detected`。
- `scrub` → **exit 10**，`ok=false`、`corrupted[0].sha256=7565…e9dc`（带 references/sources）、`anomalies` 有对应条目。
- `status` → **exit 0**、`unreadableObjects=0`；`capture` → **exit 0**（不校验无关对象，既有语义）。
- 与修复前二进制逐项对照：`read` 6 / `scrub` 10 / `status` 0，**完全一致**；代码 diff 也只改了「未引用」分支，引用对象仍按 manifest 取原始字节、不走解码。故修复没有把引用对象的损坏静默吞掉；`status` 不校验被引用对象内容属既有设计（完整性由 `read`/`scrub` 承担，README 与 4.7 已写明）。

### 3. 新增用例 `undecodable_orphan_zst_never_fails_status_or_capture` —— **通过（非空转）**

- 位置：`agent-memory/tests/p7_storage_compaction.rs:757-794`。流程：init + 采集一个来源 → 写入 `hex_sha256(b"hand-placed orphan")` 命名的 `.zst`（内容 `\x00not a zstd frame`）→ 断言 `status` 成功且 `reconcile.storage.unreadableObjects == 1` → 断言该 sha 仍在 `orphan_objects` → 再采集第二个来源并断言 `report.ok`。
- 非空转证明：把该测试原样拷入 `410b2e3` 源码（独立目录）运行 → **FAILED**（`tests\p7_storage_compaction.rs:779` 断言失败，stderr 即 `Unknown frame descriptor` 的 `io error`）；在 HEAD 上运行 → `ok`。说明该用例确实被修复行为区分，且 `unreadableObjects==1` 的断言不是恒真（文件名若未被 `object_files` 枚举，断言会失败）。
- 覆盖边界（非阻断，见 F6）：只直接断言 `status` 与 capture；`coverage`/`reconcile` 未单独断言（与 `status` 同一 `storage_stats` 路径，实测已过）；未断言「被引用对象损坏仍 fail-closed」。

### 4. N1 / N3 / N4 / N7 文档措辞与实现一致性 —— **通过（N1 有一处历史记录残留，见 F2）**

- **N1**：`README.md:586-591` 与 `archive.md:336` 改为「读取一律 fail-closed；崩溃窗口内 `.zst` 解码失败而同 sha 明文仍在时，读取报错、两份文件都保留供人工恢复，compact 记 `corrupted` 且不删除任何文件」。实测：人为造 plain+`.zst` 并存并翻转 `.zst` 尾部字节 → `read` **exit 6** `Data corruption detected`；`compact` **exit 3**、`corrupted=1`、`converged=0`、两个文件都保留。文档与实现一致。残留：`STAGE2-S25-COMPLETED.md:34` 仍写「保留明文兜底」（F2）。
- **N3**：`archive.md:23` 改为「常态下同一 sha 只有一种编码，`compact` 崩溃窗口内的短暂并存由下一次 `compact` 收敛」，与 4.7 的崩溃窗口语义（`:333`）不再矛盾；全仓仅上一轮验收报告在引用旧措辞。
- **N4**：`README.md:597-599` 与 `archive.md:337` 明说「只做内容匹配、不校验内容地址；完整性由 scrub 负责」。实测：放一个结构合法但内容与文件名不符的 `.zst`（`zstd -3` 压入 `NEEDLE-s25-wrong-address …`，文件名取自另一段内容）→ `audit-scan` **exit 0**、`objects=3`、`decodedBytes=35467`、`filesWithHits=1`、`hitsByRule={"needle":1}`、`errors=[]`；同一 sha 的 `read` 被拒（**exit 5**，`has no committed reference (orphan or unknown; fail-closed)`）。与文档一致。
- **N7**：`archive.md:334` 新增「compact 被强行中断后会留下锁文件：先确认没有采集或 compact 进程在运行，再删除 `locks/capture.lock` 后重跑（见 4.6）」。实测：预置 `locks/capture.lock` 后 `compact` **exit 4**，错误串 `capture lock already held … (a hard-killed run leaves this file behind; remove it only after confirming no capture is running)`；`scrub` 同样 exit 4。文档、错误信息、退出码一致。

### 5. 工程卫生与测试总量 —— **通过**

- `cargo test --locked --no-fail-fast`（`CARGO_TARGET_DIR` 外部）：**exit 0**，30 个 test binary 汇总 **678 passed / 3 ignored / 0 failed**（677 基线 + 新增 1 条）；新用例在日志中为 `test undecodable_orphan_zst_never_fails_status_or_capture ... ok`。
- `git diff --check 410b2e3 6d880db` → **exit 0**（无空白错误）。
- 新增行 248 行；`grep -c 'C:\Users'` = **0**，`grep -c 'chenr'` = **0**（不含真实用户目录路径）。
- 被改动的 6 个文本文件（含新增的上一轮验收报告）均为 UTF-8 无 BOM；`git ls-files --eol` 索引侧均 `i/lf`（工作区 CRLF 由 autocrlf 造成，非提交内容）。
- 复查期间未写入被检仓库：`C:\wt\s25-recheck` 的 `git status --short` 前后均为空。

---

## 新发现（均非阻断）

### F1. `UPGRADE.md` 只写了旧二进制「报成缺失」，漏掉 `.zst` 孤儿症状

- 位置：`UPGRADE.md:135` —— 「旧版读不懂 `.zst` 对象，会把它们报成缺失」。
- 证据：旧二进制实测同时产出 `missing_objects`（2）与带 `.zst` 的 `orphan_objects`（2）。README（`:581-582`）与 4.7（`:335`）都写全了，UPGRADE 少了 orphan 一半。
- 影响：`UPGRADE.md` 是最贴近升级操作的入口，排障时看到孤儿列表会找不到解释。建议补一句「同时出现带 `.zst` 的孤儿对象」。

### F2. `STAGE2-S25-COMPLETED.md` §2.4 仍保留「保留明文兜底」的旧措辞（N1 残留）

- 位置：`agent-memory/docs/verification/STAGE2-S25-COMPLETED.md:34` —— 「`.zst` 校验失败（外部损坏）时保留明文兜底、只计数不删任何文件」。
- 证据：实测读取**不回落**明文（`read` exit 6），只是两份文件都保留供人工恢复；README/4.7 已改为准确表述。该完成记录与现行文档、实现三者不一致。
- 影响：仅文档；建议改为「保留明文文件供人工恢复，读取仍 fail-closed」。该文件是阶段完成记录，是否修订由 orchestrator 决定。

### F3. CHANGELOG 的 S2.5 条目未随修复更新（677→678、缺 `unreadableObjects`）

- 位置：`CHANGELOG.md:12` —— 仍写「**677 passed** / 3 ignored / 0 failed」「`tests/p7_storage_compaction.rs` **12 条**」，且全条目未提 `storage.unreadableObjects` 与「不可解码孤儿不再阻塞采集」。
- 证据：HEAD 实测 678 passed、p7 13 条、`status`/`coverage` 输出新增字段。
- 影响：仓库 `AGENTS.md` 完工清单要求「脚本行为变更必写」「遗留风险 ⚠️ 标记」；本次是用户可见的 CLI 输出字段新增。先例（`1e253f5` S1 修复）未改 CHANGELOG，故判非阻断；建议追加一条修复说明或标注。

### F4. 文档列举的「不再失败的命令」漏了 `scrub`

- 位置：`agent-memory/README.md:591`、`assets/references/agent-memory-archive.md:336` —— 只列 `status` / `reconcile` / `coverage` / capture。
- 证据：修复前 `scrub` 在同一孤儿场景下 **exit 3**（`Unknown frame descriptor`），修复后 **exit 0** 且 `orphanObjects` 列出该 sha。即 `scrub` 同样受益且同样是 `reconcile` 的消费者。
- 影响：文档不完整但不错误；建议改为「任何走 `reconcile` 的命令（`status`/`reconcile`/`coverage`/`scrub`）与采集」。

### F5. `unreadableObjects` 的字节口径未说明：原始字节记 0，存储字节仍计入

- 位置：`agent-memory/src/archive/mod.rs:1090-1096`（失败时 `original_len = 0`，但 `zstd_stored_bytes` 已在前面累加该文件大小）；文档仅说「计入 `storage.unreadableObjects`」（`README.md:589-591`、`archive.md:336`）。
- 证据：`root_orph` 的 `status` 为 `zstdStoredBytes=459`（442 已引用 + 17 孤儿）、`zstdOriginalBytes=34807`（不含孤儿），口径混用；若某库的 zstd 组全为不可解码孤儿，则会出现 `storedBytes > originalBytes`，而 `62-verify-agent-memory-offline.ps1:363` 以 `stored < original` 作为压缩生效断言（该用例只在健康根上跑，不会触发）。
- 影响：统计可解释性与潜在断言语义。建议在文档一句说明「不可解码对象只计存储字节与计数，原始字节未知按 0 计」，或让 62 的断言显式排除 `unreadableObjects`。

### F6. 逆操作指针依赖单域读取；新增用例的断言面偏窄

- 位置：`UPGRADE.md:139`、`README.md:583-585`、`archive.md:335` 指向 62 的 S2.5 用例；该用例用 `read --domain default` 逐个解码（`assets/scripts/62-verify-agent-memory-offline.ps1:322-329`）。
- 证据：`read` 的访问检查要求对象在每个引用 manifest 的 access 标签中都含该域（`archive/mod.rs:1776-1806`），生产库若存在非 `default` 域的对象，照抄该片段会对这些对象报 `access denied`。
- 另一处：新用例（`tests/p7_storage_compaction.rs:763-794`）只断言 `status` + capture；未覆盖 `coverage`/`reconcile` CLI 与「被引用对象损坏仍 fail-closed」。
- 影响：均为覆盖面/可操作性提示；建议逆操作说明补一句「按对象的访问域选择 `--domain`」，用例可加一条 `coverage` 断言。

---

## 已执行命令与退出码

> 除最后写入本报告外未写入任何被检仓库；两个检出（`s25-recheck`、`stage2-s2`）复查前后 `git status --short` 均为空/仅本报告。

| 命令 | 结果 |
|---|---|
| `git -C C:\wt\s25-recheck archive db15538 agent-memory \| tar -x` + `cargo build --locked --offline`（独立目录） | exit 0；旧二进制 |
| `git -C C:\wt\s25-recheck archive 410b2e3 agent-memory \| tar -x` + `cargo build --locked --offline` | exit 0；修复前二进制 |
| `cargo build --locked`（HEAD，`CARGO_TARGET_DIR` 指向实验目录） | exit 0 |
| `cargo test --locked --no-fail-fast`（HEAD） | **exit 0**；678 passed / 3 ignored / 0 failed（30 binary） |
| 新用例在修复前源码上运行 | **FAILED**（`:779`，`Unknown frame descriptor`）；HEAD 上 ok |
| 新二进制 `status` / `reconcile` / `coverage` on `root_orph` | 0 / 0 / 0；`unreadableObjects=1`，孤儿仍报告 |
| 新二进制 `scrub` on `root_orph` | exit 0；`objectsChecked=3`、`orphanObjects=[db4a…]`、`corrupted=[]` |
| 新二进制 `capture`（第三来源）on `root_orph` | exit 0；`objectsNew=1`，`orphanObjects` 仍含 `db4a…` |
| 修复前二进制同场景 `status` / `reconcile` / `coverage` / `scrub` | 3 / 3 / 3 / 3（`Unknown frame descriptor`） |
| 被引用 `.zst` 损坏：`read` / `scrub` / `status` / `capture` | 6（integrity）/ 10（ok=false, corrupted）/ 0 / 0 |
| 修复前二进制同一损坏根 `read` / `scrub` / `status` | 6 / 10 / 0（与修复后一致，无回归） |
| 双编码 + `.zst` 损坏：`read` / `compact` | 6（`Data corruption detected`）/ 3（`corrupted=1`、`converged=0`、两份文件保留） |
| `audit-scan`（内容与地址不符的合法帧） | 0；`filesWithHits=1`、`bytes`/`errors=[]`、无内容泄露 |
| 同一 sha 的 `read`（未引用孤儿） | 5（`no committed reference … fail-closed`） |
| 旧二进制读 compacted root：`read` / `status` | 6（`not found`）/ 0（`missing=2`、`orphans=2` 带 `.zst`） |
| 逆操作后旧二进制 `status` / `read` | 0（`missing=[]`、`orphans=[]`）/ 0（sha 一致） |
| 预置 `locks/capture.lock` 后 `compact` / `scrub` | 4 / 4（提示「确认无进程后再删锁」） |
| `git diff --check 410b2e3 6d880db` | exit 0 |
| 新增行 `grep -c 'C:\Users'` / `grep -c 'chenr'` | 0 / 0（共 248 新增行） |
| 被改动 6 个文本文件 BOM 检查 / `git ls-files --eol` | 均无 BOM；索引 `i/lf` |

---

## 边界与未覆盖

- 未复跑 62/90 全流程（需分发物同款 release 构建与清单同步），仅针对本次修复涉及的命令与文档指向做了定点复核；62 中 S2.5 用例只做代码与行为等价性核对（逆操作按同一手法手工复现）。
- 未执行生产回填、真实 869 MB compact、真实 61/64/90；未读取任何真实数据。
- `UPGRADE.md` 的「先 61 装新二进制」以 `61` 脚本安装 `agent-memory.exe` 的既有语义为准（`61-install-agent-memory.ps1:31/100-102`），未实跑安装。
- 报告中的路径均为实验目录与仓库相对路径，未包含真实用户目录。
