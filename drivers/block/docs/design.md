# block — 设计文档

## 定位

`block` 是 X-Kernel 的 block core。它定义 driver-private I/O operations、已发布的
`Gendisk`、按 `dev_t` 标识的 `BlockDevice`，并拥有唯一 resident block-device registry。

## Linux 对象映射

| Linux | X-Kernel | 所有权 |
|---|---|---|
| `struct block_device_operations` | `BlockDeviceOperations` | driver/backend algorithm、open mode callback |
| `struct gendisk` | `Gendisk` | disk name、major/minor range、state、operations |
| `struct block_device` | `BlockDevice` | `dev_t`、disk view、capacity |
| `bd_holder` / exclusive `bdev_open` | `BlockDeviceClaim` | exclusive holder lifetime |
| `add_disk` / `del_gendisk` | 同名函数 | 显式 publish/unpublish |
| `blkdev_get_no_open` | `lookup_block_device` | canonical `dev_t` lookup |
| `set_capacity` | `BlockDevice::set_capacity` | 可变介质容量发布 |
| `set_disk_ro` / `get_disk_ro` | `BlockDevice::set_disk_read_only` / `is_read_only` | canonical disk state |

`BlockDeviceOperations` 不继承通用 `Device`，因为 disk identity 属于 `Gendisk`；backend
只表达 I/O 和 Linux block-device operations 层的 open/release/ioctl。`Gendisk` 组合该
algorithm object，`BlockDevice` 再组合 `Gendisk`，不复制 driver identity。
`BlockOpenMode` 对应 Linux `blk_mode_t`，从 KVFS 的 opened-file mode 传入 open/ioctl，
不会在 loop 等具体驱动中另建一套打开语义。

当前只创建 whole-disk `part0`。`BlockDevice` 已用 `start_block + capacity` 表达 view
边界，后续 partition scan 可以发布更多 view，而不引入另一种设备对象。

## 发布与查找

driver probe 构造 `Gendisk` 并经 block class lifecycle 调用 `add_disk`。发布时校验 major
非零和同 major 的完整 minor range 不重叠，然后创建 part0 并按 `DeviceNumber` 放入唯一
registry。devfs、KVFS block-special open、filesystem mount 和 boot root selection 都读取
该 registry，不各自维护映射。

`del_gendisk` 按 part0 `dev_t` 取得 owning disk，并删除所有指向它的 device views。已有
`Arc<BlockDevice>` 维持对象内存生命周期；新 lookup 不再取得已撤销对象。相同 `dev_t`
随后重新发布会产生新的 canonical `BlockDevice` 对象，使用者以对象 identity 区分介质代际。

`BlockDevice::claim_exclusive()` 返回 RAII `BlockDeviceClaim`，对应 Linux block holder
所有权。一个 canonical device 同时只允许一个 holder；filesystem superblock 直接持有该
token，并在初始化失败或 final shutdown 进入 dead 后释放。因此不同 filesystem instance
不能同时拥有同一介质，且 block core 不需要了解 VFS 或文件系统类型。

## I/O 边界

`BlockDevice` 在委托 backend 前校验 buffer 是 block-size 整数倍、完整 I/O extent 不越过
capacity，并对 block offset 做 checked arithmetic。`Gendisk::new` 要求 block size 非零且
初始容量的字节乘法可表示；`set_capacity` 对每次动态更新重复该边界校验。read-only 是
`Gendisk` 的 canonical state，`BlockDevice::write_block` 在进入 backend 前统一拒绝写入。

KVFS 负责 Linux `blkdev_read_iter` / `blkdev_write_iter` 对应的字节适配：完整对齐块直接
传递调用方 buffer，首尾 partial block 复用单个 read-modify-write scratch buffer。普通
write 不等价于 durability barrier；只有显式 `fsync` 才调用 backend `flush`。
