// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn ifreq_flags_use_the_native_linux_uapi_layout() {
    assert_eq!(core::mem::size_of::<Ifreq>(), 40);
    assert_eq!(core::mem::offset_of!(Ifreq, data), 16);
    assert_eq!(IFNAMSIZ, 16);
    let mut request = Ifreq {
        name: [0; IFNAMSIZ],
        data: [0xa5; 24],
    };
    request.set_u16(1);
    let bytes = bytemuck::bytes_of(&request);
    assert_eq!(&bytes[16..18], &1u16.to_ne_bytes());
    assert!(bytes[18..].iter().all(|byte| *byte == 0xa5));
}

#[def_test(user, serial)]
fn interface_flags_roundtrip_and_missing_name_errors() {
    use unittest_support::TestUserValue;

    struct RestoreInterface {
        fd: i32,
        original: TestUserValue<Ifreq>,
    }

    impl Drop for RestoreInterface {
        fn drop(&mut self) {
            let _ = handle_net_ioctl(self.fd, SIOCSIFFLAGS, self.original.as_user_ptr() as usize);
            let _ = kprocess::current_files().close_file(self.fd);
        }
    }

    let fd = crate::sys_socket(AF_INET, linux_raw_sys::net::SOCK_DGRAM, 0).unwrap() as i32;
    let mut request = Ifreq {
        name: [0; IFNAMSIZ],
        data: [0; 24],
    };
    request.name[..4].copy_from_slice(b"eth0");
    let original = TestUserValue::from_value(request).unwrap();
    handle_net_ioctl(fd, SIOCGIFFLAGS, original.as_user_ptr() as usize).unwrap();
    let restore = RestoreInterface { fd, original };
    let mut request = TestUserValue::from_value(restore.original.read()).unwrap();
    let arg = request.as_user_ptr() as usize;
    for is_up in [false, true] {
        let mut value = restore.original.read();
        let flags = u16::from_ne_bytes(value.data[..2].try_into().unwrap());
        value.set_u16((flags & !1) | u16::from(is_up));
        request.write(value);
        assert_eq!(handle_net_ioctl(fd, SIOCSIFFLAGS, arg), Ok(Some(0)));
        assert_eq!(handle_net_ioctl(fd, SIOCGIFFLAGS, arg), Ok(Some(0)));
        let flags = u16::from_ne_bytes(request.read().data[..2].try_into().unwrap());
        assert_eq!(flags & 1 != 0, is_up);
    }
    let mut missing = Ifreq {
        name: [0; IFNAMSIZ],
        data: [0; 24],
    };
    missing.name[..13].copy_from_slice(b"ik946p-absent");
    request.write(missing);
    for command in [SIOCGIFFLAGS, SIOCSIFFLAGS] {
        assert_eq!(
            handle_net_ioctl(fd, command, arg),
            Err(KError::from(LinuxError::ENODEV))
        );
    }
}

#[def_test]
fn inet_abc_len_uses_classful_defaults() {
    assert_eq!(inet_abc_len(Ipv4Addr::UNSPECIFIED), Some(0));
    assert_eq!(inet_abc_len(Ipv4Addr::BROADCAST), Some(0));
    assert_eq!(inet_abc_len(Ipv4Addr::new(0, 1, 2, 3)), Some(8));
    assert_eq!(inet_abc_len(Ipv4Addr::new(10, 0, 2, 15)), Some(8));
    assert_eq!(inet_abc_len(Ipv4Addr::new(172, 16, 0, 1)), Some(16));
    assert_eq!(inet_abc_len(Ipv4Addr::new(192, 168, 1, 1)), Some(24));
    assert_eq!(inet_abc_len(Ipv4Addr::new(224, 0, 0, 1)), None);
    assert_eq!(inet_abc_len(Ipv4Addr::new(240, 0, 0, 1)), Some(32));
}

#[def_test]
fn prefix_from_netmask_rejects_non_contiguous_masks() {
    assert_eq!(prefix_from_netmask(Ipv4Addr::UNSPECIFIED), Some(0));
    assert_eq!(
        prefix_from_netmask(Ipv4Addr::new(255, 255, 255, 0)),
        Some(24)
    );
    assert_eq!(prefix_from_netmask(Ipv4Addr::new(255, 255, 0, 255)), None);
    assert_eq!(core::mem::size_of::<Rtentry>(), 120);
}

#[def_test]
fn extract_inet_ipv4_requires_af_inet() {
    let mut data = [0u8; 24];
    data[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    data[4..8].copy_from_slice(&[10, 0, 2, 15]);
    assert_eq!(
        extract_inet_ipv4(&data).unwrap(),
        Ipv4Addr::new(10, 0, 2, 15)
    );

    data[0..2].copy_from_slice(&0u16.to_ne_bytes());
    assert_eq!(
        extract_inet_ipv4(&data).unwrap_err(),
        KError::from(LinuxError::EINVAL)
    );
}

#[def_test]
fn route_delete_treats_zero_gateway_as_wildcard() {
    let gateway = Sockaddr {
        family: AF_INET as u16,
        data: [0; 14],
    };
    assert_eq!(
        parse_route_gateway(RTF_GATEWAY, gateway, RouteOperation::Delete).unwrap(),
        None
    );
    assert_eq!(
        parse_route_gateway(RTF_GATEWAY, gateway, RouteOperation::Add).unwrap_err(),
        KError::from(LinuxError::EINVAL)
    );
}

#[def_test]
fn udhcpc_ioctls_include_broadcast_and_route_delete() {
    assert!(is_net_ioctl_cmd(SIOCSIFBRDADDR));
    assert!(is_net_ioctl_cmd(SIOCDELRT));
    assert!(is_net_ioctl_write(SIOCSIFBRDADDR));
    assert!(is_net_ioctl_write(SIOCDELRT));
}
