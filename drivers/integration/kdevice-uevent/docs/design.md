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

A display resize publishes `change` on the primary minor with `HOTPLUG=1` in
the environment. That event is hardware-triggered rather than lifecycle-ordered:
it carries no add/remove meaning, can repeat, and only tells listeners to
re-read the connector. It is the one event this crate publishes outside the
`kdevice` lifecycle callback, through `publish_drm_hotplug`.

Input devices publish the parent `inputN` add before the `eventN` add and use
the reverse order for removal. A DRM node publishes two objects: the primary
minor (`drm/cardN`) first and the render minor (`drm/renderD{128+N}`) second on
add, in the reverse order on remove. The render path, `DEVNAME` and `uevent`
contents are derived from `kdevice::DeviceNode`, so the published event and the
sysfs attribute for the same node cannot disagree. Sysfs uses the exported formatter for explicit
user-triggered events, so automatic and manual messages cannot disagree about
paths or properties. The crate also exports the Linux capability-bitmap
formatter so sysfs attributes and uevent properties share the same word order.

## Boundaries

`kdevice` remains OS-neutral and knows only an opaque lifecycle callback. This
crate is host integration glue and may call `knet`. It does not discover
devices, own filesystem objects, decide device identity, or accept raw user
input.
