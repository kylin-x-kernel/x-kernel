# Git 指南

本指南涵盖提交规范和拉取请求惯例。
有关底层哲学，请参阅
[如何编写指南](how-guidelines-are-written.md)。

### 使用祈使语气和描述性的主题行 (`imperative-subject`) {#imperative-subject}

使用祈使语气编写提交信息，
主题行不超过72个字符。
标识符用反引号包裹。

Asterinas 提交日志中常用的前缀：

- `Fix` — 修复一个 bug

- `Add` — 引入新功能
- `Remove` — 删除代码或功能
- `Refactor` — 在不改变行为的前提下重组代码
- `Rename` — 更改文件、模块或符号的名称
- `Implement` — 添加新的子系统或功能
- `Enable` — 开启先前禁用的能力
- `Clean up` — 轻微整理，不涉及功能变更
- `Bump` — 更新依赖版本

示例：

```
Fix deadlock in `Vmar::protect` when holding the page table lock

Add initial support for the io_uring subsystem

Refactor `TcpSocket` to separate connection state from I/O logic
```

如果提交需要进一步说明，在主题行后添加一个空行，然后加上正文段落，描述变更背后的_原因_。

另请参阅：
PR [#2877](https://github.com/asterinas/asterinas/pull/2877)
和 [#2700](https://github.com/asterinas/asterinas/pull/2700)。

### 每次提交只做一个逻辑变更 (`atomic-commits`) {#atomic-commits}

每个提交应只代表一个逻辑变更。
不要在一个提交中混入无关的更改。
当修复审查过程中发现的问题时，

在本地或私有分支上时，  
请使用 `git rebase -i` 来修改引入问题的提交，  
而不是在末尾追加一个修复提交。

另请参阅：  
PR [#2791](https://github.com/asterinas/asterinas/pull/2791)  
和 [#2260](https://github.com/asterinas/asterinas/pull/2260)。

### 将重构与特性分离 (`refactor-then-feature`) {#refactor-then-feature}

如果某个特性需要前置重构，  
请将重构放在特性提交之前的独立提交中。  
这样能让每个提交更易于审查和二分定位。

另请参阅：
PR [#2877](https://github.com/asterinas/asterinas/pull/2877)。

### 保持拉取请求聚焦单一主题 (`focused-prs`) {#focused-prs}

拉取请求应围绕单一主题展开。
一个混合了错误修复、代码重构
和新功能的 PR 将难以审查。

在请求审查前，请确保 CI 通过。
如果 CI 因无关的偶发问题而失败，
请在 PR 描述中注明。
