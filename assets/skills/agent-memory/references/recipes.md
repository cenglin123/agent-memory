# 常用路径

所有示例都走同一入口：请求 JSON 写入临时文件，`--request @<file>`。字段与上限见 `requests.md`。

示例里的占位符：`$SESSION_ID` 换成上一步结果里的 `session_id`，`$REF` 换成命中里的整个 `ref` 对象（不是字符串）。

## 接上另一个窗口（刚才的对话）

第 1 步，列最近交互会话，只看标识行（`limit` 3、`tail_turns` 0）：

```json am-example:memory_recent
{
  "limit": 3,
  "tail_turns": 0
}
```

第 2 步，用每条会话的 `last_activity_local` 与用户确认目标会话；不要凭「最近一条」直接认定。

第 3 步，按确认的 `session_id` 读它的尾部（`$SESSION_ID` 换成真实值）：

```json am-example:memory_recent
{
  "session_id": "$SESSION_ID",
  "tail_turns": 6
}
```

`tail` 为空或太短：把 `tail_turns` 加到 12–20、仍带 `session_id` 再取一次（OpenCode 尾部只回看最近约 200 条记录，加大 `tail_turns` 无效时直接走下一步）；还不够就用该 `session_id` 检索（见下节）。不要自己去读宿主的原始会话文件或数据库（理由与替代见 `SKILL.md`「结果使用纪律」）。

第 4 步，某一轮要全文时，把它 tail 里的 `ref` 交给 `memory_read`：

```json am-example:memory_read
{
  "ref": "$REF",
  "context_before": 1,
  "context_after": 1
}
```

`context_before` / `context_after` 按记录条数扩上下文，活源与归档都支持；`"context": "turn"`（整轮）只对 `source_kind` 为 `archive` 的 ref 有效。

## 按会话查原话

会话 id 已知时，把它固定进请求：

```json am-example:memory_search
{
  "query": "needle",
  "mode": "exact",
  "session_id": "$SESSION_ID"
}
```

- `exact` 需要该源有可读索引。若 `source_status` 报 degraded（`detail.reason` 如 `INDEX_MISSING` / `INDEX_REQUIRED`），改用默认模式（去掉 `mode`）再传 `session_id`；`session_id` 过滤在所有模式下都生效。
- 命中只是片段，要原文用 `memory_read` 读它的 `ref`。

## 按时间查最近讨论

`since` / `until` 是 RFC 3339；与 `order` 一起用会按 exact 路径执行（需要索引）：

```json am-example:memory_search
{
  "query": "needle",
  "since": "2026-01-01T00:00:00Z",
  "order": "newest",
  "limit": 10
}
```

只想知道「最近发生过什么」、不关心关键词时，直接用 `memory_recent`（实时，无索引延迟）。

## 空结果排查

按顺序看，不要直接回答「没有」：

1. `errors[]` 非空：先处理错误码。`REF_*` 是回读问题，`INVALID_REQUEST` 是字段问题。
2. 看 `source_status[].detail.coverage`：`stale_since` 表示索引落后于活源；`total_sessions` / `session_filter` 表示会话过滤是否命中。
3. 索引滞后：改用 `memory_recent` 取实时尾部，或按会话 id 用默认模式检索。
4. `session_id` 可能写错：用 `memory_recent` 拿真实 id，不要猜。
5. 源状态是 `unavailable` / `unsupported` / `unconfigured`：看同一条的 `code` 与 `detail`（例如 `SOURCE_NOT_CONFIGURED` 会列出已启用的会话源 id），这是配置问题，不是「没记过」。
6. 换个说法再查一次：概念类查询用默认模式，标识符类用 `mode: "exact"`。
