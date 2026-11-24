# BUILD-NOTES — heldout2（会话记忆混合检索 held-out 评估集）

独立评估集构造记录。构造者与被测系统实现无关；仅读取了格式文档
`agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md` 与评估工具 `assets/scripts/recall_eval.py`，
未读取开发集、被测源码或任何真实会话/用户数据。全部语料为合成内容。

## 1. 语料构造思路

7 个文件、7 个互不相同的主题（均取自给定主题清单，未使用被排除主题）：

| 文件 | 主题 | provider | domain | 行数 | 轮次数 |
|---|---|---|---|---|---|
| `corpus/s01_excel.jsonl` | Excel 报表自动化与透视表 | codex | default | 19 | 7 |
| `corpus/s02_git.jsonl` | Git 合并冲突与回滚 | claude | default | 16 | 6 |
| `corpus/s03_proxy.jsonl` | 网络代理与证书错误 | codex | default | 15 | 5 |
| `corpus/s04_python.jsonl` | Python 依赖冲突与虚拟环境（长会话） | claude | default | 46 | 12 |
| `corpus/s05_video.jsonl` | 视频转码与字幕 | codex | default | 15 | 5 |
| `corpus/s06_mail.jsonl` | 邮件附件批量归档 | claude | default | 12 | 5 |
| `corpus/s07_nas_work.jsonl` | NAS 同步与权限 | codex | **work** | 15 | 6 |

- 格式分布：claude 3 个（≥2 ✓）、codex 4 个（≥3 ✓）。
- 长会话：`s04_python.jsonl` 46 行（≥40 ✓），12 个轮次；7 条 long_session 查询的期望轮次全部在
  第 17 行以后（T5–T12），不在会话开头。
- work 域：`s07_nas_work.jsonl` 承载权限反例；"NAS" 一词**只**出现在该文件，供严格 empty 负例
  与语料主题字面零星重合而语义确实缺失（见 §3）。
- 干扰项设计：每个会话内含多个词面相近、问题不同的轮次。例如 s01 中「数据区域不自动扩展 / 字段名
  无效 / 计算字段 #DIV/0! / 周分组错位」四轮都围绕透视表排障；s02 中「revert 后再合并 / rerere 重复
  冲突 / 回滚后 push 被拒」都围绕合并回滚；s04 中 12 轮几乎全是 pip/venv 排障。查询必须指向特定
  轮次，关键词碰巧命中会落到干扰轮次而判错。
- 中英混杂：7 个轮次整体用英文（s01 T7、s02 T5、s03 T5、s04 T8、s05 T4、s06 T4、s07 T5），
  其余中文，查询与答案语言交错，支撑 cross_lingual 类。

轮次/行号锚点表（1-based，锚点行=期望轮次内任一行）：

- s01：T1=2-5 T2=6-7 T3=8-11 T4=12-13 T5=14-15 T6=16-17 T7=18-19
- s02：T1=1-4 T2=5-6 T3=7-10 T4=11-12 T5=13-14 T6=15-16
- s03：T1=2-5 T2=6-9 T3=10-11 T4=12-13 T5=14-15
- s04：T1=1-4 T2=5-8 T3=9-12 T4=13-16 T5=17-20 T6=21-24 T7=25-28 T8=29-32 T9=33-36 T10=37-40 T11=41-44 T12=45-46
- s05：T1=2-5 T2=6-9 T3=10-11 T4=12-13 T5=14-15
- s06：T1=1-4 T2=5-6 T3=7-8 T4=9-10 T5=11-12
- s07：T1=2-3 T2=4-5 T3=6-9 T4=10-11 T5=12-13 T6=14-15

## 2. 查询设计意图与数量

共 **57** 条（≥48 ✓），全部按 §3 标注；正向查询一律 `file`+`line`+`contains`，
仅 q-sy-12、q-sy-13 两条确有「多个不同轮次都可接受」的语义，使用 `expect_any`。

| category | 条数 | 设计意图 |
|---|---|---|
| exact | 9 | 查询携带目标轮次的标志性原词（`数据透视表字段名无效`、`revert -m 1`、`SSL_CERT_FILE`、`mov_text`、`ResolutionImpossible`、`Attachment.Remove`、`sync-conflict` 等），措辞是事后回忆口吻而非复述原句。词面模式应全部命中。 |
| synonym | 13 | 换说法、不复用原文关键词（如「数据范围没法自己长」→T2 结构化引用；「声音画面对不上拍子」→itsoffset；「后悔药」→reflog 轮次）。q-sy-12/q-sy-13 用 `expect_any` 覆盖两个不同轮次（代理配置键两轮 / 体积取舍两轮）。 |
| cross_lingual | 13 | 6 条英文问→中文答轮次（q-cl-01..06），7 条中文问→英文答轮次（q-cl-07..13）。语言对不共享词面，词面模式下失败是正常且预期的。 |
| long_session | 7 | 全部指向 46 行长会话 s04 的 T5–T12（行 17+），测远离会话开头的轮次召回。 |
| model_missing | 5 | 按文档语义：验证模型缺失/降级时词面路径不中断，因此全部为正向查询且措辞含足量词面线索（`non-fast-forward`、`subtitles`、`persistent`、按月份目录、透视表刷新范围）。`--mode none` 下 5/5 通过，符合预期。 |
| permission_negative | 10 | 严格 empty 6 条（q-pn-01..06，仅 `{"empty":true}`）；`no_hits_from_source` 4 条（q-pn-07..10，其中 q-pn-07/08 同时断言 `source_status=unavailable`+`code=DOMAIN_UNREACHABLE`，q-pn-09/10 允许 default 源噪声命中但禁止 archive-work 出现）。 |

负例与语料主题字面零星重合、但语义确实不在语料中的条目（共 5 条，其中严格 empty 2 条 ✓≥2）：

- q-pn-01「NAS 硬盘坏道检测」、q-pn-02「NAS 断电以后靠 UPS 续航」——重合词 "NAS"（work 域主题
  "NAS 同步与权限"），坏道检测/UPS 供电语义不在任何语料；"NAS" 仅存在于 work 语料，默认域不可达。
- q-pn-07「NAS 相册缩略图缓存」、q-pn-10「NAS 共享链接有效期」——同上，且用 `no_hits_from_source`
  断言 work 域数据不得泄漏。
- q-pn-09「邮件怎么设置自动转发」——重合词 "邮件"（s06 主题），自动转发语义不在语料；允许 default
  源词面噪声命中，只断言 archive-work 不得出现。

## 3. 校验命令与结果

仅运行词面模式（未运行语义模式）：

```
python C:\wt\hybrid-retrieval-20261028\assets\scripts\recall_eval.py --set <本目录> --mode none --binary C:\wt\hybrid-retrieval-20261028\agent-memory\target\debug\agent-memory.exe --out <本目录>\lexical-check.json
```

- 退出码：**0**（复跑取证时附加 `echo $?` 确认；报告写入 `lexical-check.json`）。
- 词面结果（仅作标注自洽证据，不作为系统结论）：查询 57、正向 47、负例 10；
  recall@5=37/47=0.787234，first_hit=32/47=0.680851；readback 276/276 全部通过；
  分类 recall@5：exact 9/9、synonym 12/13、cross_lingual 4/13、long_session 7/7、
  model_missing 5/5、negative_pass 5/10。

## 4. 词面校验发现并修正的标注错误清单

**无。** 判据：

- 全部正向查询的 `expected_turns` 均解析为有效轮次号（报告中无 null 锚点），
  即所有 `file`/`line`/`contains` 自洽：行号未越界、文件名正确、contains 确在锚点轮次内。
- exact 9/9 在词面下全部命中（含 1 条 work 域、3 条英文轮次锚点），无「exact 类查询词面全失败」
  的标注错误信号。
- 负例的 `source_id`/`source_status`/`code` 断言与多域语义一致：default 查询下
  `archive-work` 报 `unavailable`+`DOMAIN_UNREACHABLE`，q-pn-07/08 的状态断言通过。

## 5. 词面校验观察（未修改任何查询，仅记录）

按「不得为提高得分而删除、改写或弱化难查询」的约束，以下现象**保留原样**，留待语义/验收运行判断：

1. 严格 empty 负例 q-pn-01..05 在词面模式下收到高频词噪声命中（如「怎么/到/台」等 token 命中
   s06/s05/s04 的无关轮次），`negative_pass=false`；q-pn-06（低频词面）返回 0 条通过。
   这是检索精确度信号而非标注错误：查询语义确实不在语料中，期望空结果的标注成立。
   [UNCERTAIN] 该噪声是否算被测系统缺陷取决于其评分阈值设计；本集不据此调整查询措辞。
2. q-sy-02（同义改写）与 9 条 cross_lingual 查询在词面下未命中——换说法/跨语言在纯词面下
   失败是正常且预期的，正是这两类查询的考察点。
3. `memory_search` 的非零退出码（无结果 4、有结果 10）属于正常结果信封（含 `contract_version`），
   与工具 §7 的负例判定一致，不视为失败。

## 6. 文件清单

- `manifest.json` — schema `agent-memory/hybrid-eval-set@1`，embedding 规格 qwen3-embedding:4b/256/l2/ollama
- `queries.jsonl` — 57 条查询与标注
- `corpus/s01_excel.jsonl` … `corpus/s07_nas_work.jsonl` — 7 个合成会话（s07 为 work 域）
- `lexical-check.json` — 词面校验报告（工具产出）
- `BUILD-NOTES.md` — 本文件
