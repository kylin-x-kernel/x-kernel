# 测试指南

本页涵盖了与语言无关的测试约定。
有关 Rust 特定的断言策略（`assert!` vs `debug_assert!`），请参阅
[Rust 指南 — 防御性编程](rust-guidelines/select-topics/defensive-programming.md)。

### 为每个 Bug 修复添加回归测试（`add-regression-tests`） {#add-regression-tests}

当修复一个 Bug 时，
应附带一个原本能够捕获该 Bug 的测试。
在注释中引用该问题的编号，
以便未来的读者能够恢复原始上下文。

另请参阅：

---
PR [#2962](https://github.com/asterinas/asterinas/pull/2962).

### 测试用户可见的行为，而非内部实现（`test-visible-behavior`） {#test-visible-behavior}

测试应验证可观察的、面向用户的结果。
倾向于通过公共 API 进行测试，
而非在测试代码中暴露内部常量。

根据正在验证的行为或规范概念来命名测试，
而非根据内部实现细节。
在用户空间回归测试中使用内核内部名称
会引入不必要的耦合。

另请参阅：
PR [#2926](https://github.com/asterinas/asterinas/pull/2926).

### 使用断言宏，而非手动检查（`use-assertions`）{#use-assertions}

应使用语言或框架提供的断言辅助工具，
而非打印值并手动检查输出。
断言能提供清晰的失败信息，
并使测试具备自检能力。

另请参阅：
PR [#2877](https://github.com/asterinas/asterinas/pull/2877)
和 [#2926](https://github.com/asterinas/asterinas/pull/2926).

### 每次测试后清理资源（`test-cleanup`）{#test-cleanup}

始终在测试完成后清理资源：

关闭文件描述符、删除临时文件，
并在子进程上调用 `waitpid`。
残留的资源可能导致后续测试出现间歇性失败。

```c
// 良好实践——使用后清理资源
int fd = open("/tmp/test_file", O_CREAT | O_RDWR, 0644);
// ... 测试逻辑 ...
close(fd);
unlink("/tmp/test_file");
```

另请参阅：
PR [#2926](https://github.com/asterinas/asterinas/pull/2926)

和 [#2969](https://github.com/asterinas/asterinas/pull/2969)。
