// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IP address, CIDR, and endpoint types used by the in-kernel stack.

use core::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    str::FromStr,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv4Address([u8; 4]);

impl Ipv4Address {
    pub(crate) const BROADCAST: Self = Self([255, 255, 255, 255]);
    pub(crate) const UNSPECIFIED: Self = Self([0, 0, 0, 0]);

    pub(crate) const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    pub(crate) const fn from_octets(octets: [u8; 4]) -> Self {
        Self(octets)
    }

    pub(crate) const fn octets(self) -> [u8; 4] {
        self.0
    }

    pub(crate) fn is_unspecified(self) -> bool {
        self == Self::UNSPECIFIED
    }

    pub(crate) fn is_broadcast(self) -> bool {
        self == Self::BROADCAST
    }

    pub(crate) fn is_multicast(self) -> bool {
        (224..=239).contains(&self.0[0])
    }
}

impl From<Ipv4Addr> for Ipv4Address {
    fn from(addr: Ipv4Addr) -> Self {
        Self(addr.octets())
    }
}

impl From<Ipv4Address> for Ipv4Addr {
    fn from(addr: Ipv4Address) -> Self {
        Self::from(addr.0)
    }
}

impl FromStr for Ipv4Address {
    type Err = <Ipv4Addr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ipv4Addr::from_str(s).map(Self::from)
    }
}

impl fmt::Display for Ipv4Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ipv4Addr::from(*self).fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv6Address([u8; 16]);

impl Ipv6Address {
    pub(crate) const UNSPECIFIED: Self = Self([0; 16]);

    pub(crate) const fn from_octets(octets: [u8; 16]) -> Self {
        Self(octets)
    }

    pub(crate) const fn octets(self) -> [u8; 16] {
        self.0
    }

    pub(crate) fn is_unspecified(self) -> bool {
        self == Self::UNSPECIFIED
    }
}

impl From<Ipv6Addr> for Ipv6Address {
    fn from(addr: Ipv6Addr) -> Self {
        Self::from_octets(addr.octets())
    }
}

impl From<Ipv6Address> for Ipv6Addr {
    fn from(addr: Ipv6Address) -> Self {
        Self::from(addr.octets())
    }
}

impl fmt::Display for Ipv6Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ipv6Addr::from(*self).fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum IpAddress {
    Ipv4(Ipv4Address),
    Ipv6(Ipv6Address),
}

impl IpAddress {
    pub(crate) fn is_unspecified(self) -> bool {
        match self {
            Self::Ipv4(addr) => addr.is_unspecified(),
            Self::Ipv6(addr) => addr.is_unspecified(),
        }
    }

    pub(crate) fn is_broadcast(self) -> bool {
        matches!(self, Self::Ipv4(addr) if addr.is_broadcast())
    }
}

impl From<Ipv4Address> for IpAddress {
    fn from(addr: Ipv4Address) -> Self {
        Self::Ipv4(addr)
    }
}

impl From<Ipv6Address> for IpAddress {
    fn from(addr: Ipv6Address) -> Self {
        Self::Ipv6(addr)
    }
}

impl From<IpAddr> for IpAddress {
    fn from(addr: IpAddr) -> Self {
        match addr {
            IpAddr::V4(addr) => Self::Ipv4(addr.into()),
            IpAddr::V6(addr) => Self::Ipv6(addr.into()),
        }
    }
}

impl From<IpAddress> for IpAddr {
    fn from(addr: IpAddress) -> Self {
        match addr {
            IpAddress::Ipv4(addr) => Self::V4(addr.into()),
            IpAddress::Ipv6(addr) => Self::V6(addr.into()),
        }
    }
}

impl FromStr for IpAddress {
    type Err = <IpAddr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        IpAddr::from_str(s).map(Self::from)
    }
}

impl fmt::Display for IpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipv4(addr) => addr.fmt(f),
            Self::Ipv6(addr) => addr.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv4Cidr {
    address: Ipv4Address,
    prefix_len: u8,
}

impl Ipv4Cidr {
    pub(crate) const fn new(address: Ipv4Address, prefix_len: u8) -> Self {
        Self {
            address,
            prefix_len,
        }
    }

    pub(crate) const fn address(self) -> Ipv4Address {
        self.address
    }

    pub(crate) const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    pub(crate) fn broadcast(self) -> Option<Ipv4Address> {
        if self.prefix_len > 32 {
            return None;
        }
        let mask = prefix_mask(self.prefix_len);
        let addr = u32::from_be_bytes(self.address.octets());
        Some(Ipv4Address::from_octets((addr | !mask).to_be_bytes()))
    }

    pub(crate) fn contains_addr(self, addr: &IpAddress) -> bool {
        let IpAddress::Ipv4(addr) = *addr else {
            return false;
        };
        let mask = prefix_mask(self.prefix_len);
        let lhs = u32::from_be_bytes(self.address.octets()) & mask;
        let rhs = u32::from_be_bytes(addr.octets()) & mask;
        lhs == rhs
    }
}

impl From<Ipv4Cidr> for IpCidr {
    fn from(cidr: Ipv4Cidr) -> Self {
        Self::Ipv4(cidr)
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum IpCidr {
    Ipv4(Ipv4Cidr),
}

impl IpCidr {
    pub(crate) fn address(self) -> IpAddress {
        match self {
            Self::Ipv4(cidr) => IpAddress::Ipv4(cidr.address()),
        }
    }

    pub(crate) fn prefix_len(self) -> u8 {
        match self {
            Self::Ipv4(cidr) => cidr.prefix_len(),
        }
    }

    pub(crate) fn contains_addr(self, addr: &IpAddress) -> bool {
        match self {
            Self::Ipv4(cidr) => cidr.contains_addr(addr),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct IpEndpoint {
    pub(crate) addr: IpAddress,
    pub(crate) port: u16,
}

impl From<SocketAddr> for IpEndpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr: addr.ip().into(),
            port: addr.port(),
        }
    }
}

impl From<SocketAddrV4> for IpEndpoint {
    fn from(addr: SocketAddrV4) -> Self {
        Self {
            addr: IpAddress::Ipv4((*addr.ip()).into()),
            port: addr.port(),
        }
    }
}

impl From<SocketAddrV6> for IpEndpoint {
    fn from(addr: SocketAddrV6) -> Self {
        Self {
            addr: IpAddress::Ipv6((*addr.ip()).into()),
            port: addr.port(),
        }
    }
}

impl From<IpEndpoint> for SocketAddr {
    fn from(endpoint: IpEndpoint) -> Self {
        match endpoint.addr {
            IpAddress::Ipv4(addr) => Self::V4(SocketAddrV4::new(addr.into(), endpoint.port)),
            IpAddress::Ipv6(addr) => Self::V6(SocketAddrV6::new(addr.into(), endpoint.port, 0, 0)),
        }
    }
}

impl fmt::Display for IpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.addr, self.port)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct IpListenEndpoint {
    pub(crate) addr: Option<IpAddress>,
    pub(crate) port: u16,
}

impl fmt::Display for IpListenEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.addr {
            Some(addr) => write!(f, "{}:{}", addr, self.port),
            None => write!(f, "*:{}", self.port),
        }
    }
}

fn prefix_mask(prefix_len: u8) -> u32 {
    match prefix_len {
        0 => 0,
        1..=32 => u32::MAX << (32 - prefix_len),
        _ => u32::MAX,
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::string::ToString;

    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_ip_address_and_cidr_helpers() {
        let addr = Ipv4Address::new(192, 0, 2, 1);
        let cidr = Ipv4Cidr::new(addr, 24);
        let ip_cidr = IpCidr::from(cidr);

        assert_eq!(cidr.address(), addr);
        assert_eq!(cidr.prefix_len(), 24);
        assert_eq!(cidr.broadcast(), Some(Ipv4Address::new(192, 0, 2, 255)));
        assert!(cidr.contains_addr(&IpAddress::from(addr)));
        assert_eq!(ip_cidr.address(), IpAddress::from(addr));
        assert_eq!(ip_cidr.prefix_len(), 24);
        assert!(ip_cidr.contains_addr(&IpAddress::from(addr)));
        assert!(IpAddress::from(Ipv4Address::UNSPECIFIED).is_unspecified());
        assert!(IpAddress::from(Ipv4Address::BROADCAST).is_broadcast());
    }

    #[def_test]
    fn test_ipv6_and_listen_endpoint_helpers() {
        let octets = [1; 16];
        let addr = Ipv6Address::from_octets(octets);
        let endpoint = IpListenEndpoint {
            addr: Some(IpAddress::from(addr)),
            port: 8080,
        };

        assert_eq!(addr.octets(), octets);
        assert!(!addr.is_unspecified());
        assert!(Ipv6Address::UNSPECIFIED.is_unspecified());
        assert_eq!(endpoint.to_string(), "101:101:101:101:101:101:101:101:8080");
    }
}
