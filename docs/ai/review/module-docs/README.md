# 文档审查检查点

这里是 X-Kernel 文档检查点的权威维护位置，供显式 `docreview` 和 `docreview crate=xxx` 使用。普通代码 review/re-review 不启动文档审计。

- `check.xlsx`：人工维护源，含 design、security、rustdoc 三个工作表。
- `design-doc-checks.tsv`、`security-doc-checks.tsv`、`rustdoc-checks.tsv`：程序读取的检查点。
- `export_checkpoints.py`：仅依赖 Python 标准库的导出和一致性检查工具。

在 X-Kernel 仓库根目录运行：

```sh
python3 docs/ai/review/module-docs/export_checkpoints.py
python3 docs/ai/review/module-docs/export_checkpoints.py --check
```

修改工作簿后一起提交导出的 TSV。x-review 从本次审查固定的 PR HEAD（本地审计则为指定 revision）快照读取三个 TSV，不从目标分支或未提交工作区读取。文件缺失、格式错误或空表会阻止审查，不回退到内置规则；部署新版工具前，应先让被审查分支包含这些文件。规则变化会使对应旧缓存失效。

表头固定为：规则 ID、等级、检查点、合格标准、问题判定、错误示例（以制表符分隔）。等级为 MUST、SHOULD 或 MAY，每个字段必须非空。规则 ID 保持 DD-*、SD-*、RD-* 的稳定含义。当前对象路由、本地确定性检查和编译器检查仍在 x-review 中实现；新增 ID、改变适用对象或本地判定语义需要同步修改执行器。描述、示例、等级可随本仓库规则提交更新，无需重新编译 x-review。

审查执行协议和工具调度由 x-review 维护。本目录不是生成模块文档的 skill；生成文档仍使用 `docs/ai/skills/module-docs/`。
