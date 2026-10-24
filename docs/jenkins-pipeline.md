# Jenkins 流水线运行契约

流水线顶层是 `agent none`，按架构分到不同节点，而不是绑在一个顶层 Docker Agent 上。

| 角色 | 调度 | 做什么 |
| --- | --- | --- |
| Prepare / Post | 浮动节点池，标签 `xkernel-agent && docker && vhost-vsock && container-vsock` | checkout、安装 rustfmt、`cargo fmt --check`；Post 汇总评论与 Check Run |
| `x86_64` leg | 固定节点 `xkernel-amd64-01` | 该架构的工具链、Build、Run、输出收集 |
| `aarch64` leg | 固定节点 `xkernel-arm64-01` | 同上 |
| `riscv64` leg | 与 Prepare 相同的浮动节点池 | 同上 |

浮动池里的节点必须满足本文契约；不满足契约的节点不得打上
`xkernel-agent && docker && vhost-vsock && container-vsock`。
`xkernel-amd64-01` 与 `xkernel-arm64-01` 按节点名调度，同样必须满足身份、设备和 volume 契约。
节点名与标签以 `Jenkinsfile` 里的 `nativeGuestAgentLabels()`、`floatingGuestAgentLabel()` 为准。

## Agent 身份与目录

宿主机 Jenkins 用户和 builder 容器统一使用数字身份 `1000:1000`。这是
workspace 能由宿主 Agent 与容器共同读写、且流水线完全不需要 `chown` 的前提。
节点接入前必须验证：

```bash
id jenkins
# uid=1000(jenkins) gid=1000(jenkins)
```

每个 Jenkins Controller 使用独立 Remote root，禁止两个 Controller 共用
workspace：

```text
/data/jenkins/agents/legacy/<node>
/data/jenkins/agents/openkylin/<node>
```

Remote root 只在节点创建时初始化一次，所有者为 `jenkins:jenkins`，模式为
`0750`。Jenkinsfile 不引用这些宿主路径，只使用 Jenkins 分配的 `$WORKSPACE`。

Jenkins Controller 不承接构建任务，executor 数必须为 `0`。
浮动池节点（Prepare、Post、riscv64）至少配置以下 label：

```text
xkernel-agent docker vhost-vsock container-vsock
```

`xkernel-amd64-01` 与 `xkernel-arm64-01` 按节点名调度，不依赖上述能力标签。

## 容器与设备

流水线使用的 builder 镜像以 `Jenkinsfile` 中 `ciBuilderImage()` 的返回值
为准（Prepare 与各 arch leg 经 `withFloatingCiDocker` / `withArchCiAgent` 拉取）。
镜像的 Dockerfile 在仓库内的 `scripts/ci/docker/` 维护，构建、
检查和发布方法见该目录的 README。OCI index 只包含原生 `linux/amd64` 和
`linux/arm64` 镜像。镜像默认用户是
`jenkins`（`1000:1000`）；设备组 `36` 由流水线在运行时通过 `--group-add 36`
加入，镜像内不预置。

宿主机必须将 `/dev/kvm` 和 `/dev/vhost-vsock` 持久设置为 group `36`、mode
`0660`。流水线只显式传入这两个设备并执行 `--group-add 36`，不使用
`--privileged`、root 用户或 Docker Socket。

## Workspace

Prepare、每个 arch leg 和 Post 各有独立 workspace 与独立 builder 容器。
Prepare 在 `.ci/source` checkout 之后，把源码和 Gitee Check Run 状态 stash 给各 leg；
leg 在本节点 unstash，再在 `.ci/work` 里并行 Build 和 Run。
同一架构的 Build 与 Run 留在该 leg 的同一个容器里。
阶段日志和轻量覆盖率文件经 per-arch stash 回到 Post；覆盖率 HTML 与文档在 leg 上直接 `archiveArtifacts`。

每个 workspace 的目录结构为：

```text
$WORKSPACE/
├── .ci/
│   ├── source/       # 一次 checkout 后的源码快照
│   ├── work/         # 各并行阶段的独立工作目录
│   ├── stage-logs/   # Gitee Check Run 阶段日志
│   └── gitee/        # Check Run ID 与临时 manifest
├── artifacts/
│   ├── coverage/
│   └── docs/
└── ci-summary.md
```

宿主 Agent 与容器使用相同 UID/GID，因此 checkout、并行构建、归档和
`cleanWs` 都直接以同一身份运行。流水线没有递归 `chown`、宽权限
`chmod`、root fallback 或 `safe.directory=*` 兼容逻辑。如果这里出现权限
错误，应修复节点身份、Remote root 或 volume 初始化，不能在 Jenkinsfile 中
增加权限修补。

## 构建输出与持久缓存

`/xkernel-target` 是当前容器独占的匿名 Docker volume。它只在同一 arch leg 的
Build 与 Run 之间复用（boot、链接和 Cargo 中间产物），不跨架构、不跨 Prepare/Post，
也不跨 Jenkins Build。各节点上的 named volume 也彼此独立：同名 volume 在不同节点上不是同一份数据。

TEE storage tests use `/xkernel-target/libutee-<arch>` as a source/build cache
within this container only. Each architecture has one script invocation and a
separate directory; it is not a cross-job cache. Synchronization tries fetch
and reset first, then clones into a temporary sibling on failure. A completed
clone replaces an invalid existing tree so later calls can reuse it; failed
clones leave the old tree intact and remove their temporary directory. Three
attempts use 5/10-second backoff, with per-operation retry and publication logs.


以下节点本地 named volume 可以跨 Build 复用，volume 名称以 `Jenkinsfile` 中的
`--mount` 参数为准：

| 容器路径 | 内容 |
| --- | --- |
| `/xkernel-cache/cargo` | Cargo registry、git 与元数据缓存 |
| `/usr/local/rustup` | 固定版本 Rust toolchain、下载缓存和安装临时文件 |
| `/xkernel-cache/rootfs` | 按架构校验后的 rootfs 模板 |

镜像中的这些挂载点已经是 `1000:1000`。Docker 第一次创建 volume 时会把镜像中
该目录的内容连同所有权复制进去，不需要预先进入 volume 修权限。已有的 volume
不会再从镜像更新。volume 名称带版本后缀，需要让节点用新镜像重新初始化某个
volume 时，递增 `Jenkinsfile` 中对应的后缀。Cargo 工具本体位于只读的
`/usr/local/cargo`，与可变 Cargo 缓存分离。Cargo 的镜像源配置放在镜像的
`/.cargo/config.toml`，不在缓存 volume 里，随镜像更新生效。

镜像不内置项目 Rust toolchain。Prepare 只运行
`scripts/ci/check_build_environment.sh --fmt`，在浮动节点的 Rustup volume 里安装
`AUX_RUST_TOOLCHAIN` 的 `rustfmt`，供随后的 `cargo fmt --check`。
各 leg 再运行 `scripts/ci/check_build_environment.sh --leg <arch>`，在该节点安装
`rust-toolchain.toml` 声明的全部 target 及额外的 musl target，避免后续 Cargo
调用触发隐式安装，与共享缓存的构建并发修改工具链。`x86_64` / `aarch64`
另外安装 nightly 与 host musl，供 TEE。不带参数调用同一脚本可用于手工预热。
安装通过 volume 内文件锁串行化。rootfs 下载同样按架构加锁，并通过临时文件、完整性检查和原子替换避免并发
读到半成品。

Rustup 必须挂载整个 `/usr/local/rustup`，使 `tmp` 和 `toolchains` 位于同一
文件系统，支持安装时的原子改名。当前使用新卷 `xkernel-rustup-home-v5`；
主线此前使用的 `xkernel-rustup-toolchains-v4`，以及本 PR 早期验证中使用的
`xkernel-rustup-toolchains-v5`，都只挂载 toolchains 子目录，与当前 home 卷不同。
这两个旧卷均不迁入新卷，保留供排查；当前流水线不再引用它们。

整个 home 挂载也会持久化 Rustup settings。镜像的 `rustup default none` 仅是
新卷初始状态；首次运行时安装可能写入默认 channel，后续容器会保留该设置。
项目构建通过仓库 `rust-toolchain.toml` 或显式 `+toolchain` 选择版本，
不依赖这个历史默认值。Setup 在源码目录执行的 active-toolchain 探测同样受
仓库配置约束，不等同于读取 settings 中的全局默认值。

Dylint 检查所需的工具都在镜像中：`cargo-dylint`、`dylint-link` 与其他 Cargo
工具一起位于 `/usr/local/cargo/bin`；lint 库使用的 nightly toolchain 及其组件
预装在 `/usr/local/rustup/toolchains` 下，新建的 Rustup volume 会带上它；与之
匹配的 Dylint driver 预先构建在镜像环境变量 `DYLINT_DRIVER_PATH` 指向的目录，
该目录不在任何缓存 volume 中。版本以 `scripts/ci/docker/` 中当前镜像版本的
Dockerfile 为准。镜像改动预装 toolchain 时，同时递增 Rustup volume 的后缀，
否则已有节点看不到新预装的内容。

`Lint Check: dylint kplat-aarch64` 在 aarch64 leg 上运行，
`Lint Check: dylint kplat-x86_64` 在 x86_64 leg 上运行，两条腿彼此并行。
lint 只检查当前配置实际编译的代码，因此两边分别覆盖对应架构的专属代码。
每个检查在该 leg 的独立 workspace 中使用对应平台的 `qemu_defconfig` 执行
`make defconfig`、`make dylint_test` 和 `make dylint`，并使用独立的
`/xkernel-target/dylint-<platform>` 构建输出目录。
aarch64 分支在保留上述普通配置检查的同时，额外依次执行
`make UNITTEST=y clippy` 和 `make UNITTEST=y dylint`，覆盖 Cargo `unittest`
feature 选入的测试专用 optional 依赖。仅传 `--cfg unittest` 不会启用这些依赖；
x86_64 分支没有这两次额外扫描，因此该测试依赖图的 lint 覆盖目前仅在 aarch64。
aarch64 分支完成这两次扫描后，再向 `.config` 追加 `KFEAT_LOCK_STAT=y`，执行 `make olddefconfig`
并确认该选项生效后，在同一构建输出目录再运行一次 `make dylint`。只有打开锁统计时，
`static_lock!` 才展开为注册锁类的过程宏，锁统计相关代码才参与编译；其他 CI 配置都
不打开该选项。检查结果与其他并行阶段一起进入 Gitee Check Run。

`Jenkinsfile` 不覆盖 `DYLINT_DRIVER_PATH`，Dylint 直接使用镜像预构建的 driver。
每个 Dylint 分支开始时运行 `scripts/ci/check_dylint_environment.sh`，只核对
Dylint 工具、lint toolchain 及其组件和 driver 是否与仓库固定的版本一致
（`xtask/xkmake/src/hygiene.rs` 的 `DYLINT_VERSION` 与
`tools/lints/rust-toolchain.toml`），不一致时报错，不在节点上现场安装。通用的 Prepare rustfmt 安装不检查 Dylint 工具，不跑 Dylint 的流水线可以使用不含这些工具的镜像。
修改上述两处版本之前，必须先发布新的镜像版本，并在同一改动中切换 `Jenkinsfile`
的镜像引用、递增 Rustup volume 后缀。lint 库自身依赖的 crate 不随镜像 vendor，和内核
crate 一样经 Cargo 镜像源下载并缓存在 Cargo volume 中。

guest case target、kernel target、覆盖率与测试制品不能放入共享缓存。

## Jenkins Job

Pipeline Job 使用仓库中的 `Jenkinsfile`：

- Refspec：`+refs/pull/*:refs/pull/*`
- Branch Specifier：`pull/${giteePullRequestIid}/MERGE`
- Script Path：`Jenkinsfile`
- Reference repository：留空

Gitee 触发器只处理 PR 创建、源分支更新和 `rerun` 评论；关闭普通 Push 与删除
ref 构建，并启用 `CI_SKIP`。同一 PR 更新后应取消仍在运行的旧 Build，避免旧
提交继续占用 Agent 或回写过期状态。

## 凭据边界

X-Kernel、fork 和 starry-test-harness 均以公开仓库匿名拉取。Gitee Token
只用于更新 PR 评论、Check Run 与测试状态，不写入 Git remote，也不插入 shell
命令。

同一架构的 Build 与 Run 在该 leg 的同一个 builder 容器内完成，Run 使用
Build 留在该容器 `/xkernel-target` 里的 boot、链接和 Cargo 中间产物。
跨节点传递的是 stash 的源码、阶段日志、轻量覆盖率文件，以及 leg 上归档的
覆盖率 HTML 与文档。leg 结束时的 `Collect: <arch> outputs` 把 stash 和归档
结果写入 `ciResults` 与对应的 Gitee Check Run；收集失败时构建失败，PR 评论会标出该阶段。
