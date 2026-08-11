# X-Kernel 自动代码审查规则

本目录保存 X-Kernel 项目自有的 Pull Request 自动审查规则。
这些规则由项目仓库维护，而不是编译进 `x-review` 服务。
这样可以让审查标准与代码、架构和开发流程一起演进，
也可以让修改规则的 PR 使用源分支中的新规则接受审查。

## 规则从哪里读取

`x-review` 在运行 review pipeline 前会依次完成以下操作：

1. clone 或复用本地 X-Kernel 仓库；
2. 拉取并更新 PR 的目标分支；
3. 拉取 PR 的源分支；
4. 最终 checkout 到 PR 源分支对应的本地分支；
5. 启动各个相互独立的审查 agent。

每个 agent 收到的启动提示只包含 PR 运行时信息和对应规则文件路径。
agent 必须主动调用 `read_file` 从当前本地工作树读取规则。
禁止改为从目标分支读取，也不要假设 review 服务二进制中仍然内嵌项目规则。

如果对应规则文件不存在、无法读取或内容明显损坏，
当前阶段应停止并明确报告原因，不能退回到模型自行猜测的通用审查标准。

## 目录结构

```text
docs/ai/review/
├── README.md       # 入口、加载顺序和维护约定
├── common.md       # 所有发现型阶段共用的证据和输出契约
├── description.md  # PR 描述检查与更新
├── posix.md        # POSIX / Linux ABI 语义
├── bug.md          # 可触发实际错误的缺陷
├── performance.md  # 性能与资源使用
├── logic.md        # 逻辑、状态、错误路径、unsafe 和测试
└── guidelines.md   # X-Kernel 编码规范和代码库一致性
```

## 加载顺序

不同阶段按以下顺序加载文档：

| 阶段 | 必须读取 | 按变更类型继续读取 |
|---|---|---|
| Description | `description.md` | 无 |
| POSIX | `posix.md`、`common.md` | 相关 man page、Linux 参考实现 |
| Bug | `bug.md`、`common.md` | 相关模块设计与安全文档 |
| Performance | `performance.md`、`common.md` | performance skill 中相关 reference |
| Logic | `logic.md`、`common.md` | code-guidelines 中 unsafe、并发、边界等主题文件 |
| Guidelines | `guidelines.md`、`common.md` | code-guidelines 的入口和相关主题文件 |

`common.md` 负责统一评论格式、行号、证据门槛、suggestion 和误报约束。
各阶段文件不重复定义公共协议，只描述本阶段的审查职责。

## 审查阶段之间的边界

阶段划分的目的是让每个 agent 专注于一个问题域，
而不是要求同一个问题必须被多个阶段重复报告。

- Bug 阶段关注能够导致 panic、错误结果、资源泄漏、死锁或安全破坏的具体缺陷。
- Logic 阶段关注控制流、状态机、不变量、错误传播、unsafe 契约和测试完整性。
- Performance 阶段只报告有执行频率、复杂度或资源生命周期证据支撑的问题。
- POSIX 阶段只报告对用户可见语义、ABI、errno 或 Linux 兼容性的偏差。
- Guidelines 阶段关注规范合规、模块边界、API 形态和代码库一致性。
- Description 阶段不产生代码问题评论，而是判断并更新 PR 描述。

同一问题跨越多个维度时，选择证据最强、定位最准确的阶段报告即可。
不要为了填满阶段输出而重复或降低问题门槛。

## 维护原则

修改本目录时应遵守以下原则：

1. 项目特有规则写在这里，review 服务只负责调度和提供工具。
2. 可复用的编码规范继续以 `docs/ai/skills/code-guidelines/` 为权威来源，
   本目录只说明 review 阶段如何选择和执行这些规范。
3. 新增工具依赖时，必须确认 `x-review` 已向对应阶段暴露该工具。
4. 规则必须能导向可验证的证据，避免“代码不够优雅”一类无法落行的要求。
5. 发现型规则只评论 PR diff 中可评论的行，不借 review 处理无关历史问题。
6. 示例应使用 X-Kernel 的目录、类型和工作流，不继续引入 Asterinas 专有表述。
7. 规则更新后，应同步检查 `x-review` 中阶段到文件路径的映射。
