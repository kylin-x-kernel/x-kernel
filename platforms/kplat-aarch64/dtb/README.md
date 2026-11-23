# Board device trees

`orangepi-5-plus.dts` is the board device tree source used for the RK3588
bring-up. It is the Orange Pi 5 Plus tree with the console UART enabled, and
the blob built from it is passed to the kernel by U-Boot — the kernel does not
build or embed it.

Build the blob next to the source when assembling a board image:

```sh
dtc -I dts -O dtb -o orangepi-5-plus.dtb orangepi-5-plus.dts
```

The tree is one flat file for now. When more RK3588 boards are added, split it
into an SoC `.dtsi` shared by boards and one `.dts` per board, as Linux does;
that layering is not worth its complexity while this is the only board.

It lives next to the platform configuration so a bootable board image can be
assembled without hunting for a matching DT. The blob is a boot asset, not a
build input; changing it does not change the kernel image.
