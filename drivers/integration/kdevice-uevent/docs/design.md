# kdevice-uevent — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose

`kdevice-uevent` is the X-Kernel adapter between portable `kdevice` node
lifecycle notifications and `NETLINK_KOBJECT_UEVENT`. It also owns the common
Linux path and property formatter used by sysfs `uevent` attributes.

## Lifecycle ordering

Boot initializes the sysfs projection and this publisher before driver
discovery. `kdevice` then enforces one order for every node:

```text
projection add -> add uevent
mark removing -> projection quiesce -> remove uevent -> projection delete
```

The publisher never scans or backfills an existing registry. Its installation
fails if discovery already published a node, which makes incorrect boot order
visible instead of creating synthetic add events at first sysfs mount.

Input devices publish the parent `inputN` add before the `eventN` add and use
the reverse order for removal. Sysfs uses the exported formatter for explicit
user-triggered events, so automatic and manual messages cannot disagree about
paths or properties. The crate also exports the Linux capability-bitmap
formatter so sysfs attributes and uevent properties share the same word order.

## Boundaries

`kdevice` remains OS-neutral and knows only an opaque lifecycle callback. This
crate is host integration glue and may call `knet`. It does not discover
devices, own filesystem objects, decide device identity, or accept raw user
input.
