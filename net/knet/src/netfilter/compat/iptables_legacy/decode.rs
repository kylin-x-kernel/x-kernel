// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Decodes legacy entries. Jump offsets are checked later when building the table.

use alloc::{string::String, vec::Vec};
use core::ffi::CStr;

use zerocopy::FromBytes;

use super::{
    abi::{
        IFNAMSIZ, IPT_ENTRY_SIZE, IPT_ICMP_INV, IPT_INV_DSTIP, IPT_INV_PROTO, IPT_INV_SRCIP,
        IPT_INV_VIA_IN, IPT_INV_VIA_OUT, IptEntry, IptIcmp, IptIp, XT_ALIGN, XT_ENTRY_HEADER_SIZE,
        XT_RETURN, XT_TCP_INV_DSTPT, XT_TCP_INV_FLAGS, XT_TCP_INV_SRCPT, XT_UDP_INV_DSTPT,
        XT_UDP_INV_SRCPT, XT_VERDICT_ACCEPT, XT_VERDICT_DROP, XtEntryHeader, XtErrorTarget,
        XtStandardTarget, XtTcp, XtUdp,
    },
    encode::LegacyExtension,
};
use crate::{
    ip::{Ipv4Address, Ipv4Cidr},
    netfilter::filter::{
        matcher::{
            BaseMatch, ExtensionMatch, IcmpMatch, InterfacePattern, Negated, PortRange,
            TcpFlagsCondition, TcpMatch, UdpMatch,
        },
        rule::{MAX_EXTENSION_MATCHES_PER_RULE, Verdict},
    },
};

/// An invalid entry layout or value, or an unsupported match or target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DecodeError {
    InvalidLayout,
    InvalidValue,
    Unsupported,
}

/// An aligned byte offset from the start of a table.
/// The target entry is checked when building the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TableOffset(u32);

impl TableOffset {
    /// Returns the offset in bytes.
    pub(super) fn bytes(self) -> u32 {
        self.0
    }
}

/// A standard action or an ERROR target for a chain or the table end.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum DecodedTarget {
    Verdict(Verdict),
    Return,
    /// Byte offset from the start of the table. Building the table decides
    /// whether this means `Continue` or `Jump`.
    Offset(TableOffset),
    ErrorTarget(String),
}

/// A decoded entry that owns its data. `Rule::new` checks that its protocol
/// agrees with its extension matches.
#[derive(Debug)]
pub(super) struct DecodedEntry {
    pub(super) next_offset: usize,
    pub(super) base: BaseMatch,
    pub(super) extensions: Vec<ExtensionMatch>,
    pub(super) target: DecodedTarget,
}

impl DecodedEntry {
    /// Decodes the first entry in `input`, which may contain more entries.
    ///
    /// Must run in task context: it allocates space for names with length limits
    /// and at most 16 extension matches. On error, no partial entry is returned.
    /// Counters, cache fields, and padding are ignored. Checks that need the
    /// whole table, such as jump targets and chain types, are left to its builder.
    pub(super) fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let header = read::<IptEntry>(input)?;
        let next_offset = usize::from(header.next_offset);
        let target_offset = usize::from(header.target_offset);
        if target_offset < IPT_ENTRY_SIZE
            || !target_offset.is_multiple_of(XT_ALIGN)
            || !next_offset.is_multiple_of(XT_ALIGN)
            || target_offset + XT_ENTRY_HEADER_SIZE > next_offset
        {
            return Err(DecodeError::InvalidLayout);
        }
        let entry = input.get(..next_offset).ok_or(DecodeError::InvalidLayout)?;
        let base = BaseMatch::try_from(&header.ip)?;
        let mut extensions = Vec::new();
        let mut remaining = &entry[IPT_ENTRY_SIZE..target_offset];
        while !remaining.is_empty() {
            if extensions.len() == MAX_EXTENSION_MATCHES_PER_RULE {
                return Err(DecodeError::InvalidValue);
            }
            let header = read::<XtEntryHeader>(remaining)?;
            let size = usize::from(header.size);
            if size < XT_ENTRY_HEADER_SIZE || !size.is_multiple_of(XT_ALIGN) {
                return Err(DecodeError::InvalidLayout);
            }
            let bytes = remaining.get(..size).ok_or(DecodeError::InvalidLayout)?;
            extensions.push(decode_extension(&header, bytes)?);
            remaining = &remaining[size..];
        }
        let target = DecodedTarget::decode(&entry[target_offset..])?;
        if matches!(target, DecodedTarget::ErrorTarget(_))
            && (base != BaseMatch::default() || !extensions.is_empty())
        {
            return Err(DecodeError::InvalidValue);
        }
        Ok(Self {
            next_offset,
            base,
            extensions,
            target,
        })
    }
}

impl DecodedTarget {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let header = read::<XtEntryHeader>(bytes)?;
        if usize::from(header.size) != bytes.len() {
            return Err(DecodeError::InvalidLayout);
        }
        let extension = header.extension()?;
        match extension {
            LegacyExtension::Standard => match read::<XtStandardTarget>(bytes)?.verdict {
                XT_VERDICT_ACCEPT => Ok(Self::Verdict(Verdict::Accept)),
                XT_VERDICT_DROP => Ok(Self::Verdict(Verdict::Drop)),
                XT_RETURN => Ok(Self::Return),
                offset if offset >= 0 && (offset as usize).is_multiple_of(XT_ALIGN) => {
                    Ok(Self::Offset(TableOffset(offset as u32)))
                }
                _ => Err(DecodeError::InvalidValue),
            },
            LegacyExtension::Error => Ok(Self::ErrorTarget(
                read::<XtErrorTarget>(bytes)?.decode_name()?,
            )),
            _ => Err(DecodeError::Unsupported),
        }
    }
}

fn decode_extension(header: &XtEntryHeader, bytes: &[u8]) -> Result<ExtensionMatch, DecodeError> {
    let extension = header.extension()?;
    let data = &bytes[XT_ENTRY_HEADER_SIZE..];
    match extension {
        LegacyExtension::Tcp => Ok(ExtensionMatch::Tcp(TcpMatch::try_from(&read::<XtTcp>(
            data,
        )?)?)),
        LegacyExtension::Udp => Ok(ExtensionMatch::Udp(UdpMatch::try_from(&read::<XtUdp>(
            data,
        )?)?)),
        LegacyExtension::Icmp => Ok(ExtensionMatch::Icmp(IcmpMatch::try_from(
            &read::<IptIcmp>(data)?,
        )?)),
        _ => Err(DecodeError::Unsupported),
    }
}

impl XtEntryHeader {
    fn extension(&self) -> Result<LegacyExtension, DecodeError> {
        let name = CStr::from_bytes_until_nul(&self.name).map_err(|_| DecodeError::InvalidValue)?;
        let extension = LegacyExtension::lookup(name.to_bytes()).ok_or(DecodeError::Unsupported)?;
        if self.revision != extension.revision() || usize::from(self.size) != extension.size() {
            return Err(DecodeError::Unsupported);
        }
        Ok(extension)
    }
}

impl XtErrorTarget {
    fn decode_name(&self) -> Result<String, DecodeError> {
        let name = CStr::from_bytes_until_nul(&self.name).map_err(|_| DecodeError::InvalidValue)?;
        let name = name.to_str().map_err(|_| DecodeError::Unsupported)?;
        if name.is_empty() {
            return Err(DecodeError::InvalidValue);
        }
        Ok(String::from(name))
    }
}

impl TryFrom<&IptIp> for BaseMatch {
    type Error = DecodeError;

    fn try_from(ip: &IptIp) -> Result<Self, Self::Error> {
        let inverse = ip.inverse_flags;
        if ip.flags != 0
            || inverse
                & !(IPT_INV_VIA_IN
                    | IPT_INV_VIA_OUT
                    | IPT_INV_SRCIP
                    | IPT_INV_DSTIP
                    | IPT_INV_PROTO)
                != 0
        {
            return Err(DecodeError::Unsupported);
        }
        let protocol = u8::try_from(ip.protocol).map_err(|_| DecodeError::Unsupported)?;
        Ok(Self {
            source: decode_cidr(ip.source, ip.source_mask, inverse & IPT_INV_SRCIP != 0)?,
            destination: decode_cidr(
                ip.destination,
                ip.destination_mask,
                inverse & IPT_INV_DSTIP != 0,
            )?,
            input_interface: decode_interface(
                &ip.input_interface,
                &ip.input_interface_mask,
                inverse & IPT_INV_VIA_IN != 0,
            )?,
            output_interface: decode_interface(
                &ip.output_interface,
                &ip.output_interface_mask,
                inverse & IPT_INV_VIA_OUT != 0,
            )?,
            // Protocol zero matches any protocol, even with the NOT flag set.
            protocol: (protocol != 0)
                .then_some(Negated::new(protocol, inverse & IPT_INV_PROTO != 0)),
        })
    }
}

impl TryFrom<&XtTcp> for TcpMatch {
    type Error = DecodeError;

    fn try_from(tcp: &XtTcp) -> Result<Self, Self::Error> {
        let inverse = tcp.inverse_flags;
        if tcp.option != 0
            || inverse & !(XT_TCP_INV_SRCPT | XT_TCP_INV_DSTPT | XT_TCP_INV_FLAGS) != 0
        {
            return Err(DecodeError::Unsupported);
        }
        let flags = TcpFlagsCondition::new(tcp.flags_mask, tcp.flags_compare)
            .ok_or(DecodeError::InvalidValue)?;
        let tcp = Self::new()
            .with_source_port(decode_ports(
                tcp.source_ports,
                inverse & XT_TCP_INV_SRCPT != 0,
            )?)
            .with_destination_port(decode_ports(
                tcp.destination_ports,
                inverse & XT_TCP_INV_DSTPT != 0,
            )?);
        Ok(if flags.mask() != 0 || inverse & XT_TCP_INV_FLAGS != 0 {
            tcp.with_flags(Negated::new(flags, inverse & XT_TCP_INV_FLAGS != 0))
        } else {
            tcp
        })
    }
}

impl TryFrom<&XtUdp> for UdpMatch {
    type Error = DecodeError;

    fn try_from(udp: &XtUdp) -> Result<Self, Self::Error> {
        if udp.inverse_flags & !(XT_UDP_INV_SRCPT | XT_UDP_INV_DSTPT) != 0 {
            return Err(DecodeError::Unsupported);
        }
        Ok(Self::new()
            .with_source_port(decode_ports(
                udp.source_ports,
                udp.inverse_flags & XT_UDP_INV_SRCPT != 0,
            )?)
            .with_destination_port(decode_ports(
                udp.destination_ports,
                udp.inverse_flags & XT_UDP_INV_DSTPT != 0,
            )?))
    }
}

impl TryFrom<&IptIcmp> for IcmpMatch {
    type Error = DecodeError;

    fn try_from(icmp: &IptIcmp) -> Result<Self, Self::Error> {
        if icmp.inverse_flags & !IPT_ICMP_INV != 0 {
            return Err(DecodeError::Unsupported);
        }
        let (icmp_type, code) = if icmp.icmp_type == u8::MAX {
            (None, [0, u8::MAX])
        } else {
            (Some(icmp.icmp_type), icmp.code)
        };
        Self::new(icmp_type, code[0], code[1])
            .map(|value| value.with_inversion(icmp.inverse_flags & IPT_ICMP_INV != 0))
            .ok_or(DecodeError::InvalidValue)
    }
}

fn decode_cidr(
    address: [u8; 4],
    mask: [u8; 4],
    is_inverted: bool,
) -> Result<Option<Negated<Ipv4Cidr>>, DecodeError> {
    let mask = u32::from_be_bytes(mask);
    let prefix = mask.leading_ones() as u8;
    if mask != u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
        || u32::from_be_bytes(address) & !mask != 0
    {
        return Err(DecodeError::Unsupported);
    }
    if prefix == 0 && !is_inverted {
        return Ok(None);
    }
    let cidr = Ipv4Cidr::new(Ipv4Address::from_octets(address), prefix)
        .ok_or(DecodeError::InvalidValue)?;
    Ok(Some(Negated::new(cidr, is_inverted)))
}

fn decode_interface(
    name: &[u8; IFNAMSIZ],
    mask: &[u8; IFNAMSIZ],
    is_inverted: bool,
) -> Result<Option<Negated<InterfacePattern>>, DecodeError> {
    let len = mask.iter().take_while(|&&byte| byte == u8::MAX).count();
    if mask[len..].iter().any(|&byte| byte != 0) {
        return Err(DecodeError::Unsupported);
    }
    if len == 0 && !is_inverted {
        return Ok(None);
    }
    let significant = &name[..len];
    let end = significant
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(len);
    if significant[end..].iter().any(|&byte| byte != 0) {
        return Err(DecodeError::Unsupported);
    }
    let name = core::str::from_utf8(&significant[..end]).map_err(|_| DecodeError::Unsupported)?;
    let pattern = if end < len {
        InterfacePattern::exact(String::from(name))
    } else {
        InterfacePattern::prefix(String::from(name))
    }
    .ok_or(DecodeError::Unsupported)?;
    Ok(Some(Negated::new(pattern, is_inverted)))
}

fn decode_ports(ports: [u16; 2], is_inverted: bool) -> Result<Negated<PortRange>, DecodeError> {
    let range = PortRange::new(ports[0], ports[1]).ok_or(DecodeError::InvalidValue)?;
    Ok(Negated::new(range, is_inverted))
}

fn read<T: FromBytes>(bytes: &[u8]) -> Result<T, DecodeError> {
    T::read_from_prefix(bytes)
        .map(|(value, _)| value)
        .map_err(|_| DecodeError::InvalidLayout)
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/decode/tests.rs"
);
