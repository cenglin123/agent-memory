# 统一混合检索 P0 合同基线完成记录

状态：P0 完成（`CONFIG_VERSION = 2`、`CONTRACT_VERSION = 2`）。
真实数据：**否**。本阶段只使用合成 fixture 与临时目录，未读取真实会话、知识库或 Archive，未执行采集，未实现 P1–P5。

## 1. 实现范围

### Rust 合同与配置

- `agent-memory/src/recall/config.rs`：v2 顶层键 `version/sources/source_order/interpreter/domain/telemetry`；v1 仅内存归一（`domain=default`、无 archive、无 telemetry、不回写）；archive 仅 v2；`interpreter.embedding` 可选。
- `agent-memory/src/recall/domain.rs`：`request > configuration > default`；Archive 可读谓词（access 非空且生效 domain ∈ labels）；空标签 fail-closed；非 default 标签在隐式 default 下返回 `DOMAIN_UNREACHABLE`。
- `agent-memory/src/recall/contract.rs`：合同版本升 2；新增 `DOMAIN_DENIED`、`DOMAIN_UNREACHABLE`、`ARCHIVE_PROVIDER_UNAVAILABLE`；错误码表 27 项。
- `agent-memory/src/recall/search.rs` / `read.rs`：请求接受 `domain`；v2 archive 源在 P0 明确返回 `ARCHIVE_PROVIDER_UNAVAILABLE`（`unavailable`/`unsupported`），不是健康空结果。
- `agent-memory/src/recall/telemetry.rs`：telemetry 配置、四类事件 schema、事件命令退出码；P0 不写存储。
- `agent-memory/src/recall/semantic/`：模型身份（id/维度/归一化/后端）、生成键、降级矩阵、冻结指标。
- `agent-memory/src/mcp.rs`：两个工具仍恒只读，`readOnlyHint: true` 不变；schema 增加 `domain`。
- `agent-memory/src/bin/agent-memory.rs`：新增 `events record|prune` 骨架。

### Python 与运维

- `assets/scripts/recall_onboarding.py`：生成 v2；v1 默认逐字保留；`--migrate-v1` 显式迁移。
- `assets/scripts/recall_embedding.py`：离线能力探测；Python/模型/Ollama 不可用时明确降级，词面继续；`--embed` 在 P0 退出 3。
- `assets/scripts/64-install-recall.ps1`：新增 `-MigrateV1`。
- `agent-memory/docs/verification/HYBRID-P0-METRICS.md`：指标冻结文档。

## 2. 配置迁移矩阵

| 输入 | 解析行为 | 写回 |
|---|---|---|
| v1 合法配置 | 内存归一：`domain=default`、无 telemetry、无 archive；只允许 kb/device/opencode/codex | 不回写 |
| v1 + `domain`/`telemetry`/`interpreter.embedding` | `INVALID_REQUEST`（unknown_key） | 不回写 |
| v1 + archive 源 | `sources[i].kind` / `archive_requires_v2` | 不回写 |
| v2 合法配置 | 原样加载；`domain` 必填；telemetry/embedding 可选 | 由 onboarding 生成或显式迁移 |
| v2 + 未知顶层键 | `INVALID_REQUEST`（unknown_key） | 不写 |
| v2 + archive 源 | 配置接受；检索/回读显式 `ARCHIVE_PROVIDER_UNAVAILABLE` | 不写 |
| onboarding 遇到 v1 | 默认逐字保留；`--migrate-v1` 才生成 v2 | 仅显式迁移 |

domain 优先级：请求 `domain` > 配置 `domain` > `default`。配置层 domain 参与 Archive 可读判定；非 default 标签在缺省 domain 下不可达。

## 3. 事件与只读边界

- MCP `memory_search` / `memory_read` 恒只读，`readOnlyHint` 与工具数量（2）不变；合成测试确认 MCP 调用前后不创建 telemetry 路径或其目录。
- 事件只能来自独立 CLI `agent-memory events record|prune` 或宿主 wrapper；P0 只冻结 schema 与退出码。
- 退出码：`0` 成功（P2 才有）、`2` 用法/事件无效、`11` telemetry 关闭或存储未实现。
- `events record` 在 P0 对合法事件也返回非零（`EVENTS_STORAGE_NOT_IMPLEMENTED`、`wrote=false`），不写任何文件；`events prune` 同样非零且不删除。
- 事件 schema：`kind/hit_id/ts` 必填；`applied` 需 `task_id` + `evidence`；`confirmed` 需 `actor` + `evidence`；`query` 必须同时带 `expires_at`，无 query 时禁止 `expires_at`。

## 4. 模型与降级矩阵

`interpreter.embedding` 冻结 `model_id`、`dimension`、`normalization`（`l2|none`）、`backend`（`python|ollama`）与可选显式 `model_path`。生成键：

```
embed-v1;model=<id>;dim=<n>;norm=<l2|none>;backend=<python|ollama>
```

| 情况 | 状态 | 词面检索 |
|---|---|---|
| 未配置 embedding | `disabled` / `embedding_not_configured` | 继续 |
| Python 不可用 | `degraded` / `python_unavailable` | 继续 |
| 模型缓存不可用 | `degraded` / `model_unavailable` | 继续 |
| Ollama 不可达/未给 URL | `degraded` / `ollama_unreachable` | 继续 |
| Python 后端依赖缺失 | `degraded` / `backend_unavailable` | 继续 |
| 全部可用 | `available` | 继续 |

P0 不新增 Rust ML crate，不引入 Ollama 强依赖；`recall_embedding.py --probe` 只做本地探测，`--embed` 明确未实现。

### 交付前修复（F1/F2/F3/F6）

- **F1**：P0 无 Archive provider 时，显式非 default 生效 domain 的 `memory_search` 对每个源返回 `unavailable` + `DOMAIN_UNREACHABLE`（exit 4，不返回任何 default 域命中）；`memory_read` 返回 `DOMAIN_UNREACHABLE`（exit 5）。default 行为不变。CLI 与 MCP 均有反例，P1 再接 Archive 标签。
- **F2**：`backend=ollama` 不再要求本地 `model_path`；未给 URL 或不可达统一 `ollama_unreachable`。Rust 降级矩阵与 `recall_embedding.py` 顺序一致。
- **F3**：`--probe` 的模型路径检查改为有界顶层扫描（最多 16 项、0.25 s、深度 1），不再使用递归 `rglob`；只以目录存在性判定可用。
- **F6**：CLI 顶部退出码注释补 `11`；本执行器不 commit，提交由 orchestrator 执行；62/90 的实际退出码按下方表格记录，分发物重建后必须为 0。

## 5. 测试与验收

- `cargo test --locked`：**453 passed / 1 ignored**（P0 前基线 413 passed / 1 ignored；旧测试无回归，新增 40）。
  - 新增 `tests/recall_config_v2.rs`：8 项（v1/v2 键集合、未知键、domain 优先级、空标签、非 default 不可达、archive 仅 v2、onboarding 生成→v2 加载→合成搜索、v1 保留/显式迁移）。
  - 新增 `tests/recall_telemetry.rs`：3 项（MCP 无事件写入、events record 非零且不写、events prune 非零且不删）。
  - `tests/recall_cli_mcp.rs` 新增 1 项：显式非 default domain 的 CLI/MCP search+read 反例。
  - 新增单元测试：domain 5、telemetry 7、semantic model 2 / degrade 6 / metrics 3；配置 v1/v2 迁移与 telemetry/embedding 校验。
- Python 合成测试：`test_recall_onboarding.py` **23 项**、`test_recall_embedding.py` **9 项**，共 32 项全部通过。
- 机械链路：`recall_onboarding.py install`（v2 生成）→ `load_config` → `memory_search` 合成 KB 冒烟，退出码 0；v1 保留字节级一致，`--migrate-v1` 后为 v2。
- 指标冻结：recall@5 ≥ 0.80、首命中轮次正确率 ≥ 0.90、回读 100%、p95 ≤ 20 s、索引 ≤ 3× 源文本（允许量化）；文档读回测试通过。

### 62/90 与文档检查退出码

| 命令 | 当前实际 | 要求 |
|---|---|---|
| `assets/scripts/check-docs.ps1` | 0 | 0 |
| `assets/scripts/62-verify-agent-memory-offline.ps1` | **0（28/28）** | 2026-10-29 复核通过 |
| `assets/scripts/90-audit.ps1 -AgentMemoryOnly` | **0（7/7）** | 2026-10-29 复核通过 |

分发物已由 orchestrator 重建并安装：SHA256 `9ac3acb892398052a7d704011e36c3f3f132a55f4a5952b5722495e928387408`，manifest `source.commit=0cf92448ff03cd603a3e542916d40abd2411f14c`。

## 6. P1 待办（本阶段未实现）

1. `SourceKind::Archive` provider：Archive 根、`archive/objects/<aa>/<bb>/<sha256>`、`record_sha256` + 投影偏移、解压后 `range_sha256`。
2. `derived/generations/g<N>/retrieval/` artifact 与 parser/schema/model 代际；同内容两次采集 `hit_id` 一致。
3. Archive session 派生映射；未知映射 `unmapped`，不猜测。
4. `.jsonl.zst` 解压后回读基准与 Archive 缺失/代际不匹配/域拒绝的完整状态码。
5. 将 `classify_access` 接入 Archive 命中与回读路径（P0 仅冻结谓词）。
6. P2 事件存储、幂等、导出与有界保留；P3 embedding 执行与融合。

## 7. 未做与边界

- 未实现 P1 Archive provider，未实现 embedding，未新增 Rust ML/Ollama 依赖。
- 未执行 P2–P5，未写入真实事件存储，未执行 Archive 采集。
- 未读取任何真实会话、知识库或生产 Archive；全部测试为临时目录合成 fixture。
- 本执行器不执行 git commit；提交由 orchestrator 执行。
