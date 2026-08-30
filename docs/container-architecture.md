# X-Kernel 容器与资源控制架构设计

## 1. 文档状态

本文定义 X-Kernel 的容器、namespace 与分层资源控制的目标架构，并给出兼容
Linux cgroup v2 和 OCI 用户态的实施路径。

本文是跨子系统设计，不等同于某个 crate 的现状说明。各阶段实现落地时，还必须同步
更新受影响 crate 的 `docs/design.md`、`docs/security.md` 和公共 API rustdoc。

### 1.1 术语

| 术语 | 含义 |
|------|------|
| Container | 聚合 namespace、资源域和安全域的管理对象，不直接执行资源控制 |
| Namespace set | 进程可见性视图，对应 `NsProxy` 及 credential/PID 相关 namespace |
| Resource domain | 分层资源成员关系和策略对象，Linux ABI 下表现为 cgroup v2 hierarchy |
| Security context | 进程 credentials、user namespace、capabilities 和后续 LSM/MAC 策略；当前不是独立内核对象 |
| Cgroup hierarchy | Linux cgroup v2 unified hierarchy 在内核中的资源域树表示 |
| Controller | 在 CPU、内存、任务数或 I/O owner 中执行的资源策略 |
| Membership | task/process 到 resource domain node 的稳定归属关系 |

除明确讨论 Linux ABI 的章节外，本文使用“resource domain”表示内核原生资源控制
对象，使用“cgroup”表示 Linux cgroup v2 可见语义。

## 2. 背景与现状

X-Kernel 已经具备若干容器基础组件：

- `process/kns` 聚合 mount、UTS、IPC、PID-for-children、network、cgroup 和 time
  namespace 引用；
- `process/kidentity` 表达 PID namespace 链和稳定 PID identity；
- `process/kcred` 拥有 credentials 和 user namespace 身份；
- `process/kprocess` 拥有进程、线程、fork、exit 和 publication 生命周期；
- `task/ktask` 与 `task/ksched` 拥有任务调度和 per-CPU run queue；
- `mm/memspace`、`mm/anon`、`mm/pagecache` 等拥有虚拟地址空间和内存对象；
- `fs/kvfs` 拥有 mount namespace 和 VFS 对象；
- `net/knet` 提供网络栈，但 network namespace 仍未完成实例化；
- `process/kcgroup` 已实现 unified hierarchy、canonical task membership、pids controller
  和 cgroup namespace view；
- `fs/filesystems/cgroup2fs` 已提供基础 cgroup v2 ABI；
- `/proc/<pid>/cgroup` 已从真实 membership 按当前 cgroup namespace 渲染；
  `CLONE_NEWCGROUP` 要等 Phase 2 的 capability/delegation 授权闭环后再开放。

当前仍缺 CPU、memory 和 I/O controller，`clone3(CLONE_INTO_CGROUP)`、delegation、完整
PID/user/network namespace 以及 OCI lifecycle/security ABI。`mini-oci` 只提供受限的
`run` 路径，不代表 OCI conformance。

容器不是单一内核对象。Linux 通过 namespace、cgroup、capability、LSM 和 VFS 组合
容器；FreeBSD Jail、Solaris Zone 和 Windows Silo 则提供更显式的容器聚合对象。
X-Kernel 采用两者的组合：底层机制保持正交，上层提供可选的显式 `Container` 聚合对象。

## 3. 目标与非目标

### 3.1 目标

1. 为普通 X-Kernel 服务提供不依赖 cgroupfs 的原生分层资源控制 API。
2. 支持 Linux cgroup v2 unified hierarchy，不实现新的 cgroup v1 hierarchy。
3. 保持 namespace、资源控制和安全策略的所有权边界独立。
4. 使 fork、clone、task exit 和 membership 迁移具备可回滚的事务语义。
5. 让 controller 的执行逻辑留在真正拥有资源和热路径的子系统中。
6. 支持 systemd 和 OCI runtime 所需的 cgroup2、namespace 与 procfs 基础语义。
7. 在 SMP 下保持计数不超限、成员关系唯一和锁顺序可审计。
8. 允许先落地 pids controller，再逐步接入 CPU、memory 和 I/O controller。
9. 每类关键状态只有一个事实来源，缓存和索引必须可由事实来源重建。

### 3.2 非目标

1. 第一阶段不实现 cgroup v1 或 v1/v2 hybrid hierarchy。
2. 第一阶段不实现 rootless container；它依赖完整 user namespace、UID/GID mapping、
   idmapped mount 和 delegation 权限闭环。
3. 第一阶段不承诺完整 OCI/Kubernetes 兼容，只建立不会妨碍后续兼容的对象模型。
4. `Container` 不取代 POSIX process group、session、rlimit 或 namespace 对象。
5. 不把所有 controller 状态和算法集中到 `kcgroup`。
6. 不在 CPU controller 调度语义落地前仅暴露“可读写但不生效”的 `cpu.max` 或
   `cpu.weight` 文件。
7. 不为追求“原生容器架构”预先创建没有独立不变量和调用方的 `SecurityDomain`、
   `ControllerRegistry` 或 `kcontainer` crate。

### 3.3 复盘结论

本方案的主线成立，但必须限制抽象规模。设计的首要交付物是正确的 Linux cgroup v2
hierarchy、task membership 和 pids admission，不是一次性建立通用资源管理框架。

复盘后的约束如下：

1. **语义优先于通用化**：内部可以使用 resource domain 术语，但 Phase 1 API 只为已确认
   的 cgroup v2 语义服务；出现第二个非 cgroup 调用方后再提炼通用接口。
2. **task membership 是唯一事实来源**：process-level 信息只是 domain mode 下的线程组
   一致性视图，不单独拥有另一份可变归属。
3. **不提前引入 SecurityDomain**：安全仍由 `Cred`、user namespace、capability 和未来
   LSM owner 负责；容器管理层只保存策略引用或创建参数。
4. **controller 是闭集协调，不是插件框架**：Phase 1 使用显式、typed 的 pids 路径；
   CPU/MM/I/O 各自在实现阶段增加窄接口，不先定义能容纳一切的 trait object。
5. **不建立跨全内核的总锁序**：跨 owner 操作使用 prepare、generation revalidate 和
   commit；禁止在 hierarchy 锁内回调 process、scheduler、MM 或 VFS owner。
6. **创新必须可度量**：RAII reservation、staged publication、typed hot-path handle 和
   capability-gated ABI 都要有失败注入、并发测试或性能基线证明其价值。

## 4. 核心设计原则

### 4.1 隔离、资源和安全正交

进程的执行环境由三个独立维度组成：

```text
Process
  |-- NamespaceSetRef   what the process can see
  |-- ResourceDomainRef what the process can consume
  `-- CredRef           what the process may do
```

创建新 namespace 不自动创建新 resource domain；迁移 cgroup 也不改变 mount、PID
或 network namespace。容器管理层可以按策略同时创建或复用这些对象。安全维度当前由
`kcred::Cred`、user namespace 和 capability 表达，不新增只做聚合的 `SecurityDomain`。

### 4.2 资源 owner 执行策略

`kcgroup` 拥有 hierarchy、membership、配置和聚合统计契约，但不直接操作所有资源：

| Controller | 配置与层级 owner | 执行 owner |
|------------|------------------|------------|
| pids | `kcgroup` | `kprocess` / `ktask` task publication |
| CPU | `kcgroup` | `ksched` / `ktask` |
| memory | `kcgroup` | `memspace` / `anon` / `pagecache` / allocator |
| I/O | `kcgroup` | block request 层和具体设备队列 |
| freezer/kill | `kcgroup` | `kprocess` / `ktask` / signal |
| pressure | 聚合接口在 `kcgroup` | CPU、MM、I/O 事件源 |

controller 通过窄 trait 或 capability 与 resource domain 框架交互，不得让调度器、MM
或驱动依赖 cgroupfs、VFS inode 或 Linux 文本属性名称。

### 4.3 Linux ABI 是 adapter

`cgroup2fs`、`/proc/<pid>/cgroup`、`clone3(CLONE_INTO_CGROUP)`、`setns` 和
`CLONE_NEWCGROUP` 是 Linux ABI adapter。它们将用户输入解析为经过验证的内核操作，
但不拥有 resource domain 状态。

## 5. 总体架构

```text
                         Linux / OCI userspace
                                  |
              +-------------------+--------------------+
              |                   |                    |
          clone/setns          cgroup2fs             procfs
              |                   |                    |
              +------------- Linux ABI adapters -------+
                                  |
                    +-------------+-------------+
                    |                           |
             process/kns                  process/kcgroup
          namespace reference set      unified resource hierarchy
                    |                           |
     +--------------+------------+      +-------+---------+
     |       |       |            |      |       |         |
    VFS     PID     net         cred    pids    CPU     memory/I/O
     |       |       |            |      |       |         |
    kvfs  kidentity knet        kcred  kprocess ksched   mm/block

                    optional userspace/control-plane layer
                                  |
                         ContainerDescriptor
                 namespace plan + resource root + init
```

### 5.1 建议目录

```text
process/kcgroup/
├── src/
│   ├── lib.rs
│   ├── hierarchy.rs
│   ├── node.rs
│   ├── membership.rs
│   ├── namespace.rs
│   ├── transaction.rs
│   ├── permission.rs
│   ├── events.rs
│   └── controller/
│       ├── mod.rs
│       ├── pids.rs
│       ├── cpu.rs
│       ├── memory.rs
│       └── io.rs
└── docs/
    ├── design.md
    └── security.md

fs/filesystems/cgroup2fs/
├── src/
│   ├── lib.rs
│   ├── directory.rs
│   ├── interface_file.rs
│   └── controller_file.rs
└── docs/

```

不在第一阶段创建 `kcontainer`。Linux/OCI 容器身份首先属于用户态 runtime；只有当两个
独立内核调用方都需要共同管理 namespace、resource root 和 init 生命周期，且该组合产生
新的内核不变量时，才引入内核 `Container` 类型或 crate。

### 5.2 分层与依赖方向

设计分为五层，依赖只允许从上向下或通过 owner 定义的窄 capability 反向注入：

| 层 | 组件 | 拥有内容 | 不得拥有 |
|----|------|----------|----------|
| L4 管理层 | OCI runtime、未来 container manager | 容器创建策略和组合生命周期 | scheduler/MM 内部状态 |
| L3 ABI 层 | `ksyscall`、cgroup2fs、procfs | fd/PID/path/text 解析、权限入口、errno | hierarchy 和 controller 状态 |
| L2 领域层 | `kns`、`kcgroup`、`kprocess` | namespace 引用、资源树、task membership、进程事务 | VFS inode、run queue、page/frame、device request |
| L1 resource owner | `ksched/ktask`、MM、block、network | 策略执行、热路径记账、资源生命周期 | cgroupfs 文件名、PID 文本和 namespace path |
| L0 HAL/driver | `khal`、具体 driver | 硬件执行和 completion | process、cgroup 和 OCI 语义 |

允许的关键调用：

```text
L3 parses request -> L2 typed transaction
L2 publishes config/handle -> L1 owner
L1 reports typed counters/events -> L2 aggregation
L1 submits hardware operation -> L0
```

禁止的越层依赖：

- `kcgroup` 不依赖 cgroup2fs、procfs 或 `ksyscall`；
- scheduler/MM/block 不依赖 `kcgroup::ResourceDomain` 大对象，只依赖 owner-specific handle；
- driver 不读取 current process membership；上层在 request 构造时完成 attribution；
- ABI 层不直接修改 controller atomics或成员集合；
- `kns` 不拥有 resource hierarchy，`kcgroup` 不拥有 mount/network/PID namespace；
- `kprocess` 不替 resource owner 执行 CPU、page 或 I/O 策略。

### 5.3 解耦验收问题

每次新增跨 crate 接口必须回答：

1. 哪个 crate 是该状态的唯一 owner？
2. 调用发生在控制面还是热路径，是否允许睡眠和分配？
3. API 传递的是稳定语义对象还是对方内部表示？
4. 调用方失败或被取消时，资源由谁回滚？
5. 移除 cgroup2fs 后，resource owner 是否仍可独立编译和测试？
6. 关闭对应 controller 后，热路径能否退化为零或固定最小开销？

任一问题没有明确答案时，不应以“增加一个 trait”代替边界设计。

## 6. 领域对象

以下类型用于说明语义，最终 Rust API 可按实现约束调整。字段默认私有，调用方只能通过
保持不变量的方法访问。

### 6.1 ResourceHierarchy

```rust,ignore
pub struct ResourceHierarchy {
    id: ResourceHierarchyId,
    root: Arc<ResourceDomain>,
    topology: RwLock<HierarchyTopology>,
}
```

第一阶段全系统只有一个 cgroup v2 hierarchy，但类型不把“全局唯一”写死，从而允许
host-side 测试构造独立 hierarchy。运行内核可通过初始化后的 capability 访问初始 hierarchy。

### 6.2 ResourceDomain

```rust,ignore
pub struct ResourceDomain {
    id: ResourceDomainId,
    parent: Option<Weak<ResourceDomain>>,
    state: Mutex<ResourceDomainState>,
    controllers: ControllerStateSet,
    events: ResourceDomainEvents,
}

struct ResourceDomainState {
    name: String,
    children: BTreeMap<String, Arc<ResourceDomain>>,
    tasks: BTreeSet<TaskIdentity>,
    available_controllers: ControllerMask,
    subtree_control: ControllerMask,
    domain_type: ResourceDomainType,
    lifecycle: ResourceDomainLifecycle,
}
```

`ControllerStateSet` 是随实现阶段扩展的闭集 enum/struct，只包含已经具备执行语义的
controller state。Phase 1 只有 pids state，不能预先放入只保存配置、不执行策略的 CPU、
memory 或 I/O state。

不在节点中缓存完整路径。rename、namespace view root 和未来挂载视图会使缓存路径容易
失效；路径由稳定 parent 链和 name 在需要时生成。procfs/cgroupfs 可在单次读取中缓存结果。

### 6.3 Membership

```rust,ignore
pub struct TaskMembership {
    domain: Arc<ResourceDomain>,
    generation: MembershipGeneration,
}

pub struct ThreadGroupDomainView {
    common_domain: Arc<ResourceDomain>,
    thread_set_generation: ThreadSetGeneration,
}
```

每个 `Thread`/user task 的 `TaskMembership` 是唯一事实来源。默认 domain cgroup 中，
`cgroup.procs` 操作在 process thread-membership transaction 内一次性移动线程组的全部
task。`ThreadGroupDomainView` 只是经过 generation 校验的快速视图，用于证明线程组当前
处于同一 domain；它不是第二份可以独立修改的 membership。

进入 threaded subtree 后，`cgroup.threads` 可以移动单个 task，thread-group view 变为
threaded 状态，不再声称存在唯一 common domain。`cgroup.procs` 和 `cgroup.threads` 从
一开始使用不同 typed request，不能永久折叠成同一个成员列表。

membership 必须是 task 生命周期的一部分，而不是单独用 PID 到 cgroup 的全局
`BTreeMap` 作为事实来源。domain 内的 task set 是反向索引，用于枚举和约束检查；更新时
必须与 task membership 同一事务提交。全局 registry 只用于按 ID 查找和调试。

### 6.4 CgroupNamespace

```rust,ignore
pub struct CgroupNamespace {
    id: CgroupNamespaceId,
    view_root: Arc<ResourceDomain>,
    owner_user_ns: Arc<UserNamespace>,
}
```

cgroup namespace 只定义路径视图根：

- 进程实际 membership 不变；
- namespace 创建时以调用者当前 domain 作为 `view_root`；
- `/proc/<pid>/cgroup` 输出相对于观察者 cgroup namespace root 的路径；
- view root 之外的祖先显示为 namespace 根，不向容器泄露宿主路径；
- namespace 不拥有 hierarchy，也不复制 controller 状态。

### 6.5 Container descriptor

```rust,ignore
pub struct ContainerDescriptor {
    id: ContainerId,
    namespaces: Arc<NsProxy>,
    resource_root: Arc<ResourceDomain>,
    init_process: Weak<Process>,
}
```

该类型只是未来控制面的候选数据结构，不是 Phase 1 内核对象。它不拥有 credentials，
不决定 capability，也不进入 scheduler、page fault、packet RX 或 block completion 热路径。
如果最终只有 OCI runtime 使用这组信息，应留在用户态而不是进入内核。

## 7. Hierarchy 与 controller 语义

### 7.1 Unified hierarchy

只支持 cgroup v2 unified hierarchy：

- 每个 task 在每个 hierarchy 中只有一个 membership；
- controller 由父节点的 `subtree_control` 向子节点开放；
- controller 文件只在 controller 对当前节点可用时出现；
- 删除节点要求没有 child、没有成员、没有 pending transaction 和外部强引用；
- root 是特殊 domain，可承载初始进程，但仍参与祖先统计。

### 7.2 Domain 与 threaded 类型

```rust,ignore
pub enum ResourceDomainType {
    Domain,
    DomainThreaded,
    Threaded,
    DomainInvalid,
}
```

第一阶段仅允许 `Domain`，但状态机必须预留 threaded 模式，不能把 `cgroup.procs` 和
`cgroup.threads` 永久折叠成同一个列表。

当 domain controller 在非 root 节点的 subtree 中启用时，执行 no-internal-process
约束：节点本身不得同时拥有普通进程。启用操作和成员迁移都必须在同一 topology/membership
事务中检查，不能分开检查后再更新。

### 7.3 Controller activation

controller 生命周期：

```text
Unavailable
    | parent enables in subtree_control
    v
Available
    | node enables for children
    v
EnabledForChildren
    | child created / existing child observes propagation
    v
ChildControllerOnline
```

controller 的 `online` 必须在节点向用户态可见前成功。失败时回滚节点创建或
`subtree_control` 修改，不留下只显示部分文件的节点。

## 8. 关键事务

### 8.1 Fork/clone

Linux pids controller 统计 task，包括线程，而不是只统计 process leader。每一次能够创建
新 task 的 `clone` 都必须 charge；新 task 继承调用 task 的 membership，除非
`clone3(CLONE_INTO_CGROUP)` 提供经过验证的目标 domain。

```text
validate clone request
  -> determine inherited resource domain
  -> begin_task_fork(domain)
       -> reserve pids charge from target to root
       -> record pending transaction
  -> allocate PID/TID and process resources
  -> prepare ktask (not runnable)
  -> publish task/process identity
  -> commit task membership, reverse index and charge
  -> activate task
```

建议使用 RAII guard：

```rust,ignore
pub struct TaskForkCharge {
    target: Arc<ResourceDomain>,
    charged_ancestors: SmallVec<[Arc<ResourceDomain>; 8]>,
    state: ForkChargeState,
}
```

未 commit 的 guard 在 Drop 时回滚。commit 后 charge 的释放责任转移到 task lifecycle。
如果 publication 成功而 activation 失败，调用方通过 staged publication rollback 同时撤销
identity、membership 和 charge。

### 8.2 Membership 迁移

cgroup v2 管理操作可以把已有 task 迁入一个已经达到 `pids.max` 的节点；`pids.max`
约束新 task 创建，不阻止管理迁移。因此迁移使用不检查上限的 accounting transfer。

`cgroup.procs` 移动线程组，`cgroup.threads` 移动单个 task。前者必须先在
`kprocess::ThreadMembership` owner 中冻结该线程组的 clone/exit membership 变更，取得稳定
task identity snapshot；不能先扫描线程再逐个移动，否则用户态会观察到半迁移线程组。

迁移流程：

```text
parse visible PID
  -> resolve target in caller namespace
  -> snapshot caller credential
  -> check source/target/common-ancestor permissions
  -> acquire thread-group lifecycle transaction when moving cgroup.procs
  -> snapshot task identities and membership generations
  -> prepare controller transfer without hierarchy locks
  -> lock hierarchy/membership transaction
  -> revalidate task state, generation and source membership
  -> compute source/target path and lowest common ancestor
  -> publish task membership and domain reverse indexes
  -> commit controller transfer
  -> wake cgroup.events observers
```

controller prepare 阶段不得修改不可回滚状态。任一已实现 controller 拒绝迁移时，所有
reservation 由 Drop 回滚，membership 保持不变。commit 后不允许执行可能失败或分配的工作。

### 8.3 Exit

task exit 只删除该 task 的 canonical membership、domain reverse index 和 task charge。
进程最后一个线程退出不再释放另一份“process membership”，因为它不存在；只清理
thread-group view 和真正以 process 为单位的 controller 扩展状态。重复 exit 通知必须被
lifecycle 状态拒绝，不能用 saturating decrement 隐藏 double-uncharge。

```text
task exits
  -> transition membership Active -> Exiting
  -> detach canonical task membership and reverse index
  -> controller task_exit hooks
  -> release pids task charge
  -> if last thread: invalidate thread-group view
  -> mark Detached
```

### 8.4 删除 domain

```text
Active
  -> Removing (blocks new children, migration and fork reservation)
  -> verify no children/members/pending operations
  -> controller offline in reverse order
  -> detach from parent and registry
  -> Dead
```

VFS dentry、namespace `view_root` 或管理 handle 仍持有强引用时，对象可以继续存在，但不能
重新进入 hierarchy。所有按 ID 操作必须识别 `Dead` 并返回 `ENOENT` 或 stale-handle 错误。

## 9. 并发与锁模型

### 9.1 执行上下文

hierarchy 修改、cgroupfs 写入、fork reservation 和迁移运行在普通 task context：

- 允许分配；
- 允许使用 sleepable mutex；
- 不允许从硬中断调用；
- 不允许持有 spinlock、IRQ-disable guard 或 preempt-disable guard 进入；
- controller 热路径使用预先获取的 handle，不遍历 hierarchy、不解析路径。

IRQ 或调度 tick 只能更新固定大小、无分配的 per-CPU accounting 状态，随后由普通任务
上下文聚合。

### 9.2 锁分层

不定义一个要求所有 controller owner 遵守的全内核总锁序。结构控制面内部的锁顺序为：

```text
process thread-group lifecycle transaction (only cgroup.procs/fork/exit)
  -> Hierarchy topology write lock (only topology changes)
    -> membership transaction mutex
      -> ResourceDomain state locks (root-to-leaf, then by stable ID)
```

不得在上述结构锁内调用 provider 回查 `kprocess`，也不得获取调度器 run queue lock、
page-table lock、page-cache lock、VFS inode lock 或 block queue spinlock。调用方在进入
结构事务前取得 stable identity、credential 和 generation snapshot；结构锁内只 revalidate
和交换已准备的 handle。跨 owner 更新使用 prepare/reservation/commit 或延迟事件。

反向方向同样禁止：controller owner 持有 run queue、page 或 request queue 锁时不能进入
hierarchy 控制面。热路径 handle 的替换通过 owner 自己的安全点或原子 generation 发布。

第一阶段可以使用一个 sleepable membership transaction mutex 串行化结构修改，以降低
复杂度。`cgroup.procs` 外层 thread-group transaction 防止并发 clone/exit 改变 task set；
它的 owner 是 `kprocess`，`kcgroup` 接收已冻结的 task snapshot，不反向扫描进程表。不得像
参考实现一样使用全局 `SpinNoIrq` 包住 provider 查询、树遍历和容器分配。

### 9.3 原子计数

只对独立、可单调更新的 hot counter 使用 atomics，例如 per-CPU CPU time、resident bytes
delta。以下状态必须在锁下更新：

- membership 与节点成员集合；
- controller mask 和 domain type；
- max/current 检查与 reservation ledger；
- 多 controller migration transaction。

`pids.max` 的 check-and-charge 可使用 CAS，但 reservation ledger 仍由 transaction owner
管理，确保失败回滚和 double release 可检测。

### 9.4 数据结构复杂度

- child name lookup：`BTreeMap`，O(log n)；
- process/task membership set：`BTreeSet` 或按稳定 ID 的 intrusive/indexed set；
- 不使用 `Vec<Pid>` 做高频 contains/remove；
- 热路径不从 domain 沿 parent 遍历到 root；使用 controller 预计算 ancestry/accounting
  handle，拓扑变更时替换 generation；
- path rendering 只在 procfs/cgroupfs 等控制面路径执行。

## 10. Controller 设计

### 10.1 闭集协调接口

```rust,ignore
pub enum ControllerStateSet {
    Pids(PidsControllerState),
    PidsAndCpu {
        pids: PidsControllerState,
        cpu: CpuControllerConfig,
    },
}
```

示例只表达“已实现 controller 是闭集”，最终可以使用含 optional slot 的私有 struct，
不要求真的采用该 enum。Phase 1 的 fork/migration 事务直接调用 typed pids reservation，
不先设计 `dyn ResourceController`、类型擦除 reservation 或插件注册表。

CPU/MM/I/O 接入时分别定义两个窄边界：控制面配置发布接口和热路径 handle。只有出现真实
第三方 controller 或运行期注册需求后，才评估通用 trait。这样避免一个看似解耦、实际把
所有 owner 都迫使进同一最小公分母协议的框架。

### 10.2 Pids controller

第一阶段完整实现：

- `pids.current`：当前 domain 及受控 descendants 的 task 数；
- `pids.max`：新 task 创建上限；
- `pids.events`：至少包含 `max` 命中次数；
- fork/clone 超限返回 `EAGAIN`；
- 写入低于当前值的新 max 成功，不主动杀死现有 task；
- 迁移不因目标超过 max 而失败；
- thread 创建和退出必须计数。

计数使用 reservation：先 reserve，publication commit 后成为 active charge，失败 Drop
回滚。禁止 saturating decrement；underflow 是内核不变量破坏，应在测试中失败并在生产
内核记录严重错误。

### 10.3 CPU controller

不能把 `cpu.weight` 简单映射为每个任务的 nice。正确的权重语义要求 group scheduling：

```text
per-CPU scheduler
  -> cgroup scheduling entity
       -> task scheduling entities
```

阶段划分：

1. CPU accounting：按 domain 聚合 user/system execution time；
2. `cpu.weight`：调度组参与 EEVDF/CFS 选取；
3. `cpu.max`：带宽池、period timer、throttled queue 和 unthrottle；
4. `cpu.stat`：usage、period、throttled 统计；
5. SMP：per-CPU runtime cache 和 hierarchy-level bandwidth coordination。

只有相应语义完成后才在 cgroup2fs 暴露文件。调度 tick 不获取 hierarchy mutex，也不访问
VFS 或 process registry。

### 10.4 Memory controller

memory controller 必须先定义 charge ownership：

| 内存类型 | 初始 charge owner |
|----------|-------------------|
| anonymous page | 触发分配/fault 的 task resource domain |
| private COW page | 执行写 fault 的 task resource domain |
| shared file page cache | 第一阶段记到实例化页面的 domain，后续评估共享归属策略 |
| page table | 拥有该 `MmSpace` 的 process resource domain |
| kernel stack | task resource domain |
| socket buffer | 创建/发送路径的 network memory domain |

需要的 handle：

```rust,ignore
pub struct MemoryChargeHandle {
    domain_id: ResourceDomainId,
    generation: AccountingGeneration,
}
```

示例 handle 是语义表示，不要求每个 4 KiB page 保存一个 `Arc`。实际布局应使用紧凑、
可验证 generation 的 charge ID，并由 memcg-owned table 或批量 folio/object charge 持有
强生命周期引用，避免显著扩大 page metadata。frame/page 对象持有 charge token 或可恢复
的 charge identity，释放时由 RAII uncharge。
禁止只在 `MmSpace` 粗粒度统计 RSS，因为 page cache、共享页、COW 和内核内存生命周期
不等同于地址空间生命周期。

实施顺序：accounting -> `memory.current` -> `memory.max` admission -> reclaim ->
`memory.high` -> OOM policy -> events/pressure。未建立回收和 OOM 闭环前，不宣称完整
`memory.max` 兼容。

### 10.5 I/O controller

I/O controller 依赖异步 block request/completion 层。同步 `read_block`/`write_block`
接口没有稳定的 request owner，也没有可执行权重或带宽策略的队列点。

```text
filesystem/pagecache
  -> Bio/BlockRequest { resource_domain, bytes, operation }
  -> per-device scheduler
  -> driver submission
  -> IRQ completion
  -> accounting + wake
```

在 request 层落地前不暴露 `io.weight` 或 `io.max`。ramdisk 可以用于接口测试，但不能作为
真实 I/O 调度效果的性能证据。

## 11. Namespace 设计

### 11.1 NsProxy 边界

`kns::NsProxy` 继续只聚合 namespace 引用，不持有 controller、resource domain hierarchy
或容器生命周期。user namespace 仍属于 credentials，task-active PID namespace 仍属于
PID identity/runtime，不为追求字段对称而重复存储。

### 11.2 `CLONE_NEWCGROUP`

以下是 Phase 2 的目标语义。当前 clone/clone3 返回 `ENOSYS`，避免在缺少调用者 user
namespace capability 验证时创建未经授权的 namespace view。

创建新的 cgroup namespace 时：

1. 获取当前进程 membership；
2. 创建 `CgroupNamespace { view_root: current_domain }`；
3. 在 staged process clone 中安装新的 `NsProxy`；
4. 不创建新 resource domain，不迁移进程，不修改 controller；
5. publication 失败时由 `Arc` 自动释放未发布 namespace。

权限检查使用调用者 user namespace 中的 `CAP_SYS_ADMIN` 等价能力，具体 capability
接入必须在 `kcred` 统一实现，不能散落在 clone syscall 中。

### 11.3 PID namespace

cgroupfs 写入的 PID 按写入者可见 PID namespace 解析；内部 membership 始终使用稳定
`PidHandle`/`TaskIdentity`，不保存容易复用的裸 `u32`。读取 `cgroup.procs` 时再映射为
读取者可见 PID，无法在该 PID namespace 中表示的成员不输出。

`echo 0 > cgroup.procs` 表示迁移调用线程所属进程，必须在 Linux ABI adapter 中解析为
稳定 identity，核心 `kcgroup` API 不把 0 当作特殊 PID。

### 11.4 Network namespace

network namespace 不能只拥有一个 ID。最终每个实例应拥有：

- route table；
- interface view 和 loopback；
- socket/port registries；
- neighbor state；
- netlink control state；
- protocol stack instance或明确可分区的共享 stack。

当前 `knet` 使用全局 `Service`、`SocketSet` 和 route state；在这些状态实例化之前，
`CLONE_NEWNET` 必须继续返回显式错误，不能创建只改变 namespace ID 的伪隔离。

## 12. Linux ABI 与 VFS

### 12.1 cgroup2fs

`cgroup2fs` 是独立 VFS adapter。mount 实例持有：

`KFEAT_FS_CGROUP2=y` 时，`fs_boot` 在 initial mount namespace 中自动创建并挂载
`/sys/fs/cgroup`，其 superblock 使用 `kcgroup::CgroupNamespace::initial()` 的 root。
PID 1 的 initial `NsProxy` 复用同一个 namespace。OCI runtime 只在该 hierarchy 下创建、
迁移和清理容器 cgroup，不负责挂载或初始化宿主机全局 hierarchy。

```rust,ignore
pub struct Cgroup2SuperBlock {
    hierarchy: Arc<ResourceHierarchy>,
    mount_root: Arc<ResourceDomain>,
    owner_user_ns: Arc<UserNamespace>,
}
```

目录 inode 只保存稳定 domain handle，不复制节点状态。接口文件在 lookup/readdir 时从
controller registry 生成；不能把所有可能文件硬编码为始终存在。

第一阶段文件：

```text
cgroup.controllers
cgroup.subtree_control
cgroup.procs
cgroup.events
cgroup.stat
cgroup.type
pids.current
pids.max
pids.events
```

写入采用单次命令语义，严格校验 UTF-8/ASCII、长度、token 数量、数值范围和尾随内容。
拒绝 hardlink、symlink、普通文件创建和跨节点 rename。mkdir/rmdir 映射 hierarchy
transaction。

### 12.2 procfs

- `/proc/<pid>/cgroup` 根据目标进程 membership 和观察者 cgroup namespace 生成 `0::<path>`；
- `/proc/<pid>/ns/cgroup` inode identity 来自真实 `CgroupNamespaceId`，不能使用所有进程
  共享的类型常量；
- visibility 先受 PID namespace 和 procfs mount policy 约束；
- 读取过程中进程迁移时允许返回迁移前或迁移后的完整 snapshot，不允许拼接路径。

### 12.3 clone3 与 cgroup fd

`clone3(CLONE_INTO_CGROUP)` 的 `cgroup` 字段是 cgroupfs directory fd：

1. syscall adapter 验证 fd 指向 cgroup2fs directory；
2. 提取稳定 target domain handle；
3. 检查权限和 domain constraints；
4. fork reservation 直接针对 target；
5. child publication 前 commit membership。

核心 process/fork API 接收 `ResourceDomainRef`，不接收 fd 或 VFS inode。

## 13. 权限与安全模型

### 13.1 信任边界

不可信输入包括：

- cgroupfs 路径、目录名和属性文本；
- PID/TID 和 cgroup fd；
- clone/unshare/setns flags；
- namespace-relative路径观察；
- controller limits；
- 并发 fork、exit、迁移和 rmdir。

所有输入在 syscall/VFS adapter 边界解析并转换为 typed request。`kcgroup` 仍需验证
跨调用方可破坏 hierarchy 不变量的语义条件，不能完全信任 adapter。

### 13.2 Delegation

迁移权限至少要求：

- 调用者有权操作目标 task；
- 对目标 cgroup 具有写入权限；
- 对 source 和 target 的共同祖先满足 delegation 约束；
- 不越过 cgroup namespace 和 mount root 可见边界；
- user namespace capability 在 hierarchy owner 的 user namespace 中有效。

第一阶段不支持 rootless delegation 时，应明确只允许初始 user namespace 的特权调用者，
而不是实现部分权限后默认放行。

### 13.3 防止伪隔离

未完成的 namespace 或 controller 必须返回 `ENOSYS`/`EOPNOTSUPP`，不得：

- 接受 `CLONE_NEWNET` 却继续共享全局网络状态；
- 显示 `cpu.max` 但不执行限流；
- 显示 `memory.max` 但只记录数值；
- 为不同 cgroup namespace 输出相同伪 inode；
- 固定返回 `0::/` 掩盖真实 membership。

### 13.4 审计事件

至少为以下操作提供 tracepoint：

- domain create/remove；
- controller enable/disable；
- fork charge success/failure/rollback；
- membership migrate；
- limit hit；
- OOM、throttle、unthrottle；
- permission denial。

日志不得在 scheduler、page fault 或 IRQ hot path 每次事件都同步输出；使用计数器、
tracepoint 或限速日志。

## 14. 失败处理与资源释放

### 14.1 错误映射

内核 domain error 使用语义化 enum，Linux adapter 再映射 errno：

| 语义错误 | Linux errno 示例 |
|----------|------------------|
| 名称或属性输入非法 | `EINVAL` |
| 节点不存在 | `ENOENT` |
| 节点仍有成员 | `EBUSY` |
| 节点仍有子节点 | `ENOTEMPTY` |
| pids fork limit | `EAGAIN` |
| 权限不足 | `EPERM` / `EACCES` |
| controller 未实现 | `EOPNOTSUPP` |
| namespace 功能未实现 | `ENOSYS` |

核心层不直接依赖 Linux errno。

### 14.2 RAII

- fork reservation Drop 回滚未提交 charge；
- memory/page charge 随资源对象 Drop 释放；
- block request completion 或取消恰好释放一次 accounting handle；
- namespace 和 hierarchy handle 使用 `Arc/Weak` 避免 parent-child 强引用环；
- controller online 失败按逆序 offline 已成功的 controller；
- 不依赖调用方手工成对执行 `charge/uncharge`。

### 14.3 OOM 与不可恢复错误

控制面分配失败返回 `ENOMEM` 并保持旧状态。accounting underflow、同一 task 出现在两个
domain、committed charge 无 owner 等属于内核不变量破坏：测试构建应立即失败，生产构建
记录错误、阻止进一步修改并进入可诊断状态，不使用 saturating 操作静默掩盖。

## 15. 与当前 X-Kernel 模块的集成

### 15.1 `process/kcgroup`

从 namespace ID 占位扩展为 hierarchy、membership、namespace view 和 controller
控制面 owner。保持 crate 聚焦，不吸收 VFS、scheduler 或 MM 实现。

### 15.2 `process/kns`

继续持有 `Arc<CgroupNamespace>`，实现 `CLONE_NEWCGROUP` 的 clone/share 选择。不得持有
`ResourceHierarchy` controller 状态。

### 15.3 `process/kprocess`

- 每个 `Thread` 持有 canonical task membership；
- `Process`/`ThreadMembership` 提供冻结线程集合的 transaction，用于原子
  `cgroup.procs` 迁移和 fork/exit 排他；
- domain mode 的 common-domain view 是 generation-validated cache，不是独立状态 owner；
- staged publication 集成 fork reservation；
- exit owner 负责触发一次且仅一次 detach；
- 对外把冻结后的稳定 task snapshot 传给 `kcgroup`，不让 `kcgroup` 反向遍历内部
  process state。

### 15.4 `task/ktask` 与 `task/ksched`

`TaskInner` 不持有整个 `ResourceDomain`；调度热路径持有 CPU controller 的窄
`CpuSchedulingGroupRef`。任务切换时只做固定成本 accounting，不遍历 namespace 或 hierarchy。

### 15.5 MM

分配和 fault API 逐步接受 `MemoryChargeHandle`。charge identity 应在进入可能分配页面的
owner 边界前确定，不能在 allocator 内部通过 `current()` 隐式查询，因为后台 writeback、
reclaim 和异步 I/O 未必运行在原 task 上下文。

### 15.6 VFS 与文件系统

`kvfs` 提供 cgroup2fs 所需的通用 inode/mount 能力，不包含 controller 名称和策略。
cgroup2fs 是 `fs/filesystems` 下的 Linux ABI 文件系统。mount namespace 决定挂载可见性，
cgroup namespace 决定 hierarchy 路径视图，两者不能合并。

### 15.7 网络

network namespace 完成前，Container 可以共享 host network namespace，但管理 API 必须
显式记录这种选择。请求私有 network namespace 时返回不支持，不能暗中降级共享。

## 16. 分阶段实施计划

### Phase 0：语义与测试基线

- 固化本文对象模型和 Linux v2 语义清单；
- 为当前 placeholder 增加明确 capability/status 查询；
- 测试确认未实现 flags 返回错误；
- 建立外部 Starry Test Harness cgroup suite 骨架。

完成标准：没有用户态接口伪装为已实现隔离或控制。

### Phase 1：Hierarchy、membership 与 pids

- 实现 `ResourceHierarchy`、`ResourceDomain` 和 registry；
- canonical task membership 和 domain reverse index；
- thread-group freeze/snapshot transaction；
- fork RAII reservation；
- migration、exit 和 rmdir transaction；
- pids controller；
- 基础 cgroup2fs；
- 真实 `/proc/<pid>/cgroup`。

完成标准：线程计数、fork rollback、线程组原子迁移、迁移超限语义和并发 CAS 测试通过，
并证明没有 task/domain 双重事实来源漂移。

### Phase 2：Cgroup namespace 与 ABI

- `CLONE_NEWCGROUP`；
- namespace-relative procfs path；
- 真实 `/proc/<pid>/ns/cgroup` identity；
- cgroup directory fd；
- `clone3(CLONE_INTO_CGROUP)`；
- 初始 privileged delegation model。

完成标准：不同 cgroup namespace 观察同一 membership 时得到正确的相对视图。

### Phase 3：CPU accounting 与 group scheduling

- per-domain CPU accounting；
- `cpu.stat`；
- EEVDF/CFS group entity；
- `cpu.weight`；
- bandwidth timer、throttle/unthrottle；
- `cpu.max`。

完成标准：多组、多核 workload 的比例和带宽测试达到定义容差，关闭 controller 时无热路径
显著回归。

### Phase 4：Memory controller

- page/frame charge ownership；
- anon、COW、page table 和初步 page cache accounting；
- `memory.current/events/max`；
- reclaim 和 OOM；
- 后续 `memory.high/low/min` 与 pressure。

完成标准：fork/COW、shared mapping、reclaim、OOM 和进程退出后无 charge 泄漏。

### Phase 5：I/O 与完整容器运行时

- 异步 block request/completion；
- I/O attribution 和 controller；
- network namespace 实例化；
- user namespace ID mapping 和 rootless delegation；
- freezer/kill；
- OCI runtime 和 systemd compatibility。

## 17. 测试与验证

### 17.1 Crate 单元测试

使用非全局 hierarchy 实例测试：

- create/remove/lookup；
- controller propagation；
- no-internal-process；
- fork reservation Drop rollback；
- concurrent pids limit；
- LCA migration accounting；
- double commit/double detach detection；
- namespace-relative path；
- dead/stale handle。

### 17.2 Guest regression

按照 `docs/ai/skills/test-harness/SKILL.md` 接入外部 Starry Test Harness，至少覆盖：

- mount cgroup2、mkdir/rmdir 和负路径；
- `cgroup.procs` 迁移与 `echo 0`；
- pthread 对 `pids.current` 的影响；
- `pids.max` fork/clone 返回 `EAGAIN`；
- 降低 max 到 current 以下仍允许写入；
- 迁移可以使 current 超过 max；
- fork publication 失败不泄漏 charge；
- concurrent fork 不突破 max；
- cgroup namespace procfs view；
- permission/delegation denial；
- CPU/memory controller 在各自 phase 的运行效果。

### 17.3 性能与稳定性

基线指标：

- controller 关闭时 fork、context switch、page fault、network 和 block I/O 开销；
- hierarchy 深度对 fork reservation 的影响；
- 大量 domain/process 下控制面操作复杂度；
- SMP concurrent fork/exit/migration；
- resource domain 删除与 namespace/dentry 引用竞态；
- 长时间运行后的 accounting sum 和实际资源释放一致性。

### 17.4 验证命令

实现阶段按平台 defconfig 准备 `.config`，再运行：

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
make UNITTEST=y run
cargo +nightly-2026-03-08 fmt --all
```

涉及 guest Linux ABI 时同时运行 Starry Test Harness 对应 suite。

## 18. 可观测性

提供统一 snapshot：

```rust,ignore
pub struct ResourceDomainSnapshot {
    pub id: ResourceDomainId,
    pub parent_id: Option<ResourceDomainId>,
    pub process_count: usize,
    pub task_count: usize,
    pub enabled_controllers: ControllerMask,
    pub pids: PidsSnapshot,
    pub cpu: Option<CpuSnapshot>,
    pub memory: Option<MemorySnapshot>,
    pub io: Option<IoSnapshot>,
}
```

snapshot 在控制面生成，不向 procfs/cgroupfs 暴露内部锁 guard 或可变对象。watchdog 和
诊断工具使用 snapshot/counter API，不直接遍历私有节点字段。

## 19. 架构创新与验收标准

本设计不以创建新的名词或 crate 作为创新。相对直接复制 Linux 或参考实现，值得保留的
创新集中在以下机制，并分别要求可验证证据。

### 19.1 Staged publication 与资源 reservation 合并

把 X-Kernel 已有的 task/process “prepare -> publish -> activate” 生命周期与 pids、CPU、
memory admission reservation 合并，使资源准入在 task runnable 前完成，失败由 RAII 回滚。

验收证据：对 PID 分配、parent writeback、publication、activation 各点注入失败，最终
membership、controller current 和 process registry 都回到原值。

### 19.2 Canonical membership + generation view

以 task-owned membership 为唯一事实来源，domain reverse index 和 process common-domain
view 都带 generation 校验。这比同时维护 process pointer、PID map 和 node `Vec<Pid>` 更容易
检测漂移，也为未来 threaded mode 保留正确基础。

验收证据：并发 clone/exit/`cgroup.procs`/`cgroup.threads` stress 后，双向索引可以互相
校验，任意 task 恰好属于一个 live domain。

### 19.3 控制面与热路径的 typed handle

控制面使用 hierarchy 和 typed request，scheduler/MM/block 热路径只使用 owner-specific
compact handle。它避免 cgroupfs、PID lookup 和全局树锁渗入 context switch、page fault
和 I/O completion。

验收证据：controller 关闭时热路径无额外树遍历和分配；开启时开销有基线，handle generation
切换不会使用已 offline domain。

### 19.4 Capability-gated ABI exposure

接口文件由已 online controller capability 生成，不使用“先暴露配置文件、以后再让它生效”
的兼容占位。该原则把功能声明与真实执行能力绑定。

验收证据：每个可见 controller 文件都有运行效果测试；未实现功能返回明确错误且不创建
伪 namespace 或伪统计。

### 19.5 创新边界

下列内容不是当前创新点，不能据此扩大实现范围：

- resource domain 只是内部中立术语，不自动成为跨 OS 通用框架；
- `ContainerDescriptor` 只是候选控制面视图，不证明需要内核 Container object；
- Rust trait 不是解耦本身，只有依赖方向、状态 owner 和执行上下文正确时才使用；
- `Arc` 不是生命周期设计的替代品，hot metadata 仍需评估大小和回收成本。

## 20. 已拒绝的替代方案

### 20.1 把所有容器状态放入 `NsProxy`

拒绝。namespace 是可见性视图，资源 hierarchy 和 controller 具有不同生命周期和热路径。

### 20.2 直接复制 Linux `cgroup`/`css_set` 内部结构

拒绝。应兼容用户可见语义，但不需要复制为 C 宏、RCU 和 v1/v2 共存设计形成的内部复杂度。
X-Kernel 应利用 Rust ownership、RAII transaction 和现有 staged publication。

### 20.3 直接移植 `ax-cgroup`

拒绝原样移植，但吸收其 fork guard、LCA migration 和 core/VFS 分离思路。参考实现存在：

- pids 按进程而非 task 计数；
- migration 错误受 `pids.max` 限制；
- CPU 文件可见但调度控制未生效；
- 无 cgroup namespace；
- 全局 `SpinNoIrq` 串行化控制面；
- 节点字段公开，调用方可以绕过事务修改不变量；
- 权限和 delegation 上下文不足。

### 20.4 用 rlimit 代替 resource domain

拒绝。rlimit 是进程属性，不能表达动态 descendants、分层记账、组调度、迁移和统一事件。
rlimit 保留为 POSIX 进程限制，并可与 cgroup 同时生效，取更严格结果。

### 20.5 一开始实现所有 controller

拒绝。没有稳定 resource owner 和排队点时暴露 controller 只会形成无效策略。按 pids、
CPU、memory、I/O 的依赖顺序落地。

## 21. 开放问题

1. 第一阶段是否需要支持 cgroup directory rename，还是明确返回 `EPERM`？
2. page cache charge 第一阶段采用 first-touch、inode owner 还是拆分统计？
3. CPU group scheduling 首先接入 EEVDF，还是定义对 FIFO/RR 的统一 fallback？
4. kernel thread 默认属于 root domain，还是允许显式加入内部不可见 domain？
5. TEE task 与普通 process 是否共享同一 hierarchy，还是需要独立 security/resource root？
6. network buffer 和 socket memory 在 network namespace 与 resource domain 之间如何归属？
7. hierarchy topology 更新是否需要 generation/epoch 机制优化热路径 ancestry handle？
8. `Container` 聚合对象应由内核、用户态 runtime 还是二者共同拥有生命周期？

这些问题在相应 phase 进入实现前必须形成可测试的决策，不能由首个调用点临时决定。
