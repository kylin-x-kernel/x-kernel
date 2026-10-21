# CI builder image

Jenkins 的所有阶段都在这个镜像里运行。镜像需要满足的运行契约见
[docs/jenkins-pipeline.md](../../../docs/jenkins-pipeline.md)。

## 目录与版本

```text
scripts/ci/docker/
├── build.sh                  # 构建一个版本
└── <os>_<release>/           # 一个基础系统一个目录，同时作为构建上下文
    ├── Dockerfile_v<X.Y>     # 一个版本一个文件
    └── files/                # 构建时 COPY 进镜像的文件
```

- 镜像标签取自文件名里的版本号：`<os>_<release>/Dockerfile_v<X.Y>` 对应
  `<repo>:v<X.Y>`。版本号在所有系统目录之间唯一。
- **完整版本**从按 digest 固定的基础镜像开始构建全部内容。基础镜像、工具和
  需要固定的软件包版本都写在 Dockerfile 开头的 `ARG` 里，这是版本号的唯一出处。
- 固定到精确版本号的 apt 包，在上游撤下旧版本后会导致构建失败，这时改 `ARG`
  并发布新版本。未固定的 apt 包跟随发行版的安全更新。
- **增量版本**新建一个 `Dockerfile_v<X.Y>`，开头写：

  ```dockerfile
  ARG BASE_IMAGE=<上一版镜像>@sha256:<digest>
  FROM ${BASE_IMAGE}
  ```

  文件里只写这一版的改动，需要编译的工具可以照完整版本的做法放在单独的构建
  阶段里。`BASE_IMAGE` 的默认值是基础镜像的唯一配置，必须是上一版已发布的
  multi-arch 镜像的 digest 引用；`build.sh` 拒绝没有固定 digest 的增量版本。
- 更换基础系统，或增量层积累太多时，新建一个完整版本（换系统时同时新建目录），
  主版本号加一。
- 已发布版本的文件不再修改，要改就发一个新版本。

## 镜像内容与缓存 volume

流水线把几个节点本地 named volume 挂到镜像的 `/xkernel-cache/*` 和
整个 `/usr/local/rustup` 上（见 `Jenkinsfile`）。镜像放在这些目录下的
内容只会复制进新建的 volume，已有 volume 看不到：

- `/usr/local/rustup` 中预装的 toolchain、settings 或其他需随镜像更新的内容
  有变化，或缓存损坏需要重新初始化时，递增 `Jenkinsfile` 中 Rustup volume
  名称的后缀，让节点新建 volume。整个 Rustup home（包括 downloads、tmp 和
  settings）与 toolchains 一起持久化，已有 volume 不会自动获得镜像更新。
- 每个容器都必须看到的内容（例如预先构建的 Dylint driver）放在这些挂载点
  之外。

镜像的 `rustup default none` 是新卷的初始状态；运行时安装工具链可能设置并
持久化默认 channel。项目构建按仓库 `rust-toolchain.toml` 或显式 `+toolchain`
选择版本，不依赖节点默认值。卷布局与旧卷说明见
[流水线运行契约](../../../docs/jenkins-pipeline.md)。

## 构建与检查

构建需要 `docker buildx`。在 x86_64 主机上构建 arm64 镜像，还需要 QEMU
binfmt（Arch Linux 上是 `docker-buildx`、`qemu-user-static` 和
`qemu-user-static-binfmt`）。先创建一个支持多平台的构建器：

```sh
docker buildx create --name xkernel --driver docker-container --use
```

本地构建时每次只构建一个平台，结果加载到本机。两个平台用不同的 repo 名，
否则后构建的会覆盖先构建的标签：

```sh
DOCKERFILE=scripts/ci/docker/<os>_<release>/Dockerfile_v<X.Y>
scripts/ci/docker/build.sh "$DOCKERFILE" --repo xk-local-amd64
scripts/ci/docker/build.sh "$DOCKERFILE" --repo xk-local-arm64 --platform linux/arm64
```

增量版本从 `BASE_IMAGE` 固定的上一版构建。buildx 的 `docker-container` 构建器
从镜像仓库拉取上一版，不读取本机的镜像。

在 QEMU 模拟下构建 arm64 比原生慢得多。buildx 构建器会保留缓存，之后的构建
只重做改动过的阶段。

修改 Dockerfile 或脚本后，运行：

```sh
docker buildx build --check -f "$DOCKERFILE" "$(dirname "$DOCKERFILE")"
shellcheck scripts/ci/docker/*.sh
```

## 发布

1. 两个平台的本地镜像都满足运行契约，并能完成内核构建。
2. 推送 multi-arch 镜像，记下输出的 digest：

   ```sh
   scripts/ci/docker/build.sh "$DOCKERFILE" --repo <registry>/x-kernel-builder --push
   ```

3. CI 节点无法访问 Docker Hub，在每个节点上用 `docker load` 预置对应平台的
   镜像。`docker load` 只保留 tag，不保留 manifest digest，所以流水线按
   `<repo>:<tag>` 引用镜像；digest 记在下一个增量版本的文件开头和 PR 描述里。
4. 把镜像引用换成 `<repo>:<tag>`：
   - 本仓库 `Jenkinsfile` 顶部的 `agent { docker { image ... } }`；
   - starry-test-harness 仓库 `Jenkinsfile` 中的 `dailyBuilderImage()`。

   PR 的 CI 读取的是 PR 合并引用里的 `Jenkinsfile`，所以换镜像的 PR 自己的 CI
   就会用新镜像运行。
