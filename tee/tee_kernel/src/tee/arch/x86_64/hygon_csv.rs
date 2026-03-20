// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{arch::asm, mem::size_of, ptr, slice};

use khal::mem::{VirtAddr, v2p};
use ksync::Mutex;
use mbedtls::hash::{Hkdf, Md, Type as MdType};
use tee_raw_sys::{
    TEE_ALG_HMAC_SM3, TEE_ALG_SM3, TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_BAD_STATE,
    TEE_OperationMode,
    TEE_OperationMode::{TEE_MODE_DIGEST, TEE_MODE_MAC},
    TEE_TYPE_HMAC_SM3, TEE_TYPE_SM4,
};

use super::hygon_csv_bindings::{
    PAGE_SIZE, csv_attestation_report_t, csv_guest_user_data_attestation_t,
};
use crate::tee::{
    TeeResult,
    utils::{random_bytes, slice_fmt},
};

/// Hypercall number for VM attestation (specific to Hygon platform)
const KVM_HC_VM_ATTESTATION: u64 = 100;

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
    let mut md = Md::new(MdType::SM3).map_err(|_| TEE_ERROR_BAD_STATE)?;
    md.update(&udata.user_data)
        .map_err(|_| TEE_ERROR_BAD_STATE)?;
    md.update(&udata.mnonce).map_err(|_| TEE_ERROR_BAD_STATE)?;
    md.finish(&mut udata.hash)
        .map_err(|_| TEE_ERROR_BAD_STATE)?;

    Ok(udata)
}

fn get_csv_report(user_data: &csv_guest_user_data_attestation_t) -> TeeResult<Vec<u8>> {
    let kernel_buf_size = PAGE_SIZE as usize;
    // Allocate a kernel buffer for the attestation request/response
    // We use a page-aligned buffer for the hypercall
    let mut kernel_buf = Box::new(alloc::vec![0u8; kernel_buf_size]);

    // Copy user_data to kernel buffer
    let user_data_size = size_of::<csv_guest_user_data_attestation_t>();
    let user_data_bytes =
        unsafe { slice::from_raw_parts(ptr::from_ref(user_data).cast::<u8>(), user_data_size) };
    kernel_buf[..user_data_size].copy_from_slice(user_data_bytes);

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
    let report_ptr = report_data.as_ptr() as *mut csv_attestation_report_t;
    let sealing_key = unsafe { (*report_ptr).sealing_key };

    Ok(sealing_key.to_vec())
}

pub fn get_huk_key(huk_key: &mut [u8]) -> TeeResult {
    let sealing_key = get_sealing_key()?;
    let salt = "Hygon CSV Sealing Key";
    Hkdf::hkdf(MdType::SM3, salt.as_bytes(), &sealing_key, &[], huk_key)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    // warn!("get_huk_key: huk_key: {:?}", slice_fmt(huk_key));
    Ok(())
}

#[cfg(all(target_arch = "x86_64", feature = "x86_csv"))]
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
