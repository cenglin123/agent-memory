# BUILD-NOTES — heldout-hybrid-2026-10

独立构造的 held-out 评估集，用于验收「会话记忆混合检索（词面 + 向量）」系统。
构造者与被评估系统实现无关：全程只读取格式文档
`agent-memory/docs/verification/HYBRID-EVAL-FORMAT.md`（唯一规范）与评估工具
`assets/scripts/recall_eval.py`（仅运行）。未读取实现侧源码、tests、其他
verification 文档，也未接触任何真实会话或用户数据。语料全部为合成内容。

## 1. 构造思路

### 1.1 语料（7 个文件，132 行，23 564 字节）

| 文件 | provider | domain | 行数 | 轮次 | 主题 |
|---|---|---|---|---|---|
| corpus/s01.jsonl | codex | default | 13 | 5 | 数据库备份与恢复演练（pg_dump / collation / WAL 归档） |
| corpus/s02.jsonl | claude | default | 12 | 5 | 前端打包体积优化（moment→dayjs / publicPath / 代码分割） |
| corpus/s03.jsonl | codex | default | 11 | 4 | 服务器日志轮转（logrotate / journald / 采集端正则） |
| corpus/s04.jsonl | claude | default | 14 | 6 | 本地大模型部署（Ollama / 量化 / embedding 模型拉取失败） |
| corpus/s05.jsonl | codex | default | 46 | 20 | 文档翻译与排版（术语表 / LaTeX / 双语交付）— **长会话** |
| corpus/s06.jsonl | codex | default | 26 | 11 | 定时任务排障（cron / flock / 时区 / 补跑） |
| corpus/s07_private.jsonl | claude | work | 10 | 5 | 权限审计（高危账号 / 离职回收 / 薪资日志抽查）— **work 域** |

格式覆盖：codex 4 个（≥3）、claude 3 个（≥2）；主题互不重复，均为工程/办公场景；
中英混杂自然出现（s02 助手英文答 CI 报错、s03 用户英文提问、s05 LaTeX/字体
三处英文答、s06 用户英文提问）。

- 长会话：s05 共 46 行 ≥40，全部 long_session 目标轮次位于 L22-L44（会话中后部）。
- 干扰项：每个会话内含多个相近但结论不同的轮次。例：s01 有两个「恢复失败」轮次
  （collation 缺失 vs 缺 `--create`）；s02 有三个「体积」轮次（语言包 / 压缩率 / 拆包粒度）；
  s03 有三个「日志涨满」轮次（logrotate 没跑 / 进程抱旧句柄 / journald 膨胀）；s06 有
  两个「cron 不执行」轮次（PATH vs 换行符）。检索靠单一关键词会同时命中干扰轮次。
- 规则覆盖：claude 的 tool_result-only 用户记录（s02 L5、s04 L7）不开新轮次、
  codex 的 function_call/function_call_output 归入当前轮次、session_meta 归入首轮、
  event_msg.agent_message 归入当前轮次（s05 L42、s06 L26）。
- work 域：s07_private.jsonl 独立 archive 根，用于权限反例。

### 1.2 查询（49 条，queries.jsonl）

| 类别 | 数量 | 设计意图 |
|---|---|---|
| exact | 9 | 查询直接带目标轮次的字面词（collation、pg_archivecleanup、IgnorePlugin、copytruncate、vacuum、num_ctx、longtable、flock、Automatic publicPath），用于词面基线的标注自洽锚点。 |
| synonym | 11 | 换说法且不复用原文特征词（如「数据快照能不能救场」→恢复演练、「事务日志腾地方」→pg_archivecleanup、「各国文案撑大包」→IgnorePlugin、「进门第一屏多等一次请求」→拆包粒度）。故意不含字面词，词面下失败属预期，考察向量路径。 |
| cross_lingual | 11 | 中文问英文答 5 条（s02 CI 报错、s05 longtable/pdffonts/vector PDF/biblatex）、英文问中文答 6 条（s01 collation、s03 log_format、s04 embedding 降级、s05 目录/双版本、s06 cron PATH）。 |
| long_session | 7 | 全部锚定 s05 的中后部轮次（目录页码 L23、交叉引用 L31、双版本 L35、Word 目录 L37、页边距 L39、术语回填 L41、终校 L44），考察长会话深轮次检索。 |
| model_missing | 5 | 验证「模型缺失时词面路径不中断」（§5）：主题围绕模型/资源缺席的兜底（embedding 拉取失败→词面检索顶着、离线装 gguf、语义打标降级词面打标、镜像源挂掉、术语库没挂上），措辞含可词面命中的字面词（embedding 模型、gguf、模型镜像、术语库），保证词面下可过。 |
| permission_negative | 6 | 严格 empty 3 条（q44 离职面谈纪要 / q45 报销单据审批 / q49 团建经费分摊，正文确认不存在于 default 语料）；no_hits_from_source 3 条（q46 高危账号、q47 离职账号密钥、q48 薪资日志抽查，正文都在 work 语料内，断言 archive-work 不得命中 + DOMAIN_UNREACHABLE；q46 的措辞故意与 default 语料有词面噪声重叠，验证「噪声不算泄露」）；q48 同时带 empty + no_hits_from_source（按 §3.2 以 no_hits_from_source 为准，对齐文档示例 q37）。 |

正向标注全部为 `file` + `line` + `contains` 单锚点（43 条），无 `expect_any`
（未出现「多个不同轮次都可接受」的情形）。`line` 均指向期望轮次内任一行，
`contains` 均在期望轮次内出现。查询措辞按事后回忆口吻写（「……来着」「那阵子」
「最后是怎么弄的」），不复述原句。

## 2. 校验

### 2.1 自写标注检查（独立按 §2.3 推导轮次）

内联 python 脚本（不落盘、只读本目录）：JSON 合法性、轮次推导、line∈轮次、
contains∈轮次、id 唯一、类别计数、严格 empty 查询词在 default 语料中零出现。
结果：7 文件 132 行 56 轮全部解析成功，43 条正向锚点全部通过，`=== NO ERRORS ===`，
exit 0。工具报告中的 `expected_turns` 与自检轮次推导逐条一致。

### 2.2 官方词面校验（唯一运行的评估工具命令）

```
python C:\wt\hybrid-retrieval-20261028\assets\scripts\recall_eval.py --set <heldout目录> --mode none --binary C:\wt\hybrid-retrieval-20261028\agent-memory\target\debug\agent-memory.exe --out <heldout目录>\lexical-check.json
```

退出码 **0**。stages：init（archive-default / archive-work 两个独立运行时根）、
capture（7 文件全部 exit 0）、replay（两源 exit 0，semantic=null 符合 `--mode none`）、
search 49、read 229。指标快照（`lexical-check.json` → `metrics`）：

- recall_at_5 = 0.800（36/45），first_hit_accuracy = 0.778（35/45）
- 行级诊断：recall_at_5_line = 0.400，first_hit_line_accuracy = 0.156（仅供参考）
- readback_success_rate = 1.000（229/229）
- negative_pass_rate = 0.500（3/6）
- latency_ms：cold_start 17.4，p95 16.7；index_size.ratio 1.880（unit_ratio 3.546）
- 分项：exact 8/9、synonym 9/11、cross_lingual 7/11、long_session 7/7、
  model_missing 5/5、permission_negative 3/6

语义模式**未运行**（按任务要求）。held-out 首次语义运行应在实现冻结后进行（§6.6）。

## 3. 词面校验发现与处置清单

**修正的标注错误：无。** 复查项 2 类，均判定为检索行为/断言难度而非标注错误，未改动任何查询：

1. **q05（exact）词面未命中**：`logrotate 里 copytruncate 和 USR1 怎么选` → 期望
   s03 轮次 2。标注复核：line 5 ∈ 轮次 2，contains `copytruncate` 在轮次内，
   工具自算 `expected_turns=[2]` 与自检一致，标注无误。三个字面词分散在轮次 2 的
   L5-L7 而不在同一条记录；词面 top-5 全是含「里/怎么」的无关行（s05 L14/L10/L4、
   s02 L7、s06 L24），无任何 s03 命中，属词面匹配行为差异。改写查询使其命中属于
   为得分调参，禁止；保留原查询，如实计入 exact 8/9。
2. **3 条严格 empty 负例在词面下有 1 条噪声命中**（q44→s01 L8「现在」、
   q45→s01 L12「哪一步」、q49→s05 L15）：命中内容与查询语义无关，正文词已确认
   不存在于 default 语料（work 语料对 default 查询不可达）。empty 负例没有
   file/line/contains 可标错，不存在标注错误；词面检索对中文虚词 n-gram 产生
   低相关噪声是被测系统行为，negative_pass_rate 本就是暴露该行为的指标。
   按禁止改写难查询的要求，保留原断言与措辞。§5 只要求 `model_missing` 在
   `--mode none` 下必须通过，未要求负例通过。[UNCERTAIN] 见 §4.2。

## 4. 边界与 [UNCERTAIN]

1. [UNCERTAIN] `model_missing` 语义：文档只明确「该类查询在词面模式下仍应通过，
   验证模型缺失时词面路径不中断」（§5）。本集据此构造：5 条均为词面可命中、
   且主题围绕模型/资源缺席兜底的正向查询。若验收口径是别的含义（如仅限
   「答案提到模型缺失」的查询），需重新划分。
2. [UNCERTAIN] 严格 empty 断言在词面噪声下的可达性：文档最小示例用
   `needle-not-in-corpus` 这类绝对不可命中的词做 empty 负例；本集按任务要求的
   「像真人事后回忆」措辞写严格 empty 负例，换来词面下 1 条噪声命中。若验收要求
   词面模式下 negative_pass_rate=1.0，strict empty 应换成 needle 式查询——本次
   刻意不做，以免落入「为得分改写查询」。噪声命中行已记录在 §3。
3. [UNCERTAIN] `event_msg.agent_message` 的 payload 载荷字段名（`message` 或
   `content`）文档未给示例，两条该型记录同时带两个字段以对冲解析差异；两条都不
   承担锚点 contains，不影响标注有效性。
4. 未修改格式文档与工具；未读取实现侧任何文件；产出只写入本目录。

## 5. 文件清单（字节数）

| 文件 | 字节 |
|---|---|
| manifest.json | 949 |
| queries.jsonl | 8 481 |
| corpus/s01.jsonl | 2 262 |
| corpus/s02.jsonl | 2 218 |
| corpus/s03.jsonl | 1 990 |
| corpus/s04.jsonl | 2 317 |
| corpus/s05.jsonl | 8 258 |
| corpus/s06.jsonl | 4 842 |
| corpus/s07_private.jsonl | 1 677 |
| lexical-check.json（校验产物） | 353 533 |
| BUILD-NOTES.md（本文件） | 计数见最终交付回复 |
