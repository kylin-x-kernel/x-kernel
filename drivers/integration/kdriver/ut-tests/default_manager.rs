// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Default bus manager assembly contract.
//!
//! `default_bus_manager()` and `BusManager` are public, so callers may
//! enumerate without going through `DeviceManager`. The default PCI backend
//! must therefore obtain its synthetic host/bridge driver handles when it is
//! constructed; enumeration only clones those handles and must not depend on
//! a private preparation step.
//!
//! Every boot, including the unittest boot, runs
//! `late_init_main -> kdriver::init_drivers() -> discover_unified()` before
//! this harness executes, so the first (cold) registration is exercised by
//! the boot path itself. This test pins the assembly contract rather than the
//! process-wide register-once slot: it inspects the handles held by the
//! manager it just built, so a registry reset by another test cannot make it
//! pass without an assembled PCI backend.

use unittest::{assert_eq, assert_ne, def_test};

#[def_test(serial)]
fn default_manager_holds_pci_host_and_bridge_handles() {
    let manager = super::default_bus_manager();
    let (host, bridge) = manager
        .pci_driver_handles()
        .expect("default manager must assemble the PCI backend");

    assert_eq!(host.name(), "pci-host");
    assert_eq!(host.device_kind(), kdevice::DeviceKind::Bus);
    assert_eq!(host.bus_types(), &[kdevice::BusTypeId::PLATFORM][..]);
    assert_eq!(bridge.name(), "pci-bridge");
    assert_eq!(bridge.device_kind(), kdevice::DeviceKind::Bus);
    assert_eq!(bridge.bus_types(), &[kdevice::BusTypeId::PCI][..]);
    assert_ne!(host.id(), bridge.id());
}
