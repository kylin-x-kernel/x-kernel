// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_uts_namespace_default_values() {
    let ns = UtsNamespace::new();
    assert_eq!(ns.nodename(), b"kylin-x");
    assert_eq!(ns.domainname(), b"https://gitee/openkylin/x-kernel");
}

#[def_test]
fn test_uts_namespace_set_nodename() {
    let ns = UtsNamespace::new();
    ns.set_nodename(b"testhost").unwrap();
    assert_eq!(ns.nodename(), b"testhost");
}

#[def_test]
fn test_uts_namespace_set_domainname() {
    let ns = UtsNamespace::new();
    ns.set_domainname(b"example.com").unwrap();
    assert_eq!(ns.domainname(), b"example.com");
}

#[def_test]
fn test_uts_namespace_rejects_oversized_name() {
    let ns = UtsNamespace::new();
    let long_name = [b'x'; 65];
    assert!(ns.set_nodename(&long_name).is_err());
    assert!(ns.set_domainname(&long_name).is_err());
}

#[def_test]
fn test_uts_namespace_clone_isolation() {
    let original = UtsNamespace::new();
    original.set_nodename(b"parent").unwrap();

    let cloned = UtsNamespace::clone_from(&original);
    cloned.set_nodename(b"child").unwrap();

    assert_eq!(original.nodename(), b"parent");
    assert_eq!(cloned.nodename(), b"child");
}

#[def_test]
fn test_uts_namespace_unique_ids() {
    let ns1 = UtsNamespace::new();
    let ns2 = UtsNamespace::new();
    assert_ne!(ns1.id(), ns2.id());
}
