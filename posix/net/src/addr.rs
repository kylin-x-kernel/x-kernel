// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrapper for [`sockaddr`]. Using trait to convert between [`SocketAddr`] and
//! [`sockaddr`] types.

use alloc::vec::Vec;
use core::{
    mem::size_of,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    str,
};

use kerrno::{KError, KResult, LinuxError};
#[cfg(feature = "vsock")]
use knet::vsock::VsockAddr;
use knet::{SocketAddrEx, netlink::NetlinkAddr, packet::PacketAddr, unix::UnixAddr};
use linux_raw_sys::net::*;
use osvm::VirtPtr;
#[cfg(feature = "vsock")]
use posix_types::net::socket_addr::sockaddr_vm;
use posix_types::{UserConstPtr, UserPtr, UserRead, net::socket_addr::sockaddr_nl};

/// Trait to extend [`SocketAddr`] and its variants with methods for reading
/// from and writing to user space.
pub(crate) trait SocketAddrExt: Sized {
    /// This method attempts to interpret the data pointed to by `addr` with the
    /// given `addrlen` as a valid socket address of the implementing type.
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self>;

    /// This method serializes the current socket address instance into the
    /// [`sockaddr`] structure pointed to by `addr` in user space.
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()>;
}

/// Read the address family from a user-space sockaddr
fn read_family(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<u16> {
    if size_of::<__kernel_sa_family_t>() > addrlen as usize {
        return Err(KError::InvalidInput);
    }
    let family = addr.cast::<__kernel_sa_family_t>().read_vm()?;
    Ok(family)
}
/// Cast a reference to a byte slice
unsafe fn cast_to_slice<T>(value: &T) -> &[u8] {
    // SAFETY: `value` is a live reference, and reborrowing its contiguous
    // storage as a same-sized byte slice preserves layout and lifetime.
    unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}
/// Write socket address data to user-space buffer
fn fill_addr(addr: UserPtr<sockaddr>, addrlen: &mut socklen_t, data: &[u8]) -> KResult<()> {
    let len = (*addrlen as usize).min(data.len());
    addr.cast::<u8>().write_vm_slice(&data[..len])?;
    *addrlen = data.len() as _;
    Ok(())
}

/// SocketAddrExt implementation for SocketAddr (IPv4 or IPv6)
impl SocketAddrExt for SocketAddr {
    /// Read IPv4 or IPv6 socket address from user space
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        match read_family(addr, addrlen)? as u32 {
            AF_INET => SocketAddrV4::read_from_user(addr, addrlen).map(Self::V4),
            AF_INET6 => SocketAddrV6::read_from_user(addr, addrlen).map(Self::V6),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    /// Write IPv4 or IPv6 socket address to user space
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        match self {
            SocketAddr::V4(v4) => v4.write_to_user(addr, addrlen),
            SocketAddr::V6(v6) => v6.write_to_user(addr, addrlen),
        }
    }
}

/// SocketAddrExt implementation for IPv4 socket addresses
impl SocketAddrExt for SocketAddrV4 {
    /// Read IPv4 socket address from user space
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        if addrlen < size_of::<sockaddr_in>() as socklen_t {
            return Err(KError::InvalidInput);
        }
        let addr_in = addr.cast::<sockaddr_in>().read_vm()?;
        if addr_in.sin_family as u32 != AF_INET {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }

        Ok(SocketAddrV4::new(
            Ipv4Addr::from_bits(u32::from_be(addr_in.sin_addr.s_addr)),
            u16::from_be(addr_in.sin_port),
        ))
    }

    /// Write IPv4 socket address to user space
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        let sockin_addr = sockaddr_in {
            sin_family: AF_INET as _,
            sin_port: self.port().to_be(),
            sin_addr: in_addr {
                s_addr: u32::from_ne_bytes(self.ip().octets()),
            },
            __pad: [0_u8; 8],
        };
        // SAFETY: `sockin_addr` is a stack-allocated POD socket-address struct.
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&sockin_addr) })
    }
}

/// SocketAddrExt implementation for IPv6 socket addresses
impl SocketAddrExt for SocketAddrV6 {
    /// Read IPv6 socket address from user space
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        if addrlen < size_of::<sockaddr_in6>() as socklen_t {
            return Err(KError::InvalidInput);
        }
        let addr_in6 = addr.cast::<sockaddr_in6>().read_vm()?;
        if addr_in6.sin6_family as u32 != AF_INET6 {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }

        Ok(SocketAddrV6::new(
            // SAFETY: bindgen exposes the IPv6 bytes through a union field with
            // the same layout as the kernel `in6_addr` representation.
            Ipv6Addr::from(unsafe { addr_in6.sin6_addr.in6_u.u6_addr8 }),
            u16::from_be(addr_in6.sin6_port),
            u32::from_be(addr_in6.sin6_flowinfo),
            addr_in6.sin6_scope_id,
        ))
    }

    /// Write IPv6 socket address to user space
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        let sockin_addr = sockaddr_in6 {
            sin6_family: AF_INET6 as _,
            sin6_port: self.port().to_be(),
            sin6_flowinfo: self.flowinfo().to_be(),
            sin6_addr: in6_addr {
                in6_u: linux_raw_sys::net::in6_addr__bindgen_ty_1 {
                    u6_addr8: self.ip().octets(),
                },
            },
            sin6_scope_id: self.scope_id(),
        };
        // SAFETY: `sockin_addr` is a stack-allocated POD socket-address struct.
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&sockin_addr) })
    }
}

/// SocketAddrExt implementation for Unix domain socket addresses
impl SocketAddrExt for UnixAddr {
    /// Read Unix domain socket address from user space
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        if read_family(addr, addrlen)? as u32 != AF_UNIX {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
        let offset = size_of::<__kernel_sa_family_t>();
        let ptr = UserConstPtr::<u8>::from(addr.as_ptr() as usize + offset);
        let data = ptr.load_vm_vec(addrlen as usize - offset)?;
        Ok(if data.is_empty() {
            Self::Unbound
        } else if data[0] == 0 {
            Self::Abstract(data[1..].into())
        } else {
            let end = data.iter().position(|&c| c == 0).unwrap_or(data.len());
            Self::Path(
                str::from_utf8(&data[..end])
                    .map_err(|_| KError::InvalidInput)?
                    .into(),
            )
        })
    }

    /// Write Unix domain socket address to user space
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        let data_len = match self {
            UnixAddr::Unbound => 0,
            UnixAddr::Abstract(name) => name.len() + 1,
            UnixAddr::Path(path) => 1 + path.len(),
        };
        let mut buf = Vec::with_capacity(size_of::<__kernel_sa_family_t>() + data_len);
        buf.extend_from_slice(&AF_UNIX.to_ne_bytes());
        match self {
            UnixAddr::Unbound => {}
            UnixAddr::Abstract(name) => {
                buf.push(0);
                buf.extend_from_slice(name);
            }
            UnixAddr::Path(path) => {
                buf.extend_from_slice(path.as_bytes());
                buf.push(0);
            }
        }

        fill_addr(addr, addrlen, &buf)
    }
}

impl SocketAddrExt for NetlinkAddr {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        if addrlen != size_of::<sockaddr_nl>() as socklen_t {
            return Err(KError::InvalidInput);
        }
        let addr_nl = addr.cast::<sockaddr_nl>().read_vm()?;
        if addr_nl.nl_family as u32 != AF_NETLINK {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
        Ok(NetlinkAddr {
            pid: addr_nl.nl_pid,
            groups: addr_nl.nl_groups,
        })
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        let addr_nl = sockaddr_nl {
            nl_family: AF_NETLINK as _,
            nl_pad: 0,
            nl_pid: self.pid,
            nl_groups: self.groups,
        };
        // SAFETY: `addr_nl` is a stack-allocated POD socket-address struct.
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&addr_nl) })
    }
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_ll {
    pub sll_family: __kernel_sa_family_t,
    pub sll_protocol: u16,
    pub sll_ifindex: i32,
    pub sll_hatype: u16,
    pub sll_pkttype: u8,
    pub sll_halen: u8,
    pub sll_addr: [u8; 8],
}

// SAFETY: `sockaddr_ll` is a POD socket-address carrier copied by value.
unsafe impl UserRead for sockaddr_ll {}

impl SocketAddrExt for PacketAddr {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        if addrlen < size_of::<sockaddr_ll>() as socklen_t {
            return Err(KError::InvalidInput);
        }
        let addr_ll = addr.cast::<sockaddr_ll>().read_vm()?;
        if addr_ll.sll_family as u32 != AF_PACKET {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
        Ok(PacketAddr {
            protocol: addr_ll.sll_protocol,
            ifindex: addr_ll.sll_ifindex,
            hatype: addr_ll.sll_hatype,
            pkttype: addr_ll.sll_pkttype,
            addr_len: addr_ll.sll_halen,
            addr: addr_ll.sll_addr,
        })
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        let addr_ll = sockaddr_ll {
            sll_family: AF_PACKET as _,
            sll_protocol: self.protocol,
            sll_ifindex: self.ifindex,
            sll_hatype: self.hatype,
            sll_pkttype: self.pkttype,
            sll_halen: self.addr_len,
            sll_addr: self.addr,
        };
        // SAFETY: `addr_ll` is a stack-allocated POD socket-address struct.
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&addr_ll) })
    }
}

/// SocketAddrExt implementation for Vsock addresses
#[cfg(feature = "vsock")]
impl SocketAddrExt for VsockAddr {
    /// Read Vsock address from user space
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        if addrlen != size_of::<sockaddr_vm>() as socklen_t {
            return Err(KError::InvalidInput);
        }

        let addr_vsock = addr.cast::<sockaddr_vm>().read_vm()?;
        if addr_vsock.svm_family as u32 != AF_VSOCK {
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
        Ok(VsockAddr {
            cid: addr_vsock.svm_cid as _,
            port: addr_vsock.svm_port,
        })
    }

    /// Write Vsock address to user space
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        let sockvm_addr = sockaddr_vm {
            svm_family: AF_VSOCK as _,
            svm_reserved1: 0,
            svm_port: self.port,
            svm_cid: self.cid as _,
            svm_zero: [0_u8; 4],
        };
        // SAFETY: `sockvm_addr` is a stack-allocated POD socket-address struct.
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&sockvm_addr) })
    }
}

/// SocketAddrExt implementation for extended socket addresses (all types)
impl SocketAddrExt for SocketAddrEx {
    /// Read any type of socket address from user space
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> KResult<Self> {
        match read_family(addr, addrlen)? as u32 {
            AF_INET | AF_INET6 => SocketAddr::read_from_user(addr, addrlen).map(Self::Ip),
            AF_UNIX => UnixAddr::read_from_user(addr, addrlen).map(Self::Unix),
            AF_NETLINK => NetlinkAddr::read_from_user(addr, addrlen).map(Self::Netlink),
            AF_PACKET => PacketAddr::read_from_user(addr, addrlen).map(Self::Packet),
            #[cfg(feature = "vsock")]
            AF_VSOCK => VsockAddr::read_from_user(addr, addrlen).map(Self::Vsock),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    /// Write any type of socket address to user space
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> KResult<()> {
        match self {
            SocketAddrEx::Ip(ip_addr) => ip_addr.write_to_user(addr, addrlen),
            SocketAddrEx::Unix(unix_addr) => unix_addr.write_to_user(addr, addrlen),
            SocketAddrEx::Netlink(netlink_addr) => netlink_addr.write_to_user(addr, addrlen),
            SocketAddrEx::Packet(packet_addr) => packet_addr.write_to_user(addr, addrlen),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(vsock_addr) => vsock_addr.write_to_user(addr, addrlen),
        }
    }
}
