# BUILD-NOTES — heldout3-mixed-office-2026-10

独立构造的 held-out 评估集，用于验收「会话记忆混合检索（词面 + 向量）」系统。
构造者只读取了格式规范 `agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md` 与评估工具
`assets/scripts/recall_eval.py`（后者仅用于运行与了解输入契约），未接触开发集、被测源码或任何真实会话数据。
全部语料为合成内容，人名/路径均为虚构（`C:/synthetic/...`）。

## 1. 语料构造思路

8 个文件、8 个互不相同的主题，全部取自指定主题清单（此前未用过的）：

| 文件 | provider | domain | 主题 | 行数 | 轮次设计 |
|---|---|---|---|---|---|
| s01.jsonl | codex | default | 浏览器插件开发与权限声明 | 15 | 6 轮；T1/T2/T4 三轮都谈 host_permissions/permissions，是彼此相近的干扰轮 |
| s02.jsonl | codex | default | Docker 镜像瘦身与多阶段构建 | 13 | 5 轮；T1/T2/T5 均围绕“变小”，T4/T5 一个英文问英文答、一个英文问中文答 |
| s03.jsonl | claude | default | 打印机与扫描仪驱动排障 | 12 | 5 轮；打印类 2 轮 + 扫描类 3 轮互相干扰；含 tool_use 与 tool_result-only user 记录（不开启新轮次） |
| s04.jsonl | claude | **work** | 公司年报 PDF 表格抽取 | 8 | 4 轮；work 域专用，供权限反例与 work 域正例 |
| s05.jsonl | codex | default | 家用路由器端口转发与 DDNS | 13 | 5 轮；T1/T2 都是“配了不通”，T3/T4 都是 DDNS 问题 |
| s06.jsonl | codex | default | Markdown 静态站点部署 | 11 | 5 轮；T2 与 T4 都是 404/资源丢失，构成强干扰对 |
| s07.jsonl | codex | default | SQL 慢查询与索引优化 | **49（长会话）** | 18 轮；索引/执行计划类多轮互扰；目标轮次分布在 14–33 行（不在开头） |
| s08.jsonl | claude | default | 键盘快捷键与输入法冲突 | 8 | 4 轮；T1/T2/T3 均为“快捷键被抢/被挡”近义问题 |

- 格式配比：codex 5 个（s01/s02/s05/s06/s07）≥ 3 ✓，claude 3 个（s03/s04/s08）≥ 2 ✓。
- 长会话：s07 共 49 行 ≥ 40 ✓；被标注的目标轮次在 14–33 行（T5–T12），不在会话开头 ✓。
- work 域：s04 单独放 `domain: "work"`，其余为 `default`，形成被拒域/不可达域反例。
- 中英混杂：每文件至少一轮英文问答或英文问中文答（s02 T5、s05 T5、s06 T5、s08 T3 为英文问中文答；s01 T6、s02 T4、s03 T3、s04 T3、s05 T4、s06 T3 为中文问英文答或英文原生轮）。
- 干扰项：每个会话都埋了 2–3 个相近但结论不同的轮次（例如 s06 的两轮 404、s07 的多轮索引问题、s01 的三轮权限问题），检索不能只靠关键词碰巧命中；关键词相同的轮次分布在不同行号/轮次，标注精确指向其中唯一正确轮。

## 2. 查询设计意图（共 57 条）

| 类别 | 数量 | 设计意图 |
|---|---|---|
| exact | 10 | 复述式回忆提问，但保留目标轮的字面关键词（如 0x0000011b、CGNAT、gh-pages、keyset）。词面模式下应当命中，是标注自洽性的探针。 |
| synonym | 14 | 换说法提问，不复用目标轮关键词（例：问“运行产物和编译环境怎么分开”，答案轮说的是“多阶段构建”）。锚点 contains 仍取目标轮原文，仅作标注校验。词面下失败是预期的。 |
| cross_lingual | 12 | 中文提问→英文回答轮 6 条（q025/026/028/029/030/033），英文提问→中文回答轮 6 条（q027/031/032/034/035/036），跨语言换说法。词面下失败是预期的。 |
| long_session | 7 | 全部指向长会话 s07 的中后段轮次（第 14–33 行），验证长会话中远端轮次的召回；措辞均为事后回忆型（“后来怎么改的”“当时用的什么办法”）。 |
| model_missing | 4 | 按文档 §5 语义：验证“模型缺失时词面路径不中断”。因此设计为带高辨识度字面锚（chrome.storage.session、dpkg --path-exclude、Print Spooler SPL、pt-online-schema-change）的正例，语义与词面模式都必须命中。[UNCERTAIN] 文档未给 category 本身的判定规则，此处按“--mode none 下仍应通过”的语义构造。 |
| permission_negative | 10 | 见下。 |

permission_negative 明细：

- 严格 `empty` 6 条（≥5 ✓）：q048–q053。主题完全不在语料中（助记词备份、请柬排版、戚风蛋糕、邮件转发规则、书籍索引排版、USB 端口硬件维修）。
  - 其中 3 条与语料主题仅有零星字面重合（≥2 ✓）：q051 共享“转发”（对照 s05 端口转发）、q052 共享“索引”（对照 s07 索引优化）、q053 共享“端口”（对照 s05 端口映射）；语义上确实不在语料中。
- `no_hits_from_source` 4 条（≥3 ✓）：q054–q057。查询文本就是 work 域主题（年报表格抽取/勾稽/科目映射），在 default 域提问：允许 default 域噪声命中，但 `archive-work` 的任何命中都算泄密失败。
- 两类负例都按 §3.2“同一配置里还有被拒域来源导致非零退出，必须同时声明期望的 source_status/code”声明了
  `source_id=archive-work, source_status=unavailable, code=DOMAIN_UNREACHABLE`（§1 规定的 default 域下 work 源表现）。
  词面校验中 q054–q057 全部通过（含状态断言），证明该断言与二进制实际行为一致。

## 3. 校验（仅词面模式）

命令（按任务给定原样运行）：

```
python C:\wt\hybrid-retrieval-20261028\assets\scripts\recall_eval.py --set <heldout3 目录> --mode none --binary C:\wt\hybrid-retrieval-20261028\agent-memory\target\debug\agent-memory.exe --out <heldout3 目录>\lexical-check.json
```

- **退出码：0**（报告已产出）。未运行语义模式，未做任何调参。
- 报告：`lexical-check.json`（57 查询；expectations 47 / negatives 10；stages 全 0）。
- 轮次口径（词面）：recall@5 = 37/47（0.787），first_hit = 32/47（0.681）。
- 分类词面结果：exact 9/10 命中、model_missing 4/4、long_session 7/7、synonym 11/14、cross_lingual 6/12、
  permission_negative 4/10 通过。

## 4. 词面校验中发现并修正的标注错误清单

**0 条。无修正。** 校验确认：

- 57 条查询的 `expected_turns` 全部解析成功（无 null）：无行号越界、无文件名错误、`contains` 全部出现在锚点轮次内。
- exact 10 条中 9 条词面命中；未命中的 q001 逐项复核后确认**不是标注错误**：锚点轮（s01 轮 1，line 3，contains activeTab）解析正确，
  词面 top-5 被刻意设置的近义干扰轮（s01 轮 2 / 轮 4，同样含 host_permissions/permissions）挤出。属于排序竞争，不是标注问题；
  按任务禁令（不得为提高得分而改写难查询）保留原样。
- synonym / cross_lingual 词面未命中（q016/q019/q021/q025/q028/q030/q032/q033/q036）属预期，未做任何削弱或改写。
- 严格 empty 负例 q048–q053 词面下失败是 §3.2 明示的 P1 词面行为（偶然二元组命中，如“索引”“端口”“转发”），应在语义模式评估；
  已确认其全部误命中来自 `archive-default`，无 `archive-work` 泄露。保留原样。
- 全部 57 条查询的命中/误命中中，`archive-work` 仅出现在 4 条 `domain:"work"` 正例（q018/q019/q029/q036）里，权限边界符合 §1 语义。

## 5. 其他说明

- `manifest.embedding` 按示例规格声明（qwen3-embedding:4b / 512 / l2 / ollama）；本次只跑 `--mode none`，embedding 块未参与。
- [UNCERTAIN] 严格 empty 负例是否应携带 `source_status` 断言：§3.2 有“必须同时声明”的表述，故全部携带；若验收口径认为纯 `empty` 更干净，删掉断言字段即可，不影响 empty 判定主体（`passed = not hits`）。
- 语料在评估期间保持不变；`lexical-check.json` 为本目录内唯一生成物，其余为手写构造文件。
