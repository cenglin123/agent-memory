# 全文检索优先 L6 收尾与分发物 — 独立验收报告（FULLTEXT-L6-VERIFICATION）

- **Verdict：阻断需修复**
- **验收人**：独立 Verifier（全新上下文，与实现者不同模型族）
- **日期**：2026-10-30
- **对象**：隔离检出 `C:\wt\l6-verify`（detached HEAD `c170f75aa5286fb5f02b528af72264aaebb39c14`，工作区干净）；范围 `d66ddfc..c170f75`（c1bb3f0 L6 实现 + 196eedf/c170f75 分发物重建与谱系刷新）
- **隔离声明**：全部测试在临时目录与合成 fixture 中进行；未修改被检仓库文件（cargo 构建产物按任务允许产生于检出内 `agent-memory/target/`，属 gitignore 的构建目录）；未读取真实会话/知识库/生产 Archive/真实配置；未注册、未删除任何真实计划任务（任务 XML 与启动器只做生成内容检查；`-IndexSchedule`/`-RemoveIndexSchedule` 未对真实系统执行）；未跑真实 61/64 安装（61 仅在 63 式 %TEMP% 沙箱内运行）。
- **实现者完成记录**：`agent-memory/docs/verification/FULLTEXT-L6-COMPLETED.md`；本报告以代码与命令实证为准。

## 1. Blocking（须修复后复验）

### B1. `probe_ollama` 的 `build_opener` 在 try 块外，SSL 上下文构造失败会令 64 整体 FAIL（L6 引入）

- **位置**：`assets/scripts/recall_onboarding.py:837`（`opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))`，位于 838–842 行 `try` 之前）。
- **现象与证据**：`cargo test --locked --no-fail-fast`（`agent-memory/`）= **614 passed / 3 failed / 2 ignored**（26 个 target；期望 617/0/2，exit 101）。失败的 3 条全部是 L6 新链路的集成用例：
  - `recall_config_v2::onboarding_generates_v2_and_the_binary_loads_and_searches_it`、`recall_config_v2::onboarding_preserves_v1_byte_for_byte_until_explicit_migration`
  - `recall_archive::onboarding_writes_and_preserves_an_archive_source`
  - 失败详情一致：onboarding 子进程 `recall-onboarding FAIL`，detail=`[SSL] unknown error (_ssl.c:3138)`。
- **根因（已用命令逐层核实，非推测）**：CPython 3.12+ 的 `HTTPSHandler.__init__` **急切**构造 SSL 上下文（`http/client.py:848 _create_https_context` → `ssl.py:712 SSLContext(PROTOCOL_TLS_CLIENT)` → `_ssl.c:3138`），因此异常发生在 **`build_opener` 阶段、任何网络访问之前**。用 python shim 的 audit hook + settrace 抓到完整事件链：`probe_ollama:834 → probe_ollama:837 → urllib/request.py:567 (add_handler→klass()) → HTTPSHandler init → SSLError`，且 **0 条 `socket.connect`/`urllib.Request` audit 事件**——探测从未发出。异常类型是 `ssl.SSLError ⊂ OSError`，本应被 841 行 `except (URLError, OSError, ValueError)` 捕获并报 `reachable=False`，只因 837 行在 try 外而逃逸，最终走 `main()` 的兜底 except → 64 安装/检查整体 exit 1。
- **触发条件（最小 Rust spawner 复现 + 分组二分）**：子进程环境**缺 `SystemRoot`** 且存在任一 `OneDrive*`/`XDG_*`/`HOMEDRIVE`/`HOMEPATH` → `ssl.create_default_context()` 必现失败；补上 `SystemRoot` 后全部通过（`mode=ohxs → CTX-OK`）。bash 下同命令、同解释器、`env -i` 等价环境均通过——差异只在父进程传入的环境块。
- **为什么 cargo test 撞上**：`agent-memory/tests/support/mod.rs:15-17` 的 `PASSTHROUGH_ENV` 用大小写敏感的 `saved.get("SystemRoot")`/`"ComSpec"` 回传；本机 shell 导出的是大写 `SYSTEMROOT`（Rust `std::env::vars` 保留原大小写），回传落空 → 测试子进程无 `SystemRoot` → 撞上 B1。单线程 `--test-threads=1` 仍复现（非并行竞态）。
- **影响**：(a) 验收门 #5 客观不达标（614≠617）；(b) 产品语义违背同提交声明的"可选能力体检只报告不失败"契约——在证书库/OpenSSL 配置异常的机器上（服务上下文、精简环境、被劫持的代理环境变量等），`64` 会把"探测不可达"升级为整体安装失败。
- **建议修复**（一行级）：把 `Request`/`build_opener`（834–837 行）移入 `try`，或改用 `http.client.HTTPConnection` 直连（纯 http，不触碰 ssl）。修复后重跑 `cargo test --locked --no-fail-fast`（PowerShell 环境应回 617/2；msys 环境亦不再硬失败）。次级加固（非本条必须）：`tests/support/mod.rs` 的 PASSTHROUGH 改为大小写不敏感匹配。

### B2. `63-verify-agent-memory-lineage.ps1` 端到端安装用例失败（19 项中 2 FAIL；L1 起遗留，非 L6 回归）

- **现象与证据**：`powershell -File assets/scripts/63-verify-agent-memory-lineage.ps1` → 14 个谱系语义场景（正/反向 × PS 5.1/PS 7）+ 沙箱 `-Verify` 全部 PASS；**2 个端到端用例 FAIL**：`HEAD~1 谱系下安装 + 90 审计通过（PS 5.1/PS 7）`——沙箱内 `61-install-agent-memory.ps1` exit 1。脚本总 exit 1。
- **根因（沙箱精确复现）**：63 的沙箱 `assets\scripts` 只复制 4 个文件（`61/90/agent-memory-common/opencode-desktop-common`），而 61 自 f68c79a（full-text L1）起 §2.5 无条件安装语义辅助脚本，缺源即抛 `语义辅助脚本来源缺失：…recall_embedding.py（套件不完整…）`。复现：按 63 同款沙箱（commit=HEAD、tree=`0dcc1a29…`、分发物同哈希）跑 61 → 同报错 exit 1；**补拷 `recall_embedding.py` 后 exit 0**（`===== agent-memory 0.1.0 安装完成 =====`）。PS 7 的"90 审计未通过"是该安装失败的下文（运行时根未建立）。
- **影响**：必查项"`63 通过`"客观不满足；该门自 L1 起静默变红（63 未随 61 的辅助脚本需求更新），说明此前的"63 通过"结论至少自 L1 起未复跑。
- **建议修复**：把 `assets\scripts\recall_embedding.py` 加入 63 的沙箱复制清单（第 ~109 行文件列表），重跑 63 预期 19/19。**归因说明**：63 与 61 在 `d66ddfc..c170f75` 内均无改动，此缺陷先于 L6 存在，不属于 L6 回归；但清单门要求其通过，故列入 blocking。

## 2. Non-blocking

### N1. 分发物不可跨目录字节复现：`build-release.ps1 -Verify` 在本检出失败；"可复现双构建"校验实为空转（先于 L6 存在）

- **现象**：`powershell -File agent-memory/build/build-release.ps1 -Verify`（钉定工具链 rustc/cargo 1.99.0 与清单完全一致、源树哈希一致 `0dcc1a2944aa…`、75 个构建输入、工作区干净）重建得 `0cd3ccb211950392f5aed0956d45b95840b156fe0448d61d1b7c52205a30476c`，与清单 `cb7a9d1811421804c27d5092b4f1775854ca4cfe626acd40b49a3e97cc72471e` 不一致 → exit 1（`build-release.ps1:204`）。
- **字节级定位（`cmp -l` + PE 解析）**：9,713,152 字节中**仅 24 字节不同、5 个簇**，全部是链接器非确定性元数据：
  1. COFF `TimeDateStamp`（0x110–0x113）：dist `0x6ae38c86`=2026-10-29T18:12:22Z vs 重建 `0x6ae38da9`=18:17:13Z；
  2–4. 三个 `IMAGE_DEBUG_DIRECTORY` 条目的 `TimeDateStamp`（条目间隔恰 28 字节）；
  5. CodeView（RSDS）PDB **GUID** 16 字节；两侧 pdb 名同为 `agent_memory.pdb`，无路径字符串，文件大小与清单 `binaryBytes` 完全一致。
  **代码与数据段逐字节相同** → 分发物确由本源树+钉定工具链产出，来源真实性成立。
- **"可复现双构建"为何空转**：删除 `target\x86_64-pc-windows-msvc\release\agent-memory.exe` 后重跑 `cargo build --release --locked --target x86_64-pc-windows-msvc` → `Finished` 仅 0.06 秒、无任何 `Compiling` 行、exe 原样恢复（同哈希）——cargo 指纹未失效时从 `deps\` 硬链恢复产物，**从不二次链接**，故该门无法发现链接时间戳/GUID 非确定性。dist 内嵌时间戳（18:12:22Z）与 manifest `builtAtUtc=18:12:24Z` 仅差 2 秒亦吻合此机制。
- **评估**：`-Verify` 只可能在原构建目录（deps 缓存尚存）通过；63 脚本注释"编译器会嵌入路径"归因不准（实为时间戳+GUID）。分发的完整性与来源已由其他判据独立证实（见 §3.1），故不阻断。
- **建议**：链接参数加 `/Brepro`（并 `/PDBALTPATH`）实现真字节复现；或把 `-Verify` 改为"剥离调试目录元数据后比较代码段"；二次构建改用干净 `--target-dir` 使该门真正触发（届时现链接方式会显式失败，需先做上一条）。

### N2. `tests/support/mod.rs` PASSTHROUGH 大小写敏感（先于 L6）

- `PASSTHROUGH_ENV = ["PATH", "SystemRoot", "ComSpec"]` 用精确大小写回传；msys/git-bash 导出大写 `SYSTEMROOT` 时子进程丢失 `SystemRoot`。独立于 B1 也值得修（大小写不敏感匹配），修后 cargo test 在 msys 环境的稳定性不再依赖 B1 的容错。

### N3. 已核实为正常、无需动作的观察

- 清单 `source.commit=196eedf` = HEAD~1 而非 HEAD：符合谱系语义（祖先提交且构建输入与工作区一致；c170f75 仅改文档与 dist 清单，未触构建输入）。判据实测：`90 -AgentMemoryOnly` 对本检出输出 `commit 196eedf…（HEAD 祖先）；树 0dcc1a29…` 且 exit 0；63 场景 2 与沙箱 `-Verify` 亦验证该语义。
- 重复 `index` 后包装器日志 `total_rows=0`（源状态 `unchanged`）：与 index 报告语义一致，非缺陷。
- `d66ddfc..c170f75` 新增行路径隐私扫描（`chenr`/`C:\Users\<非占位>`/`/c/Users/<非占位>`）零命中；文档统一用 `<user-home>` 占位。

## 3. 已执行命令与结果（verifier 环境：Windows，PS 5.1.19041.7802 + pwsh 7.6.9，rustc/cargo 1.99.0 钉定，Python 3.13.17）

| # | 命令 | 结果 |
|---|---|---|
| 1 | `git -C C:/wt/l6-verify rev-parse HEAD` / `status --short` / `log d66ddfc..c170f75` | `c170f75aa…` ✓；工作区干净；15 文件 +1139/−59 |
| 2 | `sha256sum assets/dist/agent-memory/agent-memory.exe` | `cb7a9d18…471e` == 清单 `binarySha256` == 任务给定值 ✓ |
| 3 | `powershell -File agent-memory/build/build-release.ps1 -Verify` | **exit 1**（见 N1；工具链/树哈希一致，重建哈希不一致） |
| 4 | `cmp -l` + PE/调试目录解析（dist vs 重建） | 24 字节差异 = 4×TimeDateStamp + PDB GUID；代码数据逐字节同（N1） |
| 5 | 删 exe 后 `cargo build --release --locked --target x86_64-pc-windows-msvc`（同 RUSTFLAGS） | `Finished in 0.06s`，无 Compiling，哈希复原（双构建空转实证，N1） |
| 6 | `powershell -File assets/scripts/63-verify-agent-memory-lineage.ps1` | **exit 1**：14 语义场景 + 沙箱 `-Verify` PASS；端到端安装 2 FAIL（B2） |
| 7 | 沙箱复现：61（缺 `recall_embedding.py`）→ **exit 1** `语义辅助脚本来源缺失`；补文件后 → **exit 0** | B2 根因确证 |
| 8 | `powershell -File assets/scripts/90-audit.ps1 -AgentMemoryOnly -UserRoot <temp>`（本检出） | **exit 0**，8 PASS + 2 WARN；判据输出 `commit 196eedf…（HEAD 祖先）；树 0dcc1a29…`；WARN 未改退出码（#4 实测） |
| 9 | `powershell -File assets/scripts/62-verify-agent-memory-offline.ps1`（PS 5.1） | **37/37 PASS，exit 0** |
| 10 | `pwsh -File assets/scripts/62-verify-agent-memory-offline.ps1`（PS 7.6.9） | **37/37 PASS，exit 0** |
| 11 | 62 新增用例代码审查（`git diff` 全文核对） | 确认断言 `resolved_mode=hybrid`、`exact` 命中、`since` 无错误、删缓存回落 `legacy` ✓ |
| 12 | `python -m pytest -q test_recall_onboarding.py test_recall_embedding.py test_recall_eval.py`（`assets/scripts`） | **66 passed，exit 0** ✓ |
| 13 | `cargo test --locked --no-fail-fast`（`agent-memory/`，完整日志复核） | **614 passed / 3 failed / 2 ignored**（26 target），exit 101 —— 见 B1 |
| 14 | `cargo test --locked --test recall_config_v2 -- --test-threads=1` | 仍 2 failed（排除并行竞态） |
| 15 | python shim（副本 exe + `_pth` + sitecustomize audit/trace）+ 失败子进程追踪 | SSLError 产生于 `build_opener`（recall_onboarding.py:837，try 外），零网络事件 |
| 16 | 迷你 Rust spawner 分组二分（base/±OneDrive/±XDG/±HOMEDRIVE/±SystemRoot） | 缺 `SystemRoot` 且任一 profile 变量在场 → `SSLContext` 必现失败；有 `SystemRoot` → 全 PASS |
| 17 | 64 实测驱动（临时 UserRoot 含**中文+空格** `用户 数据 根目录`，17 项） | **17/17 PASS**，详见表下说明 |
| 18 | `r.probe_ollama('qwen3-embedding:4b')` 实测本机回环 `/api/tags`（只读，未 pull） | `reachable=True pulled=True models=9`（函数只访问 `http://127.0.0.1:11434`，代码审查 + 实测一致） |
| 19 | `check-docs.ps1` | PASS（6 项、70 个 Markdown），exit 0 |
| 20 | `95-test-docs-governance.ps1` | 67/67 用例通过，exit 0 |
| 21 | `git diff d66ddfc..c170f75` 路径隐私扫描 + 文档一致性审查 | 新增行零真实路径命中；README/INSTALL/UPGRADE/archive 4.5/agent-memory README/CHANGELOG/CURRENT/verification README 与实现行为一致（含 `-SkipEmbedding` 与显式参数互斥、`-RemoveIndexSchedule` 免 `-VaultPath`、WARN 审计项、`projector_version_changed` 重建说明） |

第 17 行 64 实测矩阵（均在临时目录，退出码与关键断言）：
- T1 全新安装 `-SkipEmbedding -BuildIndex`：exit 0；无嵌入块、`cache_root=<UserRoot>/.agent-memory/cache`、`index-build PASS`（codex built rows=1 vectors=skipped）、生成 `live/codex/fts.sqlite`、报告与日志无正文（`synthetic-l6-64-line`/`rollout` 零泄漏）、未注册任务时无 scripts 目录。
- T2a 缺省写入 `qwen3-embedding:4b/512/l2/ollama` 并报告；T2b 显式三参数覆盖既有块并逐项报告 `嵌入 model_id/dimension/backend`，`cache_root` 保留。
- T3 重跑保留；T4 `-SkipEmbedding` 逐字节不删不写；T5 `-SkipEmbedding`+显式参数 → exit 1 `mutually exclusive`。
- T6 手写 device/archive 源 + 自定义 `cache_root` + 自定义 `source_order`（codex,kb,device,archive）→ 安装后原样保留。
- T7 v1 配置（含显式 `-EmbeddingModel` 在场）→ 文件逐字节不变，报告 WARN「v1 按原样保留」。
- T8 `-CheckOnly`：exit 0；UserRoot 全树（7 文件）哈希+大小+mtime 前后一致；报告含 `recall-embedding`/`index-schedule`（未注册 → WARN）。T8b 无 `recall.json` → exit 1 干净失败。
- T9a/b（FakeTaskExecutor，未触真实任务）：任务名 `agent-memory-index`；XML=LogonTrigger+`Repetition Interval PT45M`+`MultipleInstancesPolicy IgnoreNew`+`LogonType InteractiveToken`+`RunLevel LeastPrivilege`+当前用户 `UserId`+动作 `wscript.exe //B //NoLogo "<vbs>"`+`ExecutionTimeLimit PT2H`+`Hidden=false`，UTF-16LE BOM + `encoding="UTF-16"`；VBS `shell.Run "powershell.exe … -WindowStyle Hidden -File ""…index-refresh.ps1"" -UserRoot ""…用户 数据 根目录""", 0, False`（中文+空格路径双引号正确、UTF-16LE BOM）；PS1（UTF-8 BOM）持 `Local\agent-memory-index` 命名互斥量、`WaitOne(0)`、`skipped='already_running'`、`2>$null`、日志仅白名单字段（ok/exit_code/total_rows/sources[source_id,status,rows,vectors,duration_ms]/started_at/finished_at/error/skipped），无 path/snippet/detail/stderr 正文；15→45 重复注册恒 1 任务；remove PASS→SKIP。
- T10a 实跑生成的 `index-refresh.ps1`（真实分发物）：exit 0，`index-last.json` 仅报告字段、`ok=true`、codex built rows=1、无正文；T10b 持互斥量并发运行 → `skipped=already_running, exit_code=0`。

## 4. 必查清单对照

| 必查 | 结果 |
|---|---|
| 1. exe SHA256 == manifest | ✅ |
| 1. `build-release.ps1 -Verify` | ❌ 本检出失败（N1，根因实证，分发物来源另行证实） |
| 1. `63…lineage.ps1` | ❌ 2/19 FAIL（B2，根因实证，可一行修复） |
| 1. 90 谱系判据（`-AgentMemoryOnly`、临时 UserRoot）对本检出 | ✅ exit 0 |
| 2. 62 PS 5.1 / PS 7 | ✅ 37/37 × 2；新增用例断言属实 |
| 3. 64/recall_onboarding（临时 UserRoot 实测 + 生成内容检查） | ✅ 17/17；**只检查生成内容，未注册真实任务** |
| 4. 90 WARN 不改退出码；只读元数据 | ✅ 代码审查（`ConvertFrom-Json` + `Test-Path` + `Get-ScheduledTask`，不打开 sqlite/jsonl 正文）+ 实测（2 WARN，exit 0） |
| 5. pytest 三件套 | ✅ 66 passed |
| 5. `cargo test --locked --no-fail-fast` | ❌ 614/3/2（期望 617/0/2）→ B1 |
| 6. 文档一致、路径隐私、`check-docs.ps1`、`95-test-docs-governance.ps1` | ✅ |

## 5. 复验指引（修复后）

1. B1：`recall_onboarding.py` 834–837 行移入 try（或改 `http.client` 直连）→ `python -m pytest` 三件套（66）+ `cargo test --locked --no-fail-fast`（期望 617/2；msys 环境亦应全绿）+ 重跑本报告 §3.17 的 64 驱动（CheckOnly 路径不受影响）。
2. B2：63 复制清单补 `recall_embedding.py` → 重跑 63（期望 19/19）。
3. 两项均修复且复验绿后，本验收可转「通过」；N1/N2 建议另立小任务（/Brepro 或 -Verify 语义调整；PASSTHROUGH 大小写）。

—— 本报告仅写入 sentinel 一个文件；被检检出未做任何修改，未执行任何 git 写操作。
