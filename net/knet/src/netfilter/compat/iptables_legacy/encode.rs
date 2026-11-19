// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Encodes legacy table metadata and entries using jump offsets from the caller.

use zerocopy::{Immutable, IntoBytes};

use super::{
    ExtensionKind, FILTER_VALID_HOOKS, TableLayout,
    abi::{
        ERROR_ENTRY_SIZE, IFNAMSIZ, IPT_ENTRY_SIZE, IPT_ICMP_INV, IPT_ICMP_MATCH_SIZE,
        IPT_INV_DSTIP, IPT_INV_PROTO, IPT_INV_SRCIP, IPT_INV_VIA_IN, IPT_INV_VIA_OUT,
        IPV4_ADDRESS_SIZE, IptEntry, IptGetInfo, IptIcmp, IptIp, STANDARD_ENTRY_SIZE,
        TABLE_NAME_LEN, XT_ENTRY_HEADER_SIZE, XT_ERROR_TARGET_SIZE, XT_STANDARD_TARGET_SIZE,
        XT_TCP_INV_DSTPT, XT_TCP_INV_FLAGS, XT_TCP_INV_SRCPT, XT_TCP_MATCH_SIZE, XT_UDP_INV_DSTPT,
        XT_UDP_INV_SRCPT, XT_UDP_MATCH_SIZE, XT_VERDICT_ACCEPT, XT_VERDICT_DROP, XtCounters,
        XtEntryHeader, XtErrorTarget, XtStandardTarget, XtTcp, XtUdp,
    },
    abi_name,
};
use crate::netfilter::filter::{
    counter::FilterCounter,
    matcher::{
        BaseMatch, ExtensionMatch, IcmpMatch, InterfacePattern, Negated, PortRange, TcpMatch,
        UdpMatch,
    },
    rule::Verdict,
};

/// Invalid sizes or names, or matches the ABI cannot express.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::netfilter) enum EncodeError {
    BufferSize,
    EntryTooLarge,
    TableTooLarge,
    UnsupportedMatch,
    InvalidName,
}

/// Supported legacy match and target formats shared by encoding, decoding, and revision queries.
#[derive(Clone, Copy)]
pub(super) enum LegacyExtension {
    Tcp,
    Udp,
    Icmp,
    Standard,
    Error,
}

impl LegacyExtension {
    const ADVERTISED: [Self; 5] = [
        Self::Tcp,
        Self::Udp,
        Self::Icmp,
        Self::Standard,
        Self::Error,
    ];

    /// Finds a format by its name without the final NUL byte.
    pub(super) fn lookup(name: &[u8]) -> Option<Self> {
        Self::ADVERTISED
            .into_iter()
            .find(|extension| extension.name() == name)
    }

    fn name(self) -> &'static [u8] {
        match self {
            Self::Tcp => b"tcp",
            Self::Udp => b"udp",
            Self::Icmp => b"icmp",
            Self::Standard => abi_name(linux_raw_sys::net::XT_STANDARD_TARGET),
            Self::Error => abi_name(linux_raw_sys::net::XT_ERROR_TARGET),
        }
    }

    fn kind(self) -> ExtensionKind {
        match self {
            Self::Tcp | Self::Udp | Self::Icmp => ExtensionKind::Match,
            Self::Standard | Self::Error => ExtensionKind::Target,
        }
    }

    pub(super) fn revision(self) -> u8 {
        match self {
            Self::Tcp | Self::Udp | Self::Icmp | Self::Standard | Self::Error => 0,
        }
    }

    pub(super) fn size(self) -> usize {
        match self {
            Self::Tcp => XT_TCP_MATCH_SIZE,
            Self::Udp => XT_UDP_MATCH_SIZE,
            Self::Icmp => IPT_ICMP_MATCH_SIZE,
            Self::Standard => XT_STANDARD_TARGET_SIZE,
            Self::Error => XT_ERROR_TARGET_SIZE,
        }
    }

    fn header(self) -> Result<XtEntryHeader, EncodeError> {
        let mut header = XtEntryHeader::new(self.size(), self.name())?;
        header.revision = self.revision();
        Ok(header)
    }
}

impl From<&ExtensionMatch> for LegacyExtension {
    fn from(extension: &ExtensionMatch) -> Self {
        match extension {
            ExtensionMatch::Tcp(_) => Self::Tcp,
            ExtensionMatch::Udp(_) => Self::Udp,
            ExtensionMatch::Icmp(_) => Self::Icmp,
        }
    }
}

/// Returns the advertised revision for a native record name and class.
pub(super) fn extension_revision(kind: ExtensionKind, name: &[u8]) -> Option<u8> {
    LegacyExtension::ADVERTISED
        .into_iter()
        .find(|extension| extension.kind() == kind && extension.name() == name)
        .map(LegacyExtension::revision)
}

impl From<&TableLayout> for IptGetInfo {
    fn from(layout: &TableLayout) -> Self {
        let mut name = [0; TABLE_NAME_LEN];
        name[..6].copy_from_slice(b"filter");
        Self {
            name,
            valid_hooks: FILTER_VALID_HOOKS,
            hook_entry: layout.hook_entry,
            underflow: layout.underflow,
            num_entries: layout.num_entries,
            size: layout.size_bytes as u32,
        }
    }
}

/// Returns the native standard-target code for a terminal verdict.
pub(super) fn verdict_code(verdict: Verdict) -> i32 {
    match verdict {
        Verdict::Accept => XT_VERDICT_ACCEPT,
        Verdict::Drop => XT_VERDICT_DROP,
    }
}

/// Returns the aligned size, including the standard target.
pub(super) fn entry_size(extensions: &[ExtensionMatch]) -> Result<usize, EncodeError> {
    let mut size = STANDARD_ENTRY_SIZE;

    for extension in extensions {
        size = size
            .checked_add(extension_size(extension))
            .ok_or(EncodeError::EntryTooLarge)?;
    }

    u16::try_from(size).map_err(|_| EncodeError::EntryTooLarge)?;

    Ok(size)
}

/// Writes an entry with zero counters.
///
/// Use matches from a checked rule. The caller computes target offsets.
/// `verdict` is `-NF_* - 1`, `XT_RETURN`, or a byte offset in the table.
/// `output` must have exactly [`entry_size`] bytes. Discard it on error.
/// Does not allocate memory.
pub(super) fn encode_entry(
    output: &mut [u8],
    base: &BaseMatch,
    extensions: &[ExtensionMatch],
    verdict: i32,
) -> Result<(), EncodeError> {
    if output.len() != entry_size(extensions)? {
        return Err(EncodeError::BufferSize);
    }
    let ip = IptIp::try_from(base)?;
    output.fill(0);
    let mut offset = IPT_ENTRY_SIZE;
    for extension in extensions {
        let size = extension_size(extension);
        encode_extension(&mut output[offset..offset + size], extension)?;
        offset += size;
    }
    let entry = IptEntry::new(ip, offset, output.len())?;
    write(output, &entry)?;
    write(
        &mut output[offset..],
        &XtStandardTarget {
            header: LegacyExtension::Standard.header()?,
            verdict,
            ..Default::default()
        },
    )
}

/// Writes an ERROR entry for a user chain or the table end.
pub(super) fn encode_error_entry(output: &mut [u8], name: &str) -> Result<(), EncodeError> {
    if output.len() != ERROR_ENTRY_SIZE {
        return Err(EncodeError::BufferSize);
    }
    let mut target = XtErrorTarget::default();
    if name.is_empty() || name.len() >= target.name.len() || name.as_bytes().contains(&0) {
        return Err(EncodeError::InvalidName);
    }
    output.fill(0);
    let entry = IptEntry::new(IptIp::default(), IPT_ENTRY_SIZE, output.len())?;
    write(output, &entry)?;
    target.header = LegacyExtension::Error.header()?;
    target.name[..name.len()].copy_from_slice(name.as_bytes());
    write(&mut output[IPT_ENTRY_SIZE..], &target)
}

fn extension_size(extension: &ExtensionMatch) -> usize {
    LegacyExtension::from(extension).size()
}

/// Writes counters into an entry produced by this encoder.
pub(super) fn encode_counter(output: &mut [u8], counter: FilterCounter) -> Result<(), EncodeError> {
    let offset = core::mem::offset_of!(IptEntry, counters);
    write(&mut output[offset..], &XtCounters::from(counter))
}

impl From<FilterCounter> for XtCounters {
    fn from(counter: FilterCounter) -> Self {
        Self {
            packets: counter.packets,
            bytes: counter.bytes,
        }
    }
}

fn encode_extension(output: &mut [u8], extension: &ExtensionMatch) -> Result<(), EncodeError> {
    let header = LegacyExtension::from(extension).header()?;
    write(output, &header)?;
    let data = &mut output[XT_ENTRY_HEADER_SIZE..];
    match extension {
        ExtensionMatch::Tcp(tcp) => write(data, &XtTcp::from(tcp)),
        ExtensionMatch::Udp(udp) => write(data, &XtUdp::from(udp)),
        ExtensionMatch::Icmp(icmp) => write(data, &IptIcmp::try_from(icmp)?),
    }
}

impl TryFrom<&BaseMatch> for IptIp {
    type Error = EncodeError;

    fn try_from(base: &BaseMatch) -> Result<Self, Self::Error> {
        let mut ip = Self::default();
        if let Some(source) = &base.source {
            let cidr = source.value().network();
            ip.source = cidr.address().octets();
            ip.source_mask = prefix_mask(cidr.prefix_len());
            ip.inverse_flags |= inversion(source, IPT_INV_SRCIP);
        }
        if let Some(destination) = &base.destination {
            let cidr = destination.value().network();
            ip.destination = cidr.address().octets();
            ip.destination_mask = prefix_mask(cidr.prefix_len());
            ip.inverse_flags |= inversion(destination, IPT_INV_DSTIP);
        }
        if let Some(interface) = &base.input_interface {
            encode_interface(
                &mut ip.input_interface,
                &mut ip.input_interface_mask,
                interface.value(),
            )?;
            ip.inverse_flags |= inversion(interface, IPT_INV_VIA_IN);
        }
        if let Some(interface) = &base.output_interface {
            encode_interface(
                &mut ip.output_interface,
                &mut ip.output_interface_mask,
                interface.value(),
            )?;
            ip.inverse_flags |= inversion(interface, IPT_INV_VIA_OUT);
        }
        if let Some(protocol) = &base.protocol {
            // Protocol zero skips the check, even when inverted.
            if *protocol.value() == 0 {
                return Err(EncodeError::UnsupportedMatch);
            }
            ip.protocol = u16::from(*protocol.value());
            ip.inverse_flags |= inversion(protocol, IPT_INV_PROTO);
        }
        Ok(ip)
    }
}

impl From<&TcpMatch> for XtTcp {
    fn from(tcp: &TcpMatch) -> Self {
        let mut inverse = 0;
        let mut header = Self {
            source_ports: encode_ports(tcp.source_port_range(), XT_TCP_INV_SRCPT, &mut inverse),
            destination_ports: encode_ports(
                tcp.destination_port_range(),
                XT_TCP_INV_DSTPT,
                &mut inverse,
            ),
            ..Default::default()
        };
        if let Some(flags) = tcp.flags_condition() {
            header.flags_mask = flags.value().mask();
            header.flags_compare = flags.value().expected();
            inverse |= inversion(&flags, XT_TCP_INV_FLAGS);
        }
        header.inverse_flags = inverse;
        header
    }
}

impl From<&UdpMatch> for XtUdp {
    fn from(udp: &UdpMatch) -> Self {
        let mut inverse = 0;
        Self {
            source_ports: encode_ports(udp.source_port_range(), XT_UDP_INV_SRCPT, &mut inverse),
            destination_ports: encode_ports(
                udp.destination_port_range(),
                XT_UDP_INV_DSTPT,
                &mut inverse,
            ),
            inverse_flags: inverse,
            ..Default::default()
        }
    }
}

impl TryFrom<&IcmpMatch> for IptIcmp {
    type Error = EncodeError;

    fn try_from(icmp: &IcmpMatch) -> Result<Self, Self::Error> {
        if icmp.icmp_type() == Some(u8::MAX) {
            return Err(EncodeError::UnsupportedMatch);
        }
        Ok(Self {
            icmp_type: icmp.icmp_type().unwrap_or(u8::MAX),
            code: [icmp.code_start(), icmp.code_end()],
            inverse_flags: u8::from(icmp.is_inverted()) * IPT_ICMP_INV,
        })
    }
}

impl IptEntry {
    fn new(ip: IptIp, target_offset: usize, next_offset: usize) -> Result<Self, EncodeError> {
        Ok(Self {
            ip,
            target_offset: u16::try_from(target_offset).map_err(|_| EncodeError::EntryTooLarge)?,
            next_offset: u16::try_from(next_offset).map_err(|_| EncodeError::EntryTooLarge)?,
            ..Default::default()
        })
    }
}

impl XtEntryHeader {
    fn new(size: usize, name: &[u8]) -> Result<Self, EncodeError> {
        let mut header = Self {
            size: u16::try_from(size).map_err(|_| EncodeError::EntryTooLarge)?,
            ..Default::default()
        };
        if name.len() >= header.name.len() {
            return Err(EncodeError::InvalidName);
        }
        header.name[..name.len()].copy_from_slice(name);
        Ok(header)
    }
}

fn encode_interface(
    name: &mut [u8; IFNAMSIZ],
    mask: &mut [u8; IFNAMSIZ],
    interface: &InterfacePattern,
) -> Result<(), EncodeError> {
    let bytes = interface.name().as_bytes();
    if bytes.contains(&0) {
        return Err(EncodeError::UnsupportedMatch);
    }
    name[..bytes.len()].copy_from_slice(bytes);
    let mask_len = if interface.is_prefix() {
        name[bytes.len()] = b'+';
        bytes.len()
    } else {
        bytes.len() + 1
    };
    mask[..mask_len].fill(u8::MAX);
    Ok(())
}

fn encode_ports(range: Option<Negated<PortRange>>, bit: u8, inverse: &mut u8) -> [u16; 2] {
    range.map_or([0, u16::MAX], |range| {
        *inverse |= inversion(&range, bit);
        [range.value().start(), range.value().end()]
    })
}

fn inversion<T>(condition: &Negated<T>, bit: u8) -> u8 {
    u8::from(condition.is_inverted()) * bit
}

fn prefix_mask(prefix_len: u8) -> [u8; IPV4_ADDRESS_SIZE] {
    u32::MAX
        .checked_shl(u32::BITS - u32::from(prefix_len))
        .unwrap_or(0)
        .to_be_bytes()
}

fn write<T: IntoBytes + Immutable>(output: &mut [u8], header: &T) -> Result<(), EncodeError> {
    header
        .write_to_prefix(output)
        .map_err(|_| EncodeError::BufferSize)
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/encode/tests.rs"
);
