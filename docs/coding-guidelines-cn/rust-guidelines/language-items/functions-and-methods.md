# 函数与方法

### 最小化嵌套（`minimize-nesting`）{#minimize-nesting}

最小化嵌套深度。
超过三层嵌套的代码应接受审查，寻找重构机会。
每一层嵌套都会成倍增加读者的认知负担。

平铺嵌套的技巧：
- 对错误路径使用提前返回和保护子句。
- 使用`let...else`简化`if let`链。
- 使用`?`操作符进行错误传播。
- 使用`continue`跳过循环迭代。
- 将嵌套体提取为辅助函数。

正常/预期的代码路径
应作为最先可见的路径；
错误和边界情况
应尽早处理并排除。

```rust
pub(crate) fn init() {
    let Some(framebuffer_arg) = boot_info().framebuffer_arg else {
        warn!("Framebuffer not found");
        return;
    };
    // ... 主逻辑保持在顶层
}
```

---

另见：
PR [#2877](https://github.com/asterinas/asterinas/pull/2877#discussion_r2685861741)。

### 保持函数小巧且专注（`small-functions`）{#small-functions}

每个函数应只做一件事，做好它，且只做这一件事。
如果你能从该函数中提取出另一个函数，
且提取出的函数名称不仅仅是其实现的重述，
那么原函数就做了不止一件事。

不要混合不同抽象层次。
例如，一个系统调用处理程序应读起来像一份规范说明；

字节级操作应归属于辅助函数。

```rust
// 好——每个函数只在一个抽象层次上操作
pub fn sys_connect(sockfd: i32, addr: Vaddr, len: u32) -> Result<()> {
    let socket = get_socket(sockfd)?;
    let remote_addr = parse_socket_addr(addr, len)?;
    socket.connect(remote_addr)
}

// 差——将高层逻辑与低层细节混杂在一起
pub fn sys_connect(sockfd: i32, addr: Vaddr, len: u32) -> Result<()> {
    let fd_table = current_process().fd_table().lock();
    let file = fd_table.get(sockfd).ok_or(Errno::EBADF)?;
    let socket = file.downcast_ref::<Socket>().ok_or(Errno::ENOTSOCK)?;
    let bytes = read_bytes_from_user(addr, len as usize)?;
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    // ... 30多行字节解析代码 ...
}
```

另见：
_Clean Code_ 第3章“函数”；
PR [#639](https://github.com/asterinas/asterinas/pull/639#discussion_r1524629393)。

### 避免布尔参数（`no-bool-args`）{#no-bool-args}

一个在两种行为之间选择的布尔参数，
表明该函数做了两件事。
应将其拆分为两个函数，
或使用带类型的枚举。

```rust
// 好——两个独立的函数
fn read(&self, buf: &mut [u8]) -> Result<usize> { ... }
fn read_nonblocking(&self, buf: &mut [u8]) -> Result<usize> { ... }

// 好——带类型的枚举
enum ReadMode { Blocking, NonBlocking }
fn read(&self, buf: &mut [u8], mode: ReadMode) -> Result<usize> { ... }

// 差——布尔参数
fn read(&self, buf: &mut [u8], blocking: bool) -> Result<usize> { ... }
```

另见：
_Clean Code_ 第3章“参数标志”。
