# lkvm user app

This directory installs an externally built `kvmtool`/`lkvm` binary into an
X-Kernel disk image.

The `uapp.toml` manifest does not build kvmtool from source. Its prepare step
expects this host-side file to already exist and be executable:

```text
/tmp/opencode/kvmtool/lkvm-static
```

It then copies that binary to:

```text
uapps/lkvm/out/lkvm
```

and injects it into the guest image as:

```text
/usr/local/bin/lkvm
```

## Source

Use upstream kvmtool and dtc/libfdt sources:

```bash
mkdir -p /tmp/opencode
git clone https://git.kernel.org/pub/scm/linux/kernel/git/will/kvmtool.git /tmp/opencode/kvmtool
git clone https://git.kernel.org/pub/scm/utils/dtc/dtc.git /tmp/opencode/dtc
```

If those directories already exist, update or reuse them as needed.

## Build libfdt

kvmtool needs `libfdt` when building with FDT support. Build an AArch64 static
libfdt first:

```bash
make -C /tmp/opencode/dtc/libfdt CC=aarch64-linux-musl-gcc AR=aarch64-linux-musl-ar libfdt.a
```

## Build lkvm

Build a static AArch64 lkvm binary:

```bash
make -C /tmp/opencode/kvmtool \
  ARCH=arm64 \
  CROSS_COMPILE=aarch64-linux-musl- \
  LIBFDT_DIR=/tmp/opencode/dtc/libfdt \
  lkvm-static
```

The expected output is:

```text
/tmp/opencode/kvmtool/lkvm-static
```

## Inject into disk image

From the repository root, inject only this app into the AArch64 disk image:

```bash
cargo uapp install \
  --disk-img disk_aarch64.img \
  --select lkvm \
  --arch aarch64 \
  --cross-compile aarch64-linux-musl-
```

This runs the manifest prepare commands and writes:

```text
disk_aarch64.img:/usr/local/bin/lkvm
```

## Run inside X-Kernel

After booting X-Kernel with the updated image, run lkvm from the host shell:

```sh
/usr/local/bin/lkvm run -i /boot/initrd.gz -p "rdinit=/init"
```

The guest kernel image is expected at:

```text
/boot/vmlinuz-10.0.0
```

The initramfs is expected at:

```text
/boot/initrd.gz
```

## Notes

- Do not patch kvmtool as an X-Kernel compatibility workaround. Fix missing or
  incorrect behavior in X-Kernel's `/dev/kvm` implementation instead.
- If kvmtool objects were previously built for another architecture, run
  `make -C /tmp/opencode/kvmtool clean` before rebuilding with the AArch64
  command above.
- `LIBFDT_DIR=/tmp/opencode/dtc/libfdt` is required when kvmtool cannot find a
  suitable host-installed libfdt for the target toolchain.
