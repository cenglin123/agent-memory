# 统一混合检索 P0 交付前修复（FIXED）

**本文件是 P0 修复的唯一 sentinel。** `HYBRID-P0-COMPLETED.md` 保留为 P0 初次交付记录，并已同步本次修复项与检查要求。

- 真实数据：**否**。全部测试使用临时目录合成 fixture；未读取真实会话、知识库或生产 Archive，未执行采集。
- 分发物：已由 orchestrator 重建并安装，SHA256 `9ac3acb892398052a7d704011e36c3f3f132a55f4a5952b5722495e928387408`；manifest `source.commit=0cf92448ff03cd603a3e542916d40abd2411f14c`。
- 范围：仅 P0 合同基线修复，不实现 P1 Archive provider、P2 事件存储或 P3 embedding。

## 1. 修复项

### F1 · 非 default domain 不再静默生效

- `memory_search`：当生效 domain 非 `default`（请求显式指定或配置层显式非 default）时，每个请求源返回 `status=unavailable`、`code=DOMAIN_UNREACHABLE`、`detail.reason=domain_not_available_without_archive_provider`，无命中，exit 4；绝不回退到 default 域结果。
- `memory_read`：同样条件返回 `ok=false`、`code=DOMAIN_UNREACHABLE`，exit 5（访问拒绝）。
- `default` 生效时保持原行为；v1 配置的隐式 default 行为不变。
- MCP 与 CLI 均新增反例；P1 才把 Archive access 标签接入该谓词。
- 实现：`src/recall/search.rs`、`src/recall/read.rs`、`src/recall/contract.rs`（`DOMAIN_*` 读取退出码为 5）。
- 测试：`tests/recall_cli_mcp.rs::explicit_non_default_domain_is_unreachable_and_never_falls_back`；`tests/recall_config_v2.rs::domain_precedence_is_request_then_configuration_then_default` 增加配置层非 default 搜索反例。

### F2 · Ollama 不要求本地 model_path

- `backend=ollama` 时模型在服务端，本地 `model_path` 可选且不参与可用性判定；未给 `--ollama-url` 或不可达统一为 `degraded / ollama_unreachable`。
- `backend=python` 仍要求本地模型可用（`model_unavailable`），后端依赖缺失为 `backend_unavailable`。
- Rust 降级矩阵与 `recall_embedding.py --probe` 顺序一致。
- 实现：`src/recall/semantic/degrade.rs`、`assets/scripts/recall_embedding.py`。
- 测试：Rust `ollama_does_not_require_a_local_model_cache`、`ollama_unreachable_wins_over_a_missing_local_cache`、`every_missing_capability_degrades_explicitly_and_keeps_lexical`；Python `test_ollama_does_not_require_a_local_model_path`。

### F3 · 模型路径探测有界

- 删除递归 `Path.rglob` 全树统计。
- `--probe` 仅以目录存在性判断可用；诊断计数改为顶层 `os.scandir`，上限 `MODEL_SCAN_MAX_ENTRIES=16`、`MODEL_SCAN_DEADLINE_SECONDS=0.25`、`MODEL_SCAN_MAX_DEPTH=1`，超出即停止并标记 `scan_limited`。
- 实现：`assets/scripts/recall_embedding.py`。
- 测试：`test_model_path_scan_is_bounded_and_non_recursive`（40+40 个嵌套文件，且 `Path.rglob` 被 mock 为一旦调用即失败）。

### F6 · 文档与退出码

- `src/bin/agent-memory.rs` 顶部退出码注释补 `11`（telemetry disabled / event storage unavailable）。
- 本执行器不 commit；提交由 orchestrator 执行。`HYBRID-P0-COMPLETED.md` 中原“未 commit”表述已改为 orchestrator 提交。
- 62/90 实际退出码记录要求已写入 `HYBRID-P0-COMPLETED.md`；当前实际值如下，分发物重建后 orchestrator 必须复跑并记录 0。

## 2. 验证结果

| 检查 | 结果 |
|---|---|
| `cargo test --locked` | **453 passed / 1 ignored / 0 failed**（旧 450 项保持全绿，新增 3 项） |
| `test_recall_onboarding.py` | 23 项通过 |
| `test_recall_embedding.py` | 9 项通过（新增 2 项） |
| Python 合计 | 32 项通过 |
| `check-docs.ps1` | exit 0 |
| `62-verify-agent-memory-offline.ps1` | **exit 0**，28/28 通过（2026-10-29 复核） |
| `90-audit.ps1 -AgentMemoryOnly` | **exit 0**，7/7 通过（2026-10-29 复核） |
| 手工 CLI 合成冒烟 | default search exit 0 / 1 命中；`domain=work` search exit 4 / 0 命中 / `DOMAIN_UNREACHABLE`；`domain=work` read exit 5 / `DOMAIN_UNREACHABLE` |
| `git diff --check` | 通过 |

命令（在工作区 `C:\wt\hybrid-retrieval-20261028`）：

```
cargo test --locked
python assets\scripts\test_recall_onboarding.py
python assets\scripts\test_recall_embedding.py
powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\check-docs.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\62-verify-agent-memory-offline.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File assets\scripts\90-audit.ps1 -AgentMemoryOnly
```

## 3. 分发物与提交（orchestrator）

- 本执行器**不重建** `assets/dist/agent-memory/`，不修改 `build-manifest.json`；62/90 当前因分发物缺失为 exit 1。
- onboarding 默认生成 v2，因此分发物必须包含同一提交的 v2 CLI 后才能发布；orchestrator 重建后必须复跑 `62` 与 `90 -AgentMemoryOnly` 并记录实际退出码 0。
- 本执行器不执行 git commit；提交由 orchestrator 执行。

## 4. 未做与边界

- 未重建 `assets/dist/agent-memory/` 分发物；未修改 `build-manifest.json`。
- 未实现 P1 Archive provider、Archive access 标签接线、P2 事件存储、P3 embedding。
- 未执行 P2–P5；未读取真实会话、知识库或生产 Archive。
- 未执行 git commit；提交由 orchestrator 执行。
