// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_namespace_type_names() {
    assert_eq!(NamespaceType::Mnt.name(), "mnt");
    assert_eq!(NamespaceType::Uts.name(), "uts");
    assert_eq!(NamespaceType::Ipc.name(), "ipc");
    assert_eq!(NamespaceType::User.name(), "user");
    assert_eq!(NamespaceType::Pid.name(), "pid");
    assert_eq!(NamespaceType::Net.name(), "net");
    assert_eq!(NamespaceType::Cgroup.name(), "cgroup");
    assert_eq!(NamespaceType::Time.name(), "time");
}
