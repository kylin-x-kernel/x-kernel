// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String};
use core::fmt;

use hex;
use tee_raw_sys::{self as raw, TEE_ERROR_BAD_FORMAT};
use uuid as uuid_crate;

use crate::tee::TeeResult;

/// A Universally Unique Resource Identifier (UUID) type as defined in RFC4122.
/// The value is used to identify a trusted application.
#[derive(Copy, Clone, Default)]
pub struct Uuid {
    raw: raw::TEE_UUID,
}

impl Uuid {
    /// Parses a Uuid from a string of hexadecimal digits with optional hyphens.
    pub fn parse_str(input: &str) -> TeeResult<Uuid> {
        let uuid = uuid_crate::Uuid::parse_str(input).map_err(|_| TEE_ERROR_BAD_FORMAT)?;
        let (time_low, time_mid, time_hi_and_version, clock_seq_and_node) = uuid.as_fields();
        Ok(Self::new_raw(
            time_low,
            time_mid,
            time_hi_and_version,
            *clock_seq_and_node,
        ))
    }

    /// Returns the TA Unix socket path for this UUID under `/tmp`.
    pub fn ta_unix_socket_path(&self) -> String {
        format!("/tmp/{}.sock", self)
    }

    /// Creates a raw TEE client uuid object with specified parameters.
    pub fn new_raw(
        time_low: u32,
        time_mid: u16,
        time_hi_and_version: u16,
        clock_seq_and_nod: [u8; 8],
    ) -> Uuid {
        let raw_uuid = raw::TEE_UUID {
            timeLow: time_low,
            timeMid: time_mid,
            timeHiAndVersion: time_hi_and_version,
            clockSeqAndNode: clock_seq_and_nod,
        };
        Self { raw: raw_uuid }
    }

    /// Converts a uuid to a raw `TEE_UUID` reference.
    pub fn as_raw_ref(&self) -> &raw::TEE_UUID {
        &self.raw
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{}-{}",
            self.raw.timeLow,
            self.raw.timeMid,
            self.raw.timeHiAndVersion,
            hex::encode(&self.raw.clockSeqAndNode[0..2]),
            hex::encode(&self.raw.clockSeqAndNode[2..8]),
        )
    }
}

impl From<raw::TEE_UUID> for Uuid {
    fn from(raw: raw::TEE_UUID) -> Self {
        Uuid { raw }
    }
}

/// Builds `/tmp/{uuid}.sock` after validating `uuid` as RFC4122.
///
/// Rejects path traversal and other non-UUID characters before the path is
/// passed to the Unix socket layer.
pub fn ta_unix_socket_path(uuid: &str) -> TeeResult<String> {
    Ok(Uuid::parse_str(uuid)?.ta_unix_socket_path())
}

#[cfg(unittest)]
mod tests {
    use tee_raw_sys::TEE_ERROR_BAD_FORMAT;

    use super::*;

    #[test]
    fn ta_unix_socket_path_valid_uuid() {
        let path = ta_unix_socket_path("936da01f-9abd-4d9d-80c7-02af85c822a8").unwrap();
        assert_eq!(path, "/tmp/936da01f-9abd-4d9d-80c7-02af85c822a8.sock");
    }

    #[test]
    fn ta_unix_socket_path_rejects_path_traversal() {
        assert_eq!(ta_unix_socket_path("../evil"), Err(TEE_ERROR_BAD_FORMAT));
    }

    #[test]
    fn ta_unix_socket_path_rejects_slashes() {
        assert_eq!(
            ta_unix_socket_path("../../etc/passwd"),
            Err(TEE_ERROR_BAD_FORMAT)
        );
    }
}
