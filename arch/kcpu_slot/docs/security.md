# kcpu_slot 安全说明

`kcpu_slot` 的不安全边界集中在三个位置：架构寄存器读写、模板复制和 slot 指针
运算。

调用者必须保证：

1. 模板 LMA 来自与 linker script 同一镜像，`initialize_cpu` 的目标区域对该 CPU
   唯一、可写，并覆盖 `template_size()` 字节；
2. 安装 base register 后，CPU 不在访问期间迁移；`PinCurrentCpu` 的调用者通过
   真实 guard（如 `kspin::NoPreempt`）负责关闭抢占或建立等价的调度器保证；
3. `CpuSlot::get_mut`/`CpuSlotCell::get` 的别名规则由调用者满足，跨 CPU 访问不能
   伪装成本地独占访问；`CpuSlot::get_at` 只在 `T: Sync` 下开放，未持锁远端写
   不在基础 API 中提供；
4. 链接脚本中的 section 符号和 crate 编译产物来自同一内核镜像，不能由不受信任
   的模块伪造；
5. `CpuSlotChunk::from_raw_parts` 的 backing memory 在所有动态 handle drop 前保持
   有效；`DynamicCpuSlot` 不允许脱离 chunk 生命周期；
6. 静态宏只接受 sealed `StaticSlotValue`；需要构造/析构的类型不得放入模板 section，
   必须使用动态 slot 的逐 CPU initializer。

架构 base register 是内核特权状态。它只能由平台架构初始化路径安装，不能通过
普通安全 API 任意改写；错误设置会导致任意 slot 访问越界。x86 使用 GS-relative
访问和 GS base 初始化，不假设 FSGSBASE 已启用。未支持架构在非测试构建下无法
编译，避免 base register 访问静默失效。

平台负责发布模板就绪、分配每个 CPU 的 backing area，并在调用 `initialize_cpu`
后才允许该 CPU 进入依赖 slot 的代码。`kcpu_slot` 不复制 BSP/AP 状态机，也不负责
IRQ、抢占或 CPU online 生命周期。
