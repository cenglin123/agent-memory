# 否决与暂缓

记忆的可信度不只在「记住了什么」，还在「哪些结论已经不能再当依据」。资格层解决三件事：

- **否决 `rejected`**：结论已被反例推翻。它是负记忆，防止同一个错误结论换个说法、跨会话再次成为前提。否决必须带依据（`basis`）与具体反例（`counterexample`），反例能回读时再给 `counterexample_ref`。
- **暂缓 `deferred`**：现在判断不了，但也不能当已证实。必须指定 `owner`（谁负责重审）与 `trigger`（出现什么新证据或事件时重审），否则会永远悬空。
- **撤销 `cleared`**：新证据推翻了原来的否决或暂缓，带 `basis` 把资格撤回 `open`。撤销不是删除历史：每一次否决、暂缓、撤销都只追加，读的时候看最新一行。

任何带 `<host>:<session>` actor 的 agent 都可以显式写入；检索结果、模型输出不会自动触发否决。写入只通过下面的命令，不手工改库。

## 查已否决（只读）

CLI（分页、文本与 JSON 两种输出，`--query` 对候选 claim 与资格 basis / counterexample 做归一化子串匹配）：

```json am-example:learning
["qualified", "--root", "$ROOT", "--json"]
```

```json am-example:learning
["qualified", "--root", "$ROOT", "--state", "rejected", "--query", "显式根", "--json"]
```

MCP 宿主使用同名只读工具，字段与 CLI 一致；学习库根固定为 `recall.json` 同目录下的 `learning`，不需要也不接受根路径参数：

```json
{"state": "rejected", "query": "显式根", "limit": 20}
```

`state` 取 `rejected` / `deferred` / `cleared`，`limit` 1–200（默认 50）。

## 写入资格

命令形状：`learning qualify --root <learning root> --candidate-id <id> --state <state> --actor <host>:<session>`，按状态补必填项。

| 状态 | 必填 | 用途 |
|---|---|---|
| `rejected` | `--basis` + `--counterexample` | 结论被推翻，不能再作前提 |
| `deferred` | `--owner` + `--trigger` | 暂时判不了，指定谁在什么条件下重审 |
| `cleared` | `--basis` | 新证据出现，撤回前一资格回到 `open` |

可选 `--counterexample-ref archive-record:<64hex>`：只在你已回读到真实反例原文、拿到它的归档记录时才给，不要编造；只给 ref 时只做形状校验；同时给 `--archive <绝对 archive 根> --domain <域>` 时会真实回读并校验指纹，结果的 `ref_verified` 为 `true`。同一候选最新一行已是同 state、同 basis 时不追加（`duplicate: true`）。

否决一条结论（反例是具体的那次失败，不是「感觉不对」）：

```json am-example:learning
["qualify", "--root", "$ROOT", "--candidate-id", "$KNOWLEDGE_ID", "--state", "rejected", "--actor", "opencode:skill-guard", "--basis", "显式根缺失时无法回读证据", "--counterexample", "省略根后命令返回 LEARNING_ROOT_INVALID"]
```

暂缓（还不能判，但把重审条件写清楚）：

```json am-example:learning
["qualify", "--root", "$ROOT", "--candidate-id", "$KNOWLEDGE_ID", "--state", "deferred", "--actor", "opencode:skill-guard", "--owner", "user", "--trigger", "出现新的可回读证据时重审"]
```

撤销（带新依据；撤销者与被撤销的否决行 actor 不同会在结果与 90 审计里带出冲突信号）：

```json am-example:learning
["qualify", "--root", "$ROOT", "--candidate-id", "$KNOWLEDGE_ID", "--state", "cleared", "--actor", "opencode:skill-guard", "--basis", "新证据已回读并推翻原反例"]
```

```json am-example:learning
["qualified", "--root", "$ROOT", "--state", "cleared", "--json"]
```

## 纪律

- 否决是判断，不是流程：先 `memory_read` 回读原文与反例，再写 `rejected`；不要把检索片段当证据。
- 只写自己确实需要的资格行；每个 actor 与时间都留痕，90 `AM-QUAL` 会检查缺依据的否决、缺 owner/trigger 的暂缓、超期未重审与短时间反复翻转。
- 已否决的结论不得再作为前提；确实要重新引入，先 `cleared` 并说明新依据，或走治理流程。
