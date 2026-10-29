// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared IPv4 packet context and match outcomes.

use crate::{
    buf::PacketBuf,
    ipv4::{Ipv4Error, Ipv4Header},
};

/// Shared packet fields and interface names for one table evaluation.
pub(super) struct MatchContext<'a> {
    header: Ipv4Header<'a>,
    payload: &'a [u8],
    indev: &'a str,
    outdev: &'a str,
}

impl<'a> MatchContext<'a> {
    /// Prepares shared match fields without repeating IPv4 validation.
    pub(super) fn new(
        packet: &'a PacketBuf,
        indev: &'a str,
        outdev: &'a str,
    ) -> Result<Self, Ipv4Error> {
        let header =
            Ipv4Header::parse_header_only(packet.network_packet().ok_or(Ipv4Error::Malformed)?)?;
        Ok(Self {
            header,
            payload: packet.transport_packet().ok_or(Ipv4Error::Malformed)?,
            indev,
            outdev,
        })
    }

    /// Borrows the IPv4 header shared by all conditions in this evaluation.
    pub(super) fn header(&self) -> &Ipv4Header<'a> {
        &self.header
    }

    /// Borrows the transport bytes within the normalized packet bounds.
    pub(super) fn payload(&self) -> &'a [u8] {
        self.payload
    }

    /// Returns the hook's input interface name, which may be empty.
    pub(super) fn input_interface(&self) -> &'a str {
        self.indev
    }

    /// Returns the hook's output interface name, which may be empty.
    pub(super) fn output_interface(&self) -> &'a str {
        self.outdev
    }
}

/// A condition's result, including an immediate packet-drop request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MatchOutcome {
    Matched,
    NotMatched,
    HotDrop,
}

impl From<bool> for MatchOutcome {
    fn from(is_matched: bool) -> Self {
        if is_matched {
            Self::Matched
        } else {
            Self::NotMatched
        }
    }
}
