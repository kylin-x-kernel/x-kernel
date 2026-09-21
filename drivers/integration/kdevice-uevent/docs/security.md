# kdevice-uevent — Security and reliability

## Trust boundary

All identity and capability fields come from immutable `kdevice::DeviceNode`
snapshots. Sysfs validates user-supplied action names before calling the shared
publisher. Driver text used in NUL-delimited messages has control characters
replaced with spaces so it cannot inject fields.

## Invariants

- The lifecycle publisher is installed once, before the first node.
- Add is sent only after filesystem projections observe the node.
- Remove is sent only after projections reject new attribute operations and
  wait for in-flight operations, but before they delete the path.
- Automatic and user-triggered events share one path/property formatter.
- Input parent/leaf event ordering follows Linux device hierarchy order.

## Failure handling

Netlink queues are bounded and may drop a message for a slow listener. The
current sysfs tree remains authoritative and listeners can rescan it. A late or
duplicate publisher installation is a boot integration error and fails during
initialization.

## Unsafe code

This crate contains no unsafe code.
