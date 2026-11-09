# Driver Framework Core

`drivers/base/` groups the driver framework core: crates that own device/class
registries and runtime state, plus `driver-os-api`, which supplies host
capability contracts without owning device registries. It is a directory, not a
new aggregate crate: each crate keeps its own manifest, features and dependency
set, and no shared `base` dependency is introduced.

| Crate | Owns | Does not own |
| --- | --- | --- |
| `kdevice` | Device descriptors, objects and relations; bus/device/driver registries; binding, removal and adoption orchestration; device-managed cleanup registration; neutral fwnode association; device-node identity. | DT/AML parsing, controller-specific mapping, hardware transport operations. |
| `kclass` | Typed runtime capability publication, lookup and availability notification; the bridge from `kdevice` activation events to class registries; existing revocation wiring. | Device discovery, driver selection, hardware shutdown. |
| `driver-os-api` (`os-api/`) | Host capability contracts and ownership guards that drivers require: MMIO, IRQ, DMA, CPU memory and time. | Device registries, device inventory, driver matching, concrete kernel implementation. |

`driver_base`, the class contracts (`block`, `net`, `char_driver`, `display`,
`input`, `vsock`, `hwrng`) and the firmware crates remain under
`drivers/contracts/` and `drivers/firmware/`. `kdevice` and `kclass` belong here
because they own framework registries or lifecycle/runtime state; `os-api` is
here because it is the host capability contract base shared by drivers. A crate
that only declares a device-type trait stays in `contracts/`.

## Dependency Facts

These are current dependencies, not an idealized layer diagram:

- `kdevice` depends on `driver_base`, `driver-os-api`, `fwnode`, `kmutex`,
  `kspin`, `lazyinit`, `smallvec` and `log`. It does not depend on `kclass`,
  `kdriver` or host-kernel implementation crates.
- `kclass` depends on `driver_base`, `kdevice`, `kspin`, `lazyinit`, `log` and
  the class contracts selected by its features. The `virtio-9p` feature also
  depends on `virtio`.
- `driver-os-api` depends on `fwnode` and shared IRQ/time types only.

The `kdevice` binding lock remains a `kmutex` and keeps its sleepable-context
contract. Moving directories is not a reason to add `ktask`, `khal` or any other
host-kernel dependency.

## Boundaries and Known Mixing

- `kdevice` and `kclass` are framework core: they own registries and runtime
  state. `driver_base` and the class crates primarily provide functional
  contracts, but some class crates still contain subsystem runtime
  implementations. For example, `drivers/contracts/block/src/lib.rs` defines
  `BLOCK_DEVICES` and `add_disk()`. This PR only corrects the description; it
  does not migrate the `block` crate.
- Known debt: the `kclass` `virtio-9p` feature directly depends on and
  re-exports the concrete `virtio::Virtio9pDevice` (`Cargo.toml`,
  `src/lib.rs`). This is not a clean generic class contract, and the move to
  `drivers/base/` does not decouple it. Replacing it requires a neutral 9P
  contract plus consumer verification, which is a separate change.
- `kdevice` still carries the bus matcher implementations. Extracting them needs
  its own consumer and ownership analysis and is not part of the directory move.
- `kdriver`, the provider crates and the platform discoverers remain outside
  this directory. They implement host integration and must not duplicate the
  authoritative binding state owned by `kdevice`.

Placement does not prove architectural purity. Treat this directory as the
current home of the framework core, with the exceptions above still open.
