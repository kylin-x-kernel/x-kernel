# 类型与特质

### 使用类型来强制保证不变量 (`rust-type-invariants`) {#rust-type-invariants}

利用类型系统
使非法状态_无法表示_。

通过定义新类型（newtype）来编码领域约束。

```rust
// 良好实践 — `Nice` 值保证有效
pub struct Nice(NiceValue);
type NiceValue = RangedI8<-20, 19>;

// 糟糕实践 — `i8` 允许无效的nice值
pub type Nice = i8;
```

优先使用枚举而非裸整数和布尔标志。

```rust
// 良好实践 — 访问模式受枚举约束
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMode {
    O_RDONLY = 0,
    O_WRONLY = 1,
    O_RDWR = 2,
}

// 糟糕实践 — `u8` 允许无效值
pub type AccessMode = u8;
```

在泛型参数中编码不变量。

```rust
impl IoMem<Sensitive> {
    // 良好实践 — 只有不安全代码才能写入敏感的MMIO
    pub unsafe fn write_u32(&self, offset: usize, new_val: u32) { /* .. */ }
}

impl IoMem<Insensitive> {
    // 良好实践 — 安全代码可以写入不敏感的MMIO
    pub fn write_u32(&self, offset: usize, new_val: u32) { /* .. */ }
}

pub enum Sensitive {}
pub enum Insensitive {}
```

Asterinas 广泛使用了这一模式，
例如通过 `CpuId` 和 `AlignedUsize<const N: u16>` 等新类型。

另请参阅：
PR [#2265](https://github.com/asterinas/asterinas/pull/2265#discussion_r2266214191)
和 [#2514](https://github.com/asterinas/asterinas/pull/2514)。

### 在封闭集合中优先使用枚举而非 trait 对象 (`enum-over-dyn`) {#enum-over-dyn}

当变体的集合已确定且封闭时，
枚举通常比 `Box<dyn Trait>` 更优，
无论是在性能还是在模式匹配的表达力方面。

```rust
// 良好实践 — 封闭集合建模为枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermStatus {
    Exited(u8),
    Killed(SigNum),
}
```

### 通过 getter 方法封装字段 (`getter-encapsulation`) {#getter-encapsulation}

如果简单的 getter 方法就能满足需求，
就不要将字段设为公开。
Getter 方法保留了命名灵活性，
并为将来引入不变式留出了空间。

```rust
// 良好实践 — 字段为私有，通过 getter 访问
pub struct Vma {
    perms: VmPerms,
}

impl Vma {
    pub fn perms(&self) -> VmPerms {
        self.perms
    }
}

// 不良实践 — 公开字段暴露了内部表示
pub struct Vma {
    pub perms: VmPerms,
}
```
