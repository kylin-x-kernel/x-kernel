// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TEE unit test entry points.
use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

#[cfg(feature = "tee_test")]
use crate::tee::TeeResult;
use crate::tee::{
    bitstring::tests_bitstring::TEST_BITSTRING, common::file_ops::tests_file_ops::TEST_FILE_OPS,
    crypto::crypto_impl::tests_tee_crypto_impl::TEST_TEE_CRYPTO_IMPL,
    crypto_temp::aes_ecb::tests_aes_ecb::TEST_TEE_AES_ECB,
    fs_dirfile::tests_tee_fs_dirfile::TEST_TEE_FS_DIRFILE, fs_htree::tests_fs_htree::TEST_FS_HTREE,
    fs_htree_tests::tests_fs_htree_tests::TEST_FS_HTREE_TESTS,
    huk_subkey::tests_huk_subkey::TEST_HUK_SUBKEY_DERIVE,
    libmbedtls::bignum::tests_tee_bignum::TEST_TEE_BIGNUM,
    rng_software::tests_rng_software::TEST_RNG_SOFTWARE, tee_misc::tests_tee_misc::TEST_TEE_MISC,
    tee_obj::tests_tee_obj::TEST_TEE_OBJ, tee_pobj::tests_tee_pobj::TEST_TEE_POBJ,
    tee_ree_fs::tests_tee_ree_fs::TEST_TEE_REE_FS,
    tee_session::tests_tee_session::TEST_TEE_SESSION,
    tee_svc_cryp::tests_tee_svc_cryp::TEST_TEE_SVC_CRYP, tee_svc_cryp2::tests_cryp::TEST_TEE_CRYP,
    tee_svc_storage::tests_tee_svc_storage::TEST_TEE_SVC_STORAGE,
    user_access::tests_user_access::TEST_USER_ACCESS, utils::tests_utils::TEST_TEE_UTILS,
};

pub fn tee_unit_test() {
    warn!("********************************");
    warn!("Starting TEE unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(
        runner,
        [
            TEST_HUK_SUBKEY_DERIVE,
            TEST_TEE_POBJ,
            TEST_TEE_OBJ,
            TEST_TEE_SVC_CRYP,
            TEST_TEE_SVC_STORAGE,
            TEST_USER_ACCESS,
            TEST_TEE_BIGNUM,
            TEST_TEE_SESSION,
            TEST_FILE_OPS,
            TEST_TEE_FS_DIRFILE,
            TEST_TEE_UTILS,
            TEST_BITSTRING,
            TEST_TEE_MISC,
            TEST_TEE_REE_FS,
            TEST_FS_HTREE,
            TEST_FS_HTREE_TESTS,
            TEST_RNG_SOFTWARE,
            TEST_TEE_CRYPTO_IMPL,
            TEST_TEE_AES_ECB,
            TEST_TEE_CRYP,
        ]
    );

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}

#[cfg(feature = "tee_test")]
pub(crate) fn sys_tee_scn_test() -> TeeResult {
    tee_unit_test();

    Ok(())
}
