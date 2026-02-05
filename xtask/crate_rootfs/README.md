# crate_rootfs

用于创建 ext4 根文件系统镜像，并按需拷贝文件。

## 用法

```bash
cargo run -p crate_rootfs --release -- \
  --image /path/to/disk.img \
  --size-bytes 64M \
  --copy /host/path/app1:/tee/app1 \
  --copy /host/path/app2:/tee/app2
```

参数说明：

- `--image`：输出镜像路径。
- `--size-bytes`：镜像大小（支持如 `64M`、`1G`）。
- `--copy`：拷贝条目，格式为 `SRC:DEST`，可重复传入多次。

提示：

- `--copy` 可省略（生成空镜像）。
- 目标路径以镜像内路径为准，例如 `/tee/app1`。
