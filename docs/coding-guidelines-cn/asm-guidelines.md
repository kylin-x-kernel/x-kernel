# 汇编指南

本指南适用于模块级 `global_asm!` 块和独立 `.S` 文件中的汇编代码。有关底层设计理念，请参阅[指南的编写方式](how-guidelines-are-written.md)。

## 段（Sections）

### 使用正确的段指令（`asm-section-directives`）{#asm-section-directives}

对于内置段，请使用简短指令（例如 `.text`）。
对于自定义段，请使用带标志和类型的 `.section` 指令（例如 `.section ".bsp_boot", "awx", @progbits`）。

每个段定义后应跟一个空行，
以便在视觉上将其与后续代码分隔开。

```asm
.section ".bsp_boot.stack", "aw", @nobits

boot_stack_bottom:
    .balign 4096
    .skip 0x40000  # 256 KiB
boot_stack_top:
```

### 将代码宽度指令放在段定义之后（`asm-code-width`）{#asm-code-width}

在 x86-64 架构中，如果一个可执行段仅包含 64 位代码，
请将 `.code64` 指令直接放在段定义之后。
同理，对于 32 位代码，也应对 `.code32` 执行相同操作。
在混合代码段中，请将 `.code64` 和 `.code32`
视为函数属性（见下文）。

```asm
.text
.code64

.global foo
foo:
    mov rax, 1
    ret
```

## 函数

### 将属性直接放在函数之前（`asm-function-attributes`）{#asm-function-attributes}

函数属性（`.global`、`.balign`、`.type`）应直接放在函数标签之前，
且不应缩进。
为清晰起见，优先使用 `.global` 而非 `.globl`。

```asm
.balign 4
.global foo
foo:
    mov rax, 1
    ret
```

### 为 Rust 可调用函数添加 `.type` 和 `.size`（`asm-type-and-size`）{#asm-type-and-size}

可从 Rust 代码调用的函数应包含 `.type` 和 `.size` 指令。
这有助于调试器更好地理解该函数。

```asm
.global bar
.type bar, @function
bar:
    mov rax, 2
    ret
.size bar, .-bar
```

这不适用于启动入口点、异常跳板或中断跳板——它们可能不符合 "函数" 的典型定义，其大小也可能无法明确定义。

另请参阅：
PR [#2320](https://github.com/asterinas/asterinas/pull/2320)。

### 使用唯一标签前缀避免名称冲突（`asm-label-prefixes`）{#asm-label-prefixes}

一个 Rust crate 是一个单一的编译单元，因此同一 crate 内不同模块中的 `global_asm!` 标签共享同一个全局命名空间。请为标签添加自定义前缀以避免名称冲突。

（例如，BSP 启动代码使用 `bsp_`，AP 启动代码使用 `ap_`）。

```asm
# 良好实践——添加前缀以避免冲突
bsp_boot_stack_top:
ap_boot_stack_top:

# 糟糕实践——通用名称可能导致重复
boot_stack_top:
```

另请参阅：
PR [#2571](https://github.com/asterinas/asterinas/pull/2571)
和 [#2573](https://github.com/asterinas/asterinas/pull/2573)。

### 优先使用 `.balign` 而非 `.align`（`asm-prefer-balign`）{#asm-prefer-balign}

`.align` 指令的行为因架构而异——在某些架构上它指定字节计数，在其他架构上则指定 2 的幂次。请使用 `.balign` 进行明确的字节计数对齐。

```asm
# 良好实践——无歧义
.balign 4096

# 糟糕实践——含义取决于架构
.align 12
```

另请参阅：
PR [#2368](https://github.com/asterinas/asterinas/pull/2368)。
