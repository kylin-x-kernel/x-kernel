// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hygon CSV guest attestation and hardware-unique key (HUK) derivation.
//!
//! # FFI bindings
//!
//! [`super::hygon_csv_bindings`] is auto-generated from the Hygon CSV guest ABI
//! (see `tee/README.md` for the generator repo). Do **not** hand-edit bindings;
//! regenerate them when the vendor layout changes.
//!
//! Binding types such as [`super::hygon_csv_bindings::csv3_attestation_report_ext`]
//! mirror C `#[repr(C, packed)]` layouts exactly. Fields named `reserved` (for
//! example the 716-byte tail in `csv3_attestation_report_ext`) are ABI padding or
//! future extension slots. They are not application payload and must not be written
//! from external input or interpreted as trusted data.
//!
//! # Safety contract
//!
//! All attestation report access must stay in this module behind a wrapper that:
//!
//! - checks the hypercall/response buffer length before any field read;
//! - uses `offset_of!` (or generated offsets) plus explicit field sizes instead of
//!   whole-struct `copy_from_slice`, `transmute`, or direct serde; and
//! - keeps `const_assert!` / `const_assert_eq!` layout checks when adding new report
//!   formats.
//!
//! Other TEE code must call [`get_huk_key`] rather than importing binding structs.
//!
//! # Current report format
//!
//! The live path uses [`super::hygon_csv_bindings::csv_attestation_report_t`].
//! [`get_sealing_key`] reads `sealing_key` only after verifying
//! `report_data.len() >= CSV_SEALING_KEY_OFFSET + size_of::<CsvSealingKey>()`.
//!
//! CSV3 extended attestation (`csv3_attestation_report_ext`) is defined in bindings
//! but not used yet. When enabled, follow the same wrapper pattern: parse from a
//! bounded `&[u8]`, validate `len >= size_of::<Report>()`, then extract fields by
//! offset; ignore `reserved` on read and never expose it as writable storage.

use alloc::{boxed::Box, vec::Vec};
use core::mem::size_of;

use bytemuck::{Pod, Zeroable, bytes_of};
use khal::mem::{VirtAddr, v2p};
use memoffset::offset_of;
use static_assertions::{const_assert, const_assert_eq};
use tee_crypto::{
    hash::{Digest, Sm3},
    hkdf,
    mac::HmacSm3,
};
use tee_raw_sys::TEE_ERROR_BAD_PARAMETERS;

use super::hygon_csv_bindings::{
    PAGE_SIZE, csv_attestation_report_t, csv_guest_user_data_attestation_t,
};
use crate::tee::{TeeResult, utils::random_bytes};

/// Hypercall number for VM attestation (specific to Hygon platform)
const KVM_HC_VM_ATTESTATION: u64 = 100;

type CsvSealingKey = [u8; 32];
const CSV_SEALING_KEY_OFFSET: usize = offset_of!(csv_attestation_report_t, sealing_key);

#[repr(C, packed)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct CsvGuestUserDataAttestationBytes {
    user_data: [u8; 64],
    mnonce: [u8; 16],
    hash: [u8; 32],
}

const_assert_eq!(
    size_of::<CsvGuestUserDataAttestationBytes>(),
    size_of::<csv_guest_user_data_attestation_t>()
);
const_assert!(
    CSV_SEALING_KEY_OFFSET + size_of::<CsvSealingKey>() <= size_of::<csv_attestation_report_t>()
);

fn construct_user_data() -> TeeResult<Box<csv_guest_user_data_attestation_t>> {
    // Allocate and initialize the attestation user data structure
    let mut udata = Box::new(csv_guest_user_data_attestation_t {
        user_data: [0u8; 64],
        mnonce: [0u8; 16],
        hash: [0u8; 32],
    });

    // Fill user_data and mnonce with random bytes
    random_bytes(&mut udata.user_data);
    random_bytes(&mut udata.mnonce);

    // Compute SM3 hash of user_data || mnonce
    let mut hasher = Sm3::new();
    hasher.update(&udata.user_data);
    hasher.update(&udata.mnonce);
    let digest = hasher.finalize();
    let digest_len = size_of::<CsvSealingKey>().min(digest.len());
    udata.hash.copy_from_slice(&digest.as_bytes()[..digest_len]);

    Ok(udata)
}

fn get_csv_report(user_data: &csv_guest_user_data_attestation_t) -> TeeResult<Vec<u8>> {
    let kernel_buf_size = PAGE_SIZE as usize;
    // Allocate a kernel buffer for the attestation request/response
    // We use a page-aligned buffer for the hypercall
    let mut kernel_buf = Box::new(alloc::vec![0u8; kernel_buf_size]);

    // Copy user_data to kernel buffer
    let user_data_size = size_of::<csv_guest_user_data_attestation_t>();
    let user_data_bytes = CsvGuestUserDataAttestationBytes {
        user_data: user_data.user_data,
        mnonce: user_data.mnonce,
        hash: user_data.hash,
    };
    kernel_buf[..user_data_size].copy_from_slice(bytes_of(&user_data_bytes));

    // get physical addr
    let kernel_buf_pa = v2p(VirtAddr::from(kernel_buf.as_ptr() as usize));

    // Make the hypercall to request attestation report
    let _ret = kcpu::hypercall(
        KVM_HC_VM_ATTESTATION,
        kernel_buf_pa.as_usize() as u64,
        kernel_buf_size as u64,
    );

    Ok(*kernel_buf)
}

fn get_sealing_key() -> TeeResult<Vec<u8>> {
    let user_data = construct_user_data()?;
    let report_data = get_csv_report(&user_data)?;
    let sealing_key_end = CSV_SEALING_KEY_OFFSET + size_of::<CsvSealingKey>();
    if report_data.len() < sealing_key_end {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let sealing_key = bytemuck::pod_read_unaligned::<CsvSealingKey>(
        &report_data[CSV_SEALING_KEY_OFFSET..sealing_key_end],
    );

    Ok(sealing_key.to_vec())
}

pub fn get_huk_key(huk_key: &mut [u8]) -> TeeResult {
    let sealing_key = get_sealing_key()?;
    let salt = "Hygon CSV Sealing Key";
    let derived = hkdf::hkdf::<HmacSm3>(salt.as_bytes(), &sealing_key, &[], huk_key.len())
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    huk_key.copy_from_slice(&derived);
    Ok(())
}
#[cfg(feature = "csv_huk_key")]
#[unittest::mod_test]
pub mod tests_hygon_csv_get_sealing_key {
    use unittest::assert;

    use super::*;

    #[unittest::def_test]
    fn test_hygon_csv_get_sealing_key() {
        let result = get_sealing_key();
        assert!(result.is_ok());
        let key_buf = result.unwrap();
        assert!(key_buf.len() == 32);
        assert!(key_buf.iter().any(|&x| x != 0));

        let result2 = get_sealing_key();
        assert!(result2.is_ok());
        let key_buf2 = result2.unwrap();
        assert!(key_buf2.len() == 32);
        assert!(key_buf2.iter().any(|&x| x != 0));

        assert!(key_buf == key_buf2);
    }
}
