// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network interface ioctl handling (SIOC* family).
//!
//! Write commands (`SIOCSIFADDR`, `SIOCSIFNETMASK`, `SIOCSIFBRDADDR`,
//! `SIOCSIFFLAGS`, `SIOCADDRT`, `SIOCDELRT`) require `CAP_NET_ADMIN`.
//! Address-set commands require `AF_INET`.
//! `SIOCDELRT` treats a missing `rt_dev` and missing gateway as wildcards.

use alloc::string::ToString;
use core::{ffi::c_char, net::Ipv4Addr};

use kerrno::{KError, KResult, LinuxError};
use knet::{find_interface, sock_from_file};
use linux_raw_sys::{
    ioctl::{
        SIOCADDRT, SIOCDELRT, SIOCGIFFLAGS, SIOCGIFHWADDR, SIOCGIFINDEX, SIOCSIFADDR,
        SIOCSIFBRDADDR, SIOCSIFFLAGS, SIOCSIFNETMASK,
    },
    net::AF_INET,
};
use posix_types::{UserConstPtr, UserPtr, UserRead, UserWrite};

/// Maximum interface name storage, including the terminating NUL.
const IFNAMSIZ: usize = 16;

/// Fixed-size socket address carried by `SIOC*` request payloads.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Sockaddr {
    family: u16,
    data: [u8; 14],
}

// SAFETY: Sockaddr is a plain-old-data ABI struct.
unsafe impl UserRead for Sockaddr {}
// SAFETY: Sockaddr is POD and can be safely written to user memory.
unsafe impl UserWrite for Sockaddr {}

impl Sockaddr {
    fn hwaddr(hw: [u8; 6], arphrd: u16) -> Self {
        let mut data = [0u8; 14];
        data[..6].copy_from_slice(&hw);
        Self {
            family: arphrd,
            data,
        }
    }
}

/// Interface request payload with a 16-byte name and 24-byte data area.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Ifreq {
    name: [u8; IFNAMSIZ],
    data: [u8; 24],
}

// SAFETY: Ifreq is a plain-old-data ABI struct.
unsafe impl UserRead for Ifreq {}
// SAFETY: Ifreq is POD and can be safely written to user memory.
unsafe impl UserWrite for Ifreq {}

impl Ifreq {
    fn name_str(&self) -> KResult<alloc::string::String> {
        let nul = self.name.iter().position(|b| *b == 0).unwrap_or(IFNAMSIZ);
        core::str::from_utf8(&self.name[..nul])
            .map(|s| s.to_string())
            .map_err(|_| KError::from(LinuxError::EINVAL))
    }

    fn set_sockaddr(&mut self, addr: Sockaddr) {
        let bytes = bytemuck::bytes_of(&addr);
        self.data[..bytes.len()].copy_from_slice(bytes);
    }

    fn set_u16(&mut self, val: u16) {
        self.data[..2].copy_from_slice(&val.to_ne_bytes());
    }

    fn set_i32(&mut self, val: i32) {
        self.data[..4].copy_from_slice(&val.to_ne_bytes());
    }
}

/// Handles network interface ioctl commands.
///
/// Command recognition uses the exact SIOC* list, not the `0x89xx` ioctl
/// type space, so unrecognized commands can fall through to the file ioctl.
///
/// Returns `Ok(Some(result))` when the command was recognized and handled.
/// Returns `Ok(None)` when the command is not a network interface ioctl.
pub fn handle_net_ioctl(fd: i32, cmd: u32, arg: usize) -> KResult<Option<isize>> {
    if !is_net_ioctl_cmd(cmd) {
        return Ok(None);
    }

    // Resolve the descriptor before command-specific handling so invalid
    // descriptors and non-socket files retain their distinct error paths.
    let file = kprocess::current_resources().get_file(fd)?;
    if sock_from_file(&file).is_err() {
        return Err(KError::NotATty);
    }
    if is_net_ioctl_write(cmd) && !kprocess::current_cred().is_privileged() {
        return Err(KError::from(LinuxError::EPERM));
    }

    match cmd {
        SIOCGIFINDEX => {
            handle_get_index(arg)?;
            Ok(Some(0))
        }
        SIOCGIFHWADDR => {
            handle_get_hwaddr(arg)?;
            Ok(Some(0))
        }
        SIOCGIFFLAGS => {
            handle_get_flags(arg)?;
            Ok(Some(0))
        }
        SIOCSIFADDR => {
            handle_set_addr(arg)?;
            Ok(Some(0))
        }
        SIOCSIFNETMASK => {
            handle_set_netmask(arg)?;
            Ok(Some(0))
        }
        SIOCSIFBRDADDR => {
            handle_set_brdaddr(arg)?;
            Ok(Some(0))
        }
        SIOCADDRT => {
            handle_add_route(arg)?;
            Ok(Some(0))
        }
        SIOCDELRT => {
            handle_del_route(arg)?;
            Ok(Some(0))
        }
        SIOCSIFFLAGS => {
            handle_set_flags(arg)?;
            Ok(Some(0))
        }
        _ => Ok(None),
    }
}

fn is_net_ioctl_cmd(cmd: u32) -> bool {
    matches!(
        cmd,
        SIOCGIFINDEX
            | SIOCGIFHWADDR
            | SIOCGIFFLAGS
            | SIOCSIFADDR
            | SIOCSIFNETMASK
            | SIOCSIFBRDADDR
            | SIOCADDRT
            | SIOCDELRT
            | SIOCSIFFLAGS
    )
}

fn is_net_ioctl_write(cmd: u32) -> bool {
    matches!(
        cmd,
        SIOCSIFADDR | SIOCSIFNETMASK | SIOCSIFBRDADDR | SIOCADDRT | SIOCDELRT | SIOCSIFFLAGS
    )
}

/// Validates the IPv4 address family before applying an address update.
fn extract_inet_ipv4(data: &[u8; 24]) -> KResult<Ipv4Addr> {
    let family = u16::from_ne_bytes([data[0], data[1]]);
    if family != AF_INET as u16 {
        return Err(KError::from(LinuxError::EINVAL));
    }
    Ok(Ipv4Addr::new(data[4], data[5], data[6], data[7]))
}

fn sockaddr_in_addr(sa: &Sockaddr) -> Ipv4Addr {
    Ipv4Addr::new(sa.data[2], sa.data[3], sa.data[4], sa.data[5])
}

/// Selects the classful default prefix for an IPv4 address.
///
/// Matches Linux 6.8 `inet_abc_len`: only `INADDR_ANY` and limited broadcast
/// use prefix 0. Other `0.x.x.x` addresses are class A (`/8`).
fn inet_abc_len(addr: Ipv4Addr) -> Option<u8> {
    let haddr = u32::from(addr);
    if addr.is_unspecified() || addr.is_broadcast() {
        return Some(0);
    }
    if haddr & 0x8000_0000 == 0 {
        Some(8)
    } else if haddr & 0xc000_0000 == 0x8000_0000 {
        Some(16)
    } else if haddr & 0xe000_0000 == 0xc000_0000 {
        Some(24)
    } else if haddr & 0xf000_0000 == 0xf000_0000 {
        Some(32)
    } else {
        None
    }
}

/// Converts a contiguous IPv4 netmask into its prefix length.
fn prefix_from_netmask(netmask: Ipv4Addr) -> Option<u8> {
    let bits = u32::from_be_bytes(netmask.octets());
    let prefix = bits.leading_ones() as u8;
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (bits == expected).then_some(prefix)
}

fn handle_set_addr(arg: usize) -> KResult {
    let ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let addr = extract_inet_ipv4(&ifr.data)?;
    let name = ifr.name_str()?;
    let prefix = inet_abc_len(addr).ok_or(KError::from(LinuxError::EINVAL))?;
    knet::set_interface_ipv4_addr(&name, addr, prefix).map_err(KError::from)?;
    Ok(())
}

fn handle_set_netmask(arg: usize) -> KResult {
    let ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let netmask = extract_inet_ipv4(&ifr.data)?;
    let name = ifr.name_str()?;
    let prefix = prefix_from_netmask(netmask).ok_or(KError::from(LinuxError::EINVAL))?;
    knet::set_interface_ipv4_netmask(&name, prefix).map_err(KError::from)?;
    Ok(())
}

fn handle_set_brdaddr(arg: usize) -> KResult {
    let ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let broadcast = extract_inet_ipv4(&ifr.data)?;
    let name = ifr.name_str()?;
    knet::set_interface_ipv4_broadcast(&name, broadcast).map_err(KError::from)?;
    Ok(())
}

fn handle_set_flags(arg: usize) -> KResult {
    let ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let name = ifr.name_str()?;
    let flags = u16::from_ne_bytes([ifr.data[0], ifr.data[1]]);
    knet::set_interface_flags(&name, flags).map_err(KError::from)?;
    Ok(())
}

/// Native 64-bit ABI carrier for a route-entry request.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Rtentry {
    _pad1: u64,
    dst: Sockaddr,
    gateway: Sockaddr,
    genmask: Sockaddr,
    flags: u16,
    _pad2: i16,
    _pad_to_pad3: [u8; 4],
    _pad3: u64,
    _pad4: u64,
    _metric: i16,
    _pad_to_dev: [u8; 6],
    rt_dev: u64,
    _mtu: u64,
    _window: u64,
    _irtt: u16,
    _pad_end: [u8; 6],
}

// SAFETY: Rtentry is a plain-old-data ABI struct with explicit padding.
unsafe impl UserRead for Rtentry {}

const RTF_GATEWAY: u16 = 0x0002;
const RTF_HOST: u16 = 0x0004;

#[derive(Clone, Copy, Eq, PartialEq)]
enum RouteOperation {
    Add,
    Delete,
}
fn handle_add_route(arg: usize) -> KResult {
    let parsed = parse_rtentry(arg, RouteOperation::Add)?;
    knet::add_ipv4_route(
        parsed.route_dst,
        parsed.dst_len,
        parsed.gateway,
        parsed.oif_name.as_deref(),
    )
    .map_err(KError::from)?;
    Ok(())
}

fn handle_del_route(arg: usize) -> KResult {
    let parsed = parse_rtentry(arg, RouteOperation::Delete)?;
    knet::del_ipv4_route(
        parsed.route_dst,
        parsed.dst_len,
        parsed.gateway,
        parsed.oif_name.as_deref(),
    )
    .map_err(KError::from)?;
    Ok(())
}

struct ParsedRtentry {
    dst_len: u8,
    route_dst: Option<Ipv4Addr>,
    gateway: Option<Ipv4Addr>,
    oif_name: Option<alloc::string::String>,
}

fn parse_rtentry(arg: usize, operation: RouteOperation) -> KResult<ParsedRtentry> {
    let rt: Rtentry = UserConstPtr::<Rtentry>::from(arg).read_vm()?;
    if rt.dst.family != AF_INET as u16 {
        return Err(KError::from(LinuxError::EAFNOSUPPORT));
    }

    let dst = sockaddr_in_addr(&rt.dst);
    let (dst_len, route_dst) = if rt.flags & RTF_HOST != 0 {
        (32, Some(dst))
    } else {
        let mask = sockaddr_in_addr(&rt.genmask);
        if rt.genmask.family != AF_INET as u16 && (u32::from(mask) != 0 || rt.genmask.family != 0) {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
        let prefix = prefix_from_netmask(mask).ok_or(KError::from(LinuxError::EINVAL))?;
        let expected_mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        if u32::from(dst) & !expected_mask != 0 {
            return Err(KError::from(LinuxError::EINVAL));
        }
        if prefix == 0 {
            (0, None)
        } else {
            (prefix, Some(dst))
        }
    };

    let gateway = parse_route_gateway(rt.flags, rt.gateway, operation)?;

    // Route deletion permits an omitted device selector. The route owner then
    // matches by the remaining destination and gateway fields.
    let oif_name = if rt.rt_dev != 0 {
        Some(
            UserConstPtr::<c_char>::from(rt.rt_dev as usize)
                .load_string_with_max_len(IFNAMSIZ - 1)?,
        )
    } else if operation == RouteOperation::Delete {
        None
    } else {
        return Err(KError::from(LinuxError::ENETUNREACH));
    };
    Ok(ParsedRtentry {
        dst_len,
        route_dst,
        gateway,
        oif_name,
    })
}

fn parse_route_gateway(
    flags: u16,
    gateway: Sockaddr,
    operation: RouteOperation,
) -> KResult<Option<Ipv4Addr>> {
    if flags & RTF_GATEWAY == 0 {
        return Ok(None);
    }
    if gateway.family != AF_INET as u16 {
        return Err(KError::from(LinuxError::EINVAL));
    }

    let gateway = sockaddr_in_addr(&gateway);
    if gateway.is_unspecified() {
        return match operation {
            RouteOperation::Add => Err(KError::from(LinuxError::EINVAL)),
            RouteOperation::Delete => Ok(None),
        };
    }
    Ok(Some(gateway))
}

fn handle_get_index(arg: usize) -> KResult {
    let mut ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let name = ifr.name_str()?;
    let iface = find_interface(&name).ok_or(KError::from(LinuxError::ENODEV))?;
    ifr.set_i32(iface.ifindex);
    UserPtr::<Ifreq>::from(arg).write_vm(ifr)?;
    Ok(())
}

fn handle_get_hwaddr(arg: usize) -> KResult {
    let mut ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let name = ifr.name_str()?;
    let iface = find_interface(&name).ok_or(KError::from(LinuxError::ENODEV))?;
    ifr.set_sockaddr(Sockaddr::hwaddr(iface.hardware_addr, iface.arphrd));
    UserPtr::<Ifreq>::from(arg).write_vm(ifr)?;
    Ok(())
}

fn handle_get_flags(arg: usize) -> KResult {
    let mut ifr: Ifreq = UserConstPtr::<Ifreq>::from(arg).read_vm()?;
    let name = ifr.name_str()?;
    let iface = find_interface(&name).ok_or(KError::from(LinuxError::ENODEV))?;
    ifr.set_u16(iface.flags as u16);
    UserPtr::<Ifreq>::from(arg).write_vm(ifr)?;
    Ok(())
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

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
}
