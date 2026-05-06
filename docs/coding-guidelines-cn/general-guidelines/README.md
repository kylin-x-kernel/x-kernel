# 通用指南

## 命名

### 命名应具有描述性（`descriptive-names`）{#descriptive-names}

选择在使用时能传达含义的名称。
避免使用单个字母的命名和含糊不清的缩写。
优先使用完整的单词而非晦涩的简写，
以便读者无需依赖上下文就能理解变量的用途。
在确保使用时不产生歧义的前提下，
应尽可能使用简短的名称。

### 命名应准确（`accurate-names`）{#accurate-names}

避免使用令人混淆的名称。
如果某个名称可能被误读，
从而暗示错误的含义、行为或副作用，
必须立即修正。

```rust
// 好——明确表示计数
nr_deleted_watches: usize,
// 差——看起来像集合
// 而非数值计数器
deleted_watches: usize
```

选择能反映实际执行工作的动词。

```rust
impl PciCommonDevice {
    // 好——暗示涉及 MMIO 读取操作
    pub fn read_command(&self) -> Command { /* .. */ }
    // 差——看起来像简单的字段访问
    pub fn command(&self) -> Command { /* .. */ }
}
```

```rust
mod char_device {
    // 好——暗示需要对集合进行 O(n) 遍历
    pub fn collect_all() -> Vec<Arc<dyn Device>> { /* .. */ }
    // 差——听起来像返回引用的访问器
    pub fn get_all() -> Vec<Arc<dyn Device>> { /* .. */ }
}
```

---

另请参阅：
PR [#1488](https://github.com/asterinas/asterinas/pull/1488#discussion_r1825441287)
和 [#2964](https://github.com/asterinas/asterinas/pull/2964#discussion_r2789739882)。

### 在名称中编码单位和重要属性（`encode-units`）{#encode-units}

当类型不编码单位时，
名称必须明确编码。
内核代码处理字节、页、帧、
纳秒、滴答和扇区——
模糊的单位是真实 bug 的来源。

```text
// 好——单位明确
timeout_ns
offset_bytes
size_pages
delay_ms

// 差——单位模糊
timeout
offset
size
delay
```

在语言类型系统能够强制单位约束（例如 `newtype`）的情况下，优先使用类型系统。
当类型系统无法做到时，名称必须承载单位信息。

另请参阅：
PR [#2796](https://github.com/asterinas/asterinas/pull/2796#discussion_r2646889913)。

### 对布尔值名称使用断言风格（`bool-names`）{#bool-names}

布尔变量和函数的命名应像事实断言一样。
使用 `is_`、`has_`、`can_`、`should_`、`was_`
或 `needs_` 前缀。
切勿使用否定形式的名称——
---

(`is_not_empty`、`no_error`)；
应优先使用肯定形式
（`is_empty`、`ok` 或 `succeeded`）。
当上下文明确时，像 `found`、`done` 或 `ready` 这样的裸名称也是可接受的。

```rust
// 好——读起来像断言
fn is_page_aligned(&self) -> bool { ... }
fn has_permission(&self, perm: Permission) -> bool { ... }
let can_read = mode.is_readable();

// 差——动词暗示行为，而非查询
fn check_permission(&self, perm: Permission) -> bool { ... }
// 差——否定名称
let is_not_empty = !buf.is_empty();
```

---

另请参阅：
PR [#1488](https://github.com/asterinas/asterinas/pull/1488#discussion_r1841827039)。

## 注释

### 优先使用语义换行（`semantic-line-breaks`）{#semantic-line-breaks}

对于 Markdown 和文档注释中的散文，
在语义边界处插入换行，
使得每行承载一个连贯的想法。
至少要在句子边界处换行。
对于较长的句子，还应考虑在子句边界处换行。

语义换行可以使差异（diff）更小，

使代码审查更容易，
并使合并冲突的噪声更小。

作为一个例外，
主要用于阅读的 RFC 文档
可以使用常规段落换行。

另请参阅：
[语义换行](https://sembr.org/)。

### 解释原因，而非内容（`explain-why`）{#explain-why}

注释应解释代码背后的意图，
而非复述代码做了什么。
如果注释只是对代码的转述，

它只会增加噪音，而不会带来洞察。

如果需要注释来解释代码做了什么，
首先应尝试重写代码。
不要用优秀的注释来弥补糟糕的代码——
应将代码重写得一目了然。

另请参阅：
《可读代码的艺术》第6章"知道要注释什么"；
PR [#2265](https://github.com/asterinas/asterinas/pull/2265#discussion_r2266220943)
和 [#2050](https://github.com/asterinas/asterinas/pull/2050#discussion_r2224106025)。

### 记录设计决策（`design-decisions`）{#design-decisions}

当代码做出了不显而易见的选择时——

一个特定的数据结构、一种锁策略，
一项与 Linux 行为的差异——
添加注释解释其理由
以及考虑过的任何备选方案。
设计决策注释（"导演评论"）
是最有价值的注释类型。

```rust
// 我们使用基数树而不是 HashMap，
// 因为在缺页处理程序中，
// 查找操作在最坏情况下必须是 O(log n)。
// HashMap 在平摊情况下是 O(1)，
// 但由于重新哈希在最坏情况下是 O(n)，
// 这在缺页路径上是不可接受的。
```

另请参阅：
PR [#2265](https://github.com/asterinas/asterinas/pull/2265#discussion_r2266220943)
和 [#2050](https://github.com/asterinas/asterinas/pull/2050#discussion_r2224106025)。

### 引用规范和算法来源（`cite-sources`）{#cite-sources}

当实现由外部规范或非平凡算法定义的行为时，
请引用来源：
相关的 POSIX 章节、Linux 手册页、
硬件参考手册或学术论文。

```rust
/// 保证能以原子方式写入管道的最大字节数。
///
/// 更多细节，请参见 `PIPE_BUF` 在
/// <https://man7.org/linux/man-pages/man7/pipe.7.html> 中的描述。
const PIPE_BUF: usize = 4096;
```

## 布局

### 一个文件一个概念（`one-concept-per-file`）{#one-concept-per-file}

当文件变得过长或包含多个不同概念时，
应将其拆分。
每个主要数据结构、每个子系统入口点、
每个重要的抽象
都应有自己的文件。

### 为自上而下阅读组织代码（`top-down-reading`）{#top-down-reading}

源文件应能自上而下阅读。
从高层入口点和核心流程开始。

将实现细节放在后面，
这样读者可以先了解整体概况，
再深入底层辅助逻辑。

在每个可见性组（如模块）中，
对方法进行排序，使调用者尽可能出现在被调用者之前，
从而支持文件的从上到下阅读。
将公有方法放在私有辅助方法之前。

### 将语句组织成逻辑段落（`logical-paragraphs`）{#logical-paragraphs}

在函数内部，
将相关语句组织成由空行分隔的逻辑段落。
每个段落应代表一个子步骤。

函数整体目标的实现。

对于长函数，
当段落意图不明显时，
在每个段落开头添加一行总结性注释。

## 格式

### 保持错误信息格式一致（`error-message-format`）{#error-message-format}

以小写字母开头
（除非第一个词是专有名词或标识符）。
具体明确：
优先使用"`len` 太长"而不是"参数无效"。

对于系统调用错误，
遵循 Linux 手册页中的风格和描述。

## API 设计

### 遵循熟悉的约定（`familiar-conventions`）{#familiar-conventions}

优先使用用户已从 Rust 和 Linux 中了解的
名称和 API 形状。
不要为众所周知的操作
发明新的术语。

```rust
// 好——遵循常见的 Rust 命名约定
pub fn len(&self) -> usize { ... }
pub fn as_ptr(&self) -> *const u8 { ... }

// 坏——对常见操作使用不熟悉的同义词
pub fn length(&self) -> usize { ... }
pub fn to_pointer(&self) -> *const u8 { ... }
```

另见：
[最小意外原则](../how-guidelines-are-written.md#least-surprise)。

### 隐藏实现细节（`hide-impl-details`）{#hide-impl-details}

不要通过公共 API（包括其文档）
暴露内部实现细节。
模块的公共接口
应仅包含其使用者所需的内容。

另见：
[Rust 专属可见性规则](../rust-guidelines/language-items/modules-and-crates.md#narrow-visibility)中的
[模块与 crate](../rust-guidelines/language-items/modules-and-crates.md#narrow-visibility)；
PR [#2951](https://github.com/asterinas/asterinas/pull/2951#discussion_r2786925410)。

### 在边界处验证，内部信任（`validate-at-boundaries`）{#validate-at-boundaries}

将特定接口指定为验证边界。
在 Asterinas 中，系统调用入口点
是主要的边界：
所有用户提供的数据
（指针、文件描述符、大小、标志、字符串）
必须在系统调用边界进行验证。
一旦验证通过，内部内核函数
可以信任这些值而无需重新验证。

另见：
PR [#2806](https://github.com/asterinas/asterinas/pull/2806)。
