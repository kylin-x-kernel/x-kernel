# Jenkins 流水线运行契约

`Jenkinsfile` 按能力标签调度，不绑定节点名。任何承接 X-Kernel CI 的节点都必须
满足本文契约；不满足契约的节点不得配置
`xkernel-agent && docker && vhost-vsock`。

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

Jenkins Controller 不承接构建任务，executor 数必须为 `0`。节点至少配置以下
label：

```text
xkernel-agent docker vhost-vsock
```

## 容器与设备

流水线固定使用：

```text
yeanwang/x-kernel-builder:v2.0.0-rc.2
sha256:b5e1c4fbf92b2e653fce0a8a4a171c893e960a782ae2288d20b10fd575ade09f
```

OCI index 只包含原生 `linux/amd64` 和 `linux/arm64` 镜像。镜像默认用户是
`jenkins`（`1000:1000`），设备补充组 GID 是 `36`。

宿主机必须将 `/dev/kvm` 和 `/dev/vhost-vsock` 持久设置为 group `36`、mode
`0660`。流水线只显式传入这两个设备并执行 `--group-add 36`，不使用
`--privileged`、root 用户或 Docker Socket。

## Workspace

流水线在一个顶层 Docker Agent 中执行，目录结构为：

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

`/xkernel-target` 是当前 Pipeline 容器独占的匿名 Docker volume，保存同一次
Build 与 Run 阶段之间需要连续使用的 Cargo、boot 和 kernel 产物。它不跨
Jenkins Build 共享。

以下节点本地 named volume 可以跨 Build 复用：

| Volume | 容器路径 | 内容 |
| --- | --- | --- |
| `xkernel-cargo-home-v2` | `/xkernel-cache/cargo` | Cargo registry、git 与元数据缓存 |
| `xkernel-rustup-toolchains-v2` | `/usr/local/rustup/toolchains` | 固定版本 Rust toolchain |
| `xkernel-rootfs-cache-v2` | `/xkernel-cache/rootfs` | 按架构校验后的 rootfs 模板 |

镜像中的这些挂载点已经是 `1000:1000`。Docker 第一次创建 volume 时会继承
正确所有权，不需要预先进入 volume 修权限。Cargo 工具本体位于只读的
`/usr/local/cargo`，与可变 Cargo 缓存分离。

镜像不内置项目 Rust toolchain。首次在新节点运行时，
`scripts/ci/check_build_environment.sh` 根据 `rust-toolchain.toml` 安装固定
版本到 Rustup volume；后续构建复用它。安装通过 volume 内文件锁串行化。
rootfs 下载同样按架构加锁，并通过临时文件、完整性检查和原子替换避免并发
读到半成品。

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

当前 Build 与 Run 阶段仍在同一个顶层 Docker Agent 内完成，因为 Run 依赖
Build 阶段留在 `/xkernel-target` 的 boot、链接及 Cargo 中间产物。只有这些
内容形成可归档、可校验的制品包后，才适合将两个阶段调度到不同节点。
