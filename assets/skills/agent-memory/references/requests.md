# 请求字段

三个回忆操作在 CLI 与 MCP 上共用同一份解析与同一份请求合同；第四个只读工具 `memory_qualified` 读取学习库的资格层，字段见文末：

- 字段与上限的来源是 `agent-memory/src/recall/search.rs`、`read.rs`、`recent.rs` 的请求常量（`SEARCH_KEYS` / `READ_KEYS` / `RECENT_REQUEST_KEYS`），MCP `tools/list` 的 `inputSchema.properties` 与此一一对应（`agent-memory/src/mcp.rs`）。
- 未知字段一律拒绝（`INVALID_REQUEST`，exit 2），不会静默忽略。
- 每个 `<!-- am-fields: <op> -->` 标记之后的表格，首列就是该操作接受的顶层字段；一致性守卫逐操作比对，增删字段必须同步改这里。

## memory_search

```json
{"query": "needle"}
```

<!-- am-fields: memory_search -->
| 字段 | 类型 | 默认 | 取值 / 上限 |
|---|---|---|---|
| `query` | string | 必填 | 非空 |
| `sources` | string[] | 全部启用源 | 非空、不重复；取 recall.json 的 `source_id` |
| `limit` | integer | 10 | 1–100 |
| `offset` | integer | 0 | 0–10000（越界是 `INVALID_OFFSET`） |
| `snippet_bytes` | integer | 400 | 1–8192 |
| `budget` | object | 见下表 | 每源预算，见 `am-fields: budget` |
| `session_id` | string | 不过滤 | 1–200 字符，精确匹配 |
| `domain` | string | 配置值，其次 `default` | 1–64 字符，影响 Archive 读取 |
| `related_hops` | integer | 关闭 | 1–2（仅 Archive 命中做邻接扩展） |
| `mode` | string | `auto` | `auto` / `exact` / `hybrid` / `legacy` |
| `since` | string | 不限 | RFC 3339 |
| `until` | string | 不限 | RFC 3339 |
| `roles` | string[] | 不限 | `user` / `assistant` / `tool` |
| `kinds` | string[] | 不限 | `text` / `tool_input` / `tool_output` |
| `session_kinds` | string[] | 不限 | `interactive` / `worker` / `unknown` |
| `order` | string | `newest` | `newest` / `oldest` / `relevance` |
| `group_by_session` | boolean | `false` | 每个会话只留代表命中 |

`mode` 的作用路径：

- `auto`（默认）：源有可读索引走 hybrid，否则退回旧扫描。
- `exact`：只走活源全文索引，词项 AND 组合；查标识符、错误码、路径、版本号、参数、会话 id 用它。
- `hybrid`：FTS5 bm25 与本地会话向量按排名融合、按会话去重；不接受 `order` / `group_by_session`。
- `legacy`：强制旧扫描；`since` / `until` / `roles` / `kinds` / `session_kinds` / `order` / `group_by_session` 会被拒绝。

`since`、`until`、`roles`、`kinds`、`session_kinds` 只在索引路径生效；`order` 与 `group_by_session` 会让默认模式按 exact 执行。带这些过滤或排序而源没有索引时报 `INDEX_REQUIRED`（degraded），不是静默忽略。

<!-- am-fields: budget -->
| 字段 | 类型 | 默认 | 取值 / 上限 |
|---|---|---|---|
| `max_bytes_per_source` | integer | 33554432 | 8192–1073741824 |
| `max_elapsed_ms_per_source` | integer | 20000 | 4000–600000 |
| `kb_top_k` | integer | 20 | 1–50 |

## memory_read

请求里必须有 `ref`；最省事的做法是把 `memory_search` / `memory_recent` 命中里的 `ref` 对象原样放进来。

<!-- am-fields: memory_read -->
| 字段 | 类型 | 默认 | 取值 / 上限 |
|---|---|---|---|
| `ref` | object | 必填 | 八个键，见下 |
| `context_before` | integer | 0 | 0–1000（按记录计的上下文条数） |
| `context_after` | integer | 0 | 0–1000 |
| `max_bytes` | integer | 8192 | 1–1048576，超出即 `truncated` |
| `domain` | string | 配置值，其次 `default` | 1–64 字符 |
| `context` | string | 窗口模式 | 唯一取值 `turn`（整轮，仅 `source_kind: "archive"` 的 ref），不得与 `context_before` / `context_after` 同用 |
| `session_id` | string | 不限 | 非空、最多 200 字节；仅 Device 回读用它做本机复用去重计数，不改变返回的字节 |

`ref` 的固定八个键：`source_id`、`source_kind`、`root_sha256`、`rel_path`、`schema_gen`、`locator`、`fingerprint`、`text_range`。不要手写或改写这些值：指纹与区间由检索端给出，回读时按字节校验，失配是 `REF_*` 错误（exit 5），不是空结果。

## memory_recent

全部字段可选，省略整个请求体即默认值。

<!-- am-fields: memory_recent -->
| 字段 | 类型 | 默认 | 取值 / 上限 |
|---|---|---|---|
| `sources` | string[] | 全部启用源 | 非空、不重复；取 `source_id`（默认安装 `kb` / `device` / `claude_code` / `opencode` / `codex` / `dsh`） |
| `session_id` | string | 不限 | 非空；只返回这一个会话（不受 `session_kinds` 限制，不论新旧都能取到），配 `tail_turns` 读它的尾部 |
| `limit` | integer | 5 | 1–20 |
| `session_kinds` | string[] | `["interactive"]` | `interactive` / `worker` / `unknown` |
| `exclude_session_ids` | string[] | 空 | 非空、不重复 |
| `since` | string | 不限 | RFC 3339；按各宿主会话时钟过滤 |
| `tail_turns` | integer | 0 | 0–20，0 表示不读尾部 |
| `tail_roles` | string[] | `["user","assistant"]` | `user` / `assistant` / `tool` |
| `max_chars_per_turn` | integer | 800 | 1–8000 |
| `max_bytes` | integer | 65536 | 1–4194304（只计尾部 `text`，信封可超出） |

响应里 `last_activity_at` 是 UTC（`Z`），`last_activity_local` 是同一时刻的本地时间；对用户说时间用后者。

## memory_qualified

只读列出当前带资格的候选（最新一行）；学习库根固定为 `recall.json` 同目录下的 `learning`，没有根路径字段。全部字段可选。

<!-- am-fields: memory_qualified -->
| 字段 | 类型 | 默认 | 取值 / 上限 |
|---|---|---|---|
| `state` | string | 不限 | `rejected` / `deferred` / `cleared` |
| `query` | string | 不限 | 非空；对候选 claim 与资格 basis / counterexample 做归一化子串匹配 |
| `limit` | integer | 50 | 1–200 |

响应形如 `{ok, root, state?, query?, total, count, qualified[]}`；每条带 `candidate_id`、`claim`、`target_kind`、`state`、`basis`、`counterexample`、`owner`、`trigger`、`actor`、`recorded_at` 与 `history_count`。学习库缺失时 `isError: true`，错误码 `LEARNING_ROOT_MISSING` / `LEARNING_DB_MISSING`，不会去猜别的路径。

## 常用组合

| 目标 | 请求 |
|---|---|
| 查标识符或错误码 | `{"query": "INDEX_REQUIRED", "mode": "exact"}` |
| 概念、换了说法 | `{"query": "接上另一个窗口的做法", "limit": 10}` |
| 限定一个会话 | `{"query": "部署脚本", "session_id": "<会话 id>"}` |
| 只看用户发言 | `{"query": "预算", "roles": ["user"]}` |
| 按时间窗 | `{"query": "回滚", "since": "2026-10-01T00:00:00Z", "order": "newest"}` |
| 每会话一条 | `{"query": "hook", "group_by_session": true}` |
| 最近会话概览 | `{"limit": 5, "tail_turns": 0}` |
| 读某个会话的尾部 | `{"session_id": "<会话 id>", "tail_turns": 6}` |
| 读某段全文 | `{"ref": <命中里的 ref>, "context_before": 2, "context_after": 2}` |
| 多读一点上下文 | `{"ref": <归档 ref>, "context": "turn"}`（整轮，仅 archive 源） |
