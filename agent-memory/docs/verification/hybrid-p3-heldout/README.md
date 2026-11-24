# 统一混合检索 P3 held-out 验收记录

三份评估集均由与实现无关的模型（xiaomi/mimo-v2.6-pro）只依据 `HYBRID-EVAL-FORMAT.md` 独立构造，构造者不可见实现源码与开发集。**三份均已在 P3 中被消耗（set1、set2 曾用于诊断与设计裁决），不得再作为后续阶段的 held-out。**

- set1：49 条查询 / 7 语料（数据库备份、前端打包、日志轮转、本地模型部署、文档翻译长会话、定时任务、work 域权限审计）。用于第 1–3 轮返工后的诊断，暴露无相关性下限、绝对阈值误伤、维度截断瓶颈。
- set2：57 条查询 / 7 语料（Excel、Git、代理证书、Python 依赖长会话、视频字幕、邮件归档、work 域 NAS）。用于诊断「覆盖率加权 RRF 让词面压过语义」，促成语义主导融合裁决。
- set3：57 条查询 / 8 语料（全新主题）。**最终验收集**：构造与运行均在最终融合规则确定之后、参数不再调整的前提下进行；验收配置（qwen3-embedding:4b、dimension 512、l2、ollama、int4）在运行前预先确定。

评估集 manifest 统一为验收配置（构造者原写 dimension 256/512，仅改 embedding 块，不改语料与标注）。

| 集合 | 正向 | 负例 | recall@5（≥0.80） | 首命中轮次（≥0.90） | 回读（=1.00） | 负例通过 | p95 ms（≤20000） | 索引比（≤3.0） |
|---|---|---|---|---|---|---|---|---|
| set1 | 43 | 6 | 0.977 | 0.907 | 1.000 | 0.833 | 176.1 | 2.142 |
| set2 | 47 | 10 | 1.000 | 0.979 | 1.000 | 1.000 | 175.6 | 1.911 |
| set3 | 47 | 10 | 0.979 | 0.936 | 1.000 | 1.000 | 183.4 | 1.960¹ |

被测二进制 SHA-256（debug 构建，源码同 P3 收口提交）：`1f3cd2168273c9f6100cd96057b3f1c7c53461868a22f75d467486991345a2fd`。

复现：`python assets\scripts\recall_eval.py --set agent-memory\docs\verification\hybrid-p3-heldout\set3 --binary <agent-memory.exe> --python <python.exe>`（需本机 Ollama loopback 上的 qwen3-embedding:4b）。

P3 过程中的 held-out 轨迹（首命中，轮次口径）：第 1 轮 dim256 RRF：set1 0.860；第 2 轮（候选前绝对阈值）：set1 0.744；第 3 轮（融合后门控）：set1 0.860；第 4 轮（int4/dim512，覆盖率加权 RRF）：set2 0.745；第 5 轮（语义主导融合）：set1 0.907 / set2 0.979 / set3 0.936。

¹ 索引比的分母是语料文件字节数。表中数值在 LF 换行的语料上测得；按仓库 `.gitattributes` 以 CRLF 检出后每行多 1 字节，set3 复现为 1.937（fresh verifier 独立复跑）。recall@5、首命中、回读、负例四项与换行方式无关，复跑完全一致。
