# khal::power - 设计文档

## 定位

`khal::power` 是系统电源终点的 HAL 边界。它把平台裸终点
（`kplat::sys::SysCtrl`）包装成唯一的普通终点入口，并在进入终点前停止
其他 CPU。本模块不做任何上层收尾：filesystem sync、进程退出、设备
teardown、orderly shutdown 阶段状态都不属于这里。

## 范围

```text
arch/khal/src/power.rs # 本模块
platforms/kplat/src/sys.rs
                       # SysCtrl::halt / power_off 平台终点契约
platforms/kplat-*/src/power.rs
                       # 各平台终点实现
```

## 接口

| API | 语义 |
| --- | --- |
| `halt() -> !` | 普通终点：先执行 `SmpStopIf::stop_other_cpus()`，再调用平台 `halt`，停止 CPU 并保持供电 |
| `power_off() -> !` | 普通终点：先执行 `SmpStopIf::stop_other_cpus()`，再调用平台 `power_off`，通过平台断电代理（PSCI SYSTEM_OFF / SBI SRST / ACPI PM 端口 / GED 寄存器等）断电 |
| `suspend_to_ram() -> KResult<()>` | 非终点：请求平台睡眠代理进入 S3。不做 SMP stop（最小路径无 resume 侧 CPU 补插），平台不能睡时返回错误供调用方降级；当前各平台均未接线睡眠代理，返回 `ENOSYS` |
| `platform_halt() -> !` | 裸平台终点，不做 SMP stop |
| `platform_power_off() -> !` | 裸平台终点，不做 SMP stop |
| `platform_suspend_to_ram() -> KResult<()>` | 裸平台睡眠请求，不做 SMP stop |
| `SmpStopIf`（kiface） | 停其他 CPU 的接口契约；SMP 构建由 `kipi` 提供，UP 构建由本模块内 no-op 兜底，链接期保证恰好一个 provider |

## 架构

```text
sys_reboot / entry / ktask 正常路径          panic / crash 路径
        |                                          |
        v                                          v
khal::power::{halt, power_off}          khal::power::platform_power_off
        |-- SmpStopIf（SMP: kipi provider / UP: khal 内 no-op）
        `-- kplat::sys::{halt, power_off}          |
                        |                          |
                        v                          v
              platforms/kplat-*/src/power.rs（纯平台终点）
```

- UP 构建（无远端 CPU 可停）或 provider 未就绪（`kipi` 的本地 IPI 队列
  初始化之前走到终点）时，SMP stop 退化为 no-op，普通终点等效于裸平台
  终点。
- 平台 `halt` 实现统一为调用一次 `karch::stop_cpu()`（`-> !` 终结：屏蔽
  本地异常后循环执行架构等待指令 wfi/hlt/idle），不调用任何断电代理；
  平台 `power_off` 保持各平台原有断电路径，断电请求意外返回时同样落进
  `stop_cpu()` 终结。
- 停机协议在 park 前通过 `khal::quiesce_nmi()` 停靠各 CPU 的本地
  NMI/pseudo-NMI 源（无 NMI 设施的平台为 no-op）；`halt()` / `power_off()`
  在调用裸平台终点前同样停靠终端 CPU 自身的 NMI 源。这避免 NMI 驱动的
  hard-lockup 看门狗唤醒停靠中的 CPU、并把故意停机误判为 lockup 后 panic
  走平台断电，从而保住 `halt` 与 `power_off` 的语义区分。

## 调用约束 / 执行上下文

- `halt` / `power_off` 永不返回；调用前必须完成所有上层清理。
- `SmpStopIf` 的 provider 必须可以在关中断上下文调用（例如调度器内的
  init 任务退出路径），不得睡眠；`kipi` 的实现不分配内存、带 1s 有界超
  时。
- panic / crash 路径禁止走 SMP stop：panic CPU 可能持有其他 CPU 自旋等
  待的锁，IPI 停机会死锁；必须使用 `platform_*` 裸终点。
- 平台终点实现不得依赖 VFS、process、syscall 或任何上层子系统。

## 并发模型

- SMP stop 协议本体（stop 状态字、per-CPU ack）由 `kipi` 维护，见
  `arch/kipi/src/stop.rs`；ack 是停靠 CPU 对共享状态的最后一次写，置位
  后不再有日志或锁操作。
- 接口经 `kiface` 在链接期绑定，本模块没有可变全局状态，终点路径无锁。
- 两个 CPU 同时进入终点时，`kipi` provider 的重入保护让后到者 ack 并自
  停，不会双 orchestrator。

## 设计决策

### 为什么用 kiface 接口而不是让调用方各自组合

终点调用方分布在 `ksyscall`、`entry`、`ktask`、`kruntime`，而契约的
自然归属是终点本身（`khal::power`），实现归属是 `kipi`——但 `kipi` 依赖
`khal`，反向依赖会成环。`kiface` 把契约放在 `khal`、provider 放在
`kipi`、绑定推迟到链接期，所有调用点零 cfg、零新增依赖，"先停其他 CPU
再进终点"的序列只有一份实现，且链接器保证全图恰好一个 provider（与
`karch::IcacheFlushIf` 同款模式：`kipi` 通过 Cargo features 打开
`khal/smp`，把本模块的 no-op 兜底编译掉）。UP 构建不链接 `kipi`，兜底自
动生效；provider 在 `kipi::init()` 之前的调用退化为 no-op，对应旧注册式
实现"runtime 初始化完成后才挂钩子"的时序语义。

### 为什么 panic 路径绕过钩子

`kipi` 的 SMP stop provider 通过普通 IPI 停机，目标 CPU 必须能进入 IPI
handler；panic CPU 持锁时其他 CPU 可能在关中断自旋，永远无法响应。Linux
用 NMI 做 crash stop，本仓库尚无对应机制，故 panic 走裸终点、best-effort
断电，保持拆分 `shutdown()` 之前的行为。

### 为什么不在本层保存 NoRequest/Halt/PowerOff 状态

终点的"请求→执行"两阶段状态属于未来 orderly-shutdown supervisor 的职
责；本模块只提供无状态的执行终点，避免状态在 syscall、process、
runtime、HAL 之间复制多份。

### 为什么 park 前要停靠本地 NMI 源

`karch::stop_cpu()` 在 CPU 级屏蔽异常：AArch64 全掩 DAIF 四类（含
pseudo-NMI），其它架构屏蔽本地可屏蔽中断。但真正的 NMI（x86 的 NMI 不受
IF 控制）无法靠 CPU 级屏蔽挡住。在启用 NMI hard-lockup 看门狗的平台，
若不停靠 NMI 源，停靠中的 CPU 会被周期性 NMI 反复唤醒，看门狗还会把
"所有任务都不再运行"误判为 hard lockup 并 panic 走平台断电——这会让
`halt` 在数十秒后变成 `power_off`。因此在 park 前调用 `quiesce_nmi()`
停靠本地 NMI 源；它与平台终点解耦、无 NMI 时为 no-op，不引入上层依赖。
在 pseudo-NMI 已被 DAIF 全掩挡住的 AArch64 上这属于纵深防御。
