// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Decodes legacy entries and builds validated filter tables.

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::ffi::CStr;

use zerocopy::FromBytes;

use super::{
    FILTER_VALID_HOOKS, HOOK_COUNT,
    abi::{
        IFNAMSIZ, IPT_ENTRY_SIZE, IPT_ICMP_INV, IPT_INV_DSTIP, IPT_INV_PROTO, IPT_INV_SRCIP,
        IPT_INV_VIA_IN, IPT_INV_VIA_OUT, IptEntry, IptIcmp, IptIp, STANDARD_ENTRY_SIZE, XT_ALIGN,
        XT_ENTRY_HEADER_SIZE, XT_RETURN, XT_TCP_INV_DSTPT, XT_TCP_INV_FLAGS, XT_TCP_INV_SRCPT,
        XT_TCP_MATCH_SIZE, XT_UDP_INV_DSTPT, XT_UDP_INV_SRCPT, XT_VERDICT_ACCEPT, XT_VERDICT_DROP,
        XtEntryHeader, XtErrorTarget, XtStandardTarget, XtTcp, XtUdp,
    },
    encode::LegacyExtension,
    linux_hook,
};
use crate::{
    ip::{Ipv4Address, Ipv4Cidr},
    netfilter::filter::{
        chain::{Chain, ChainId, FilterHook},
        matcher::{
            BaseMatch, ExtensionMatch, IcmpMatch, InterfacePattern, Negated, PortRange,
            TcpFlagsCondition, TcpMatch, UdpMatch,
        },
        rule::{MAX_EXTENSION_MATCHES_PER_RULE, Rule, Target, Verdict},
        table::{FilterTable, MAX_CHAINS, MAX_RULES},
    },
};

/// Invalid entries or table structure, unsupported values, or exceeded limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DecodeError {
    InvalidLayout,
    InvalidValue,
    InvalidTable,
    Unsupported,
    TooLarge,
}

// User-chain markers are bounded by MAX_CHAINS, including built-in chains.
pub(super) const MAX_ENTRIES: usize = MAX_RULES + 2 * MAX_CHAINS + 1;
// TCP and UDP have the largest supported match records.
pub(super) const MAX_BYTES: usize =
    MAX_ENTRIES * (STANDARD_ENTRY_SIZE + MAX_EXTENSION_MATCHES_PER_RULE * XT_TCP_MATCH_SIZE);

/// Copied table fields and entry bytes, without replacement conditions.
pub(super) struct LegacyTableInput<'a> {
    pub(super) valid_hooks: u32,
    pub(super) hook_entry: [u32; HOOK_COUNT],
    pub(super) underflow: [u32; HOOK_COUNT],
    pub(super) num_entries: u32,
    pub(super) entries: &'a [u8],
}

impl TryFrom<&LegacyTableInput<'_>> for FilterTable {
    type Error = DecodeError;

    fn try_from(input: &LegacyTableInput<'_>) -> Result<Self, Self::Error> {
        DecodedTable::decode(input)?.into_table()
    }
}

/// A rule action, an unresolved chain call, or a user-chain marker.
enum EntryTarget {
    Action(Target),
    JumpToEntry(usize),
    ChainMarker(String),
}

struct TableEntry {
    base: BaseMatch,
    extensions: Vec<ExtensionMatch>,
    target: EntryTarget,
}

struct ImportedChain {
    head_entry: usize,
    first_rule_entry: usize,
    tail_entry: usize,
    hook: Option<FilterHook>,
}

/// Each entry belongs to one chain. Chain order follows the input bytes.
struct DecodedTable {
    entries: Vec<TableEntry>,
    layout: ImportLayout,
}

impl DecodedTable {
    fn decode(input: &LegacyTableInput<'_>) -> Result<Self, DecodeError> {
        if input.valid_hooks != FILTER_VALID_HOOKS || input.num_entries == 0 {
            return Err(DecodeError::InvalidLayout);
        }
        if input.num_entries as usize > MAX_ENTRIES || input.entries.len() > MAX_BYTES {
            return Err(DecodeError::TooLarge);
        }
        let mut entries = Vec::with_capacity(input.num_entries as usize);
        let mut offsets = Vec::with_capacity(input.num_entries as usize);
        let mut offset_bytes = 0usize;
        while offset_bytes < input.entries.len() {
            if entries.len() == input.num_entries as usize {
                return Err(DecodeError::InvalidLayout);
            }
            let entry = DecodedEntry::decode(&input.entries[offset_bytes..])?;
            let next = offset_bytes
                .checked_add(entry.next_offset)
                .ok_or(DecodeError::TooLarge)?;
            offsets.push(offset_bytes);
            entries.push(entry);
            offset_bytes = next;
        }
        if entries.len() != input.num_entries as usize
            || !matches!(entries.last().map(|entry| &entry.target),
                Some(DecodedTarget::ErrorTarget(name)) if name == "ERROR")
        {
            return Err(DecodeError::InvalidLayout);
        }
        entries.pop();
        offsets.pop();
        let entry_index_fn = |offset_bytes: u32| {
            offsets
                .binary_search(&(offset_bytes as usize))
                .map_err(|_| DecodeError::InvalidLayout)
        };
        let mut chains = Vec::new();
        for hook in [
            FilterHook::LocalIn,
            FilterHook::Forward,
            FilterHook::LocalOut,
        ] {
            let slot = linux_hook(hook);
            let head_entry = entry_index_fn(input.hook_entry[slot])?;
            chains.push(ImportedChain {
                head_entry,
                first_rule_entry: head_entry,
                tail_entry: entry_index_fn(input.underflow[slot])?,
                hook: Some(hook),
            });
        }
        let entries: Vec<_> = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let target = match entry.target {
                    DecodedTarget::Verdict(verdict) => {
                        EntryTarget::Action(Target::Verdict(verdict))
                    }
                    DecodedTarget::Return => EntryTarget::Action(Target::Return),
                    DecodedTarget::Offset(offset) => {
                        let target = entry_index_fn(offset.bytes())?;
                        if target == index + 1 {
                            EntryTarget::Action(Target::Continue)
                        } else {
                            EntryTarget::JumpToEntry(target)
                        }
                    }
                    DecodedTarget::ErrorTarget(name) => EntryTarget::ChainMarker(name),
                };
                Ok(TableEntry {
                    base: entry.base,
                    extensions: entry.extensions,
                    target,
                })
            })
            .collect::<Result<_, DecodeError>>()?;
        drop(offsets);
        let layout = ImportLayout::new(&entries, chains)?;
        Ok(Self { entries, layout })
    }

    fn into_table(self) -> Result<FilterTable, DecodeError> {
        let mut entries = self.entries.into_iter();
        let mut chains = Vec::with_capacity(self.layout.chains.len());
        let mut rule_count = 0usize;
        for chain in &self.layout.chains {
            let name = if chain.hook.is_none() {
                match entries.next().ok_or(DecodeError::InvalidLayout)?.target {
                    EntryTarget::ChainMarker(name) => Some(name),
                    _ => return Err(DecodeError::InvalidLayout),
                }
            } else {
                None
            };
            rule_count += chain.tail_entry - chain.first_rule_entry;
            if rule_count > MAX_RULES {
                return Err(DecodeError::TooLarge);
            }
            let mut rules = Vec::with_capacity(chain.tail_entry - chain.first_rule_entry);
            for record in entries
                .by_ref()
                .take(chain.tail_entry - chain.first_rule_entry)
            {
                let target = self.layout.resolve_target(&record.target)?;
                rules.push(
                    Rule::new(record.base, record.extensions, target)
                        .map_err(|_| DecodeError::InvalidTable)?,
                );
            }
            let tail_entry = entries.next().ok_or(DecodeError::InvalidLayout)?;
            let chain = match (chain.hook, name, tail_entry.target) {
                (Some(hook), None, EntryTarget::Action(Target::Verdict(policy))) => {
                    Chain::built_in(hook, policy, rules)
                }
                (None, Some(name), EntryTarget::Action(Target::Return)) => {
                    Chain::user_defined(name, rules).map_err(|_| DecodeError::InvalidTable)?
                }
                _ => return Err(DecodeError::InvalidLayout),
            };
            chains.push(chain);
        }
        FilterTable::new(chains).map_err(|_| DecodeError::InvalidTable)
    }
}

/// Validated chain boundaries and user-chain entries, all in record indices.
struct ImportLayout {
    chains: Vec<ImportedChain>,
    user_entries: BTreeMap<usize, ChainId>,
}

impl ImportLayout {
    fn new(entries: &[TableEntry], mut chains: Vec<ImportedChain>) -> Result<Self, DecodeError> {
        for (head_entry, record) in entries.iter().enumerate() {
            if matches!(record.target, EntryTarget::ChainMarker(_)) {
                if chains.len() == MAX_CHAINS {
                    return Err(DecodeError::TooLarge);
                }
                chains.push(ImportedChain {
                    head_entry,
                    first_rule_entry: head_entry + 1,
                    tail_entry: 0,
                    hook: None,
                });
            }
        }
        chains.sort_unstable_by_key(|chain| chain.head_entry);
        if chains[0].head_entry != 0 {
            return Err(DecodeError::InvalidLayout);
        }
        for index in 0..chains.len() {
            let end = chains
                .get(index + 1)
                .map_or(entries.len(), |chain| chain.head_entry);
            let chain = &mut chains[index];
            if chain.first_rule_entry >= end
                || (chain.hook.is_some() && chain.tail_entry != end - 1)
            {
                return Err(DecodeError::InvalidLayout);
            }
            chain.tail_entry = end - 1;
            let tail_entry = &entries[chain.tail_entry];
            let is_valid_target = matches!(
                (&tail_entry.target, chain.hook),
                (EntryTarget::Action(Target::Verdict(_)), Some(_))
                    | (EntryTarget::Action(Target::Return), None)
            );
            if !is_valid_target
                || tail_entry.base != BaseMatch::default()
                || !tail_entry.extensions.is_empty()
            {
                return Err(DecodeError::InvalidLayout);
            }
        }
        let user_entries: BTreeMap<_, _> = chains
            .iter()
            .enumerate()
            .filter(|(_, chain)| chain.hook.is_none())
            .map(|(index, chain)| (chain.first_rule_entry, ChainId::from_index(index)))
            .collect();
        Ok(Self {
            chains,
            user_entries,
        })
    }

    fn resolve_target(&self, target: &EntryTarget) -> Result<Target, DecodeError> {
        Ok(match target {
            EntryTarget::Action(action) => *action,
            EntryTarget::JumpToEntry(index) => Target::Jump(
                *self
                    .user_entries
                    .get(index)
                    .ok_or(DecodeError::InvalidLayout)?,
            ),
            EntryTarget::ChainMarker(_) => return Err(DecodeError::InvalidLayout),
        })
    }
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
        let icmp_type = (icmp.icmp_type != u8::MAX).then_some(icmp.icmp_type);
        Self::new(icmp_type, icmp.code[0], icmp.code[1])
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

unittest::path_mod_test!(
    pub(super) mod table_tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/decode/table.rs"
);
