// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec};

use tee_raw_sys::*;

use super::TeeResult;

/// allocate memory from kernel
///
/// use for temporary memory allocation, can be optimized
pub fn bb_alloc(len: usize) -> TeeResult<Box<[u8]>> {
    let kbuf: Box<[u8]> = vec![0u8; len as _].into_boxed_slice();

    Ok(kbuf)
}

/// free memory to kernel
///
/// use for temporary memory allocation, can be optimized
pub fn bb_free(kbuf: Box<[u8]>, len: usize) {
    drop(kbuf);
    let _ = len;
}

/// Enter user access context
/// This enables safe access to user-space memory
#[inline(always)]
pub(crate) fn enter_user_access() {
    // Implementation would enable user memory access permissions
    // In OP-TEE, this dispatch_irqs entering user context for cryptographic operations
}

/// Exit user access context
/// This restores kernel/secure-world memory access permissions
#[inline(always)]
pub(crate) fn exit_user_access() {
    // Implementation would disable user memory access permissions
    // In OP-TEE, this dispatch_irqs returning from user context
}

#[unittest::mod_test]
pub mod tests_user_access {
    use unittest::{assert, assert_eq};

    use super::*;

    #[unittest::def_test]
    fn test_bb_alloc_free() {
        let kbuf = bb_alloc(10).unwrap();
        assert!(kbuf.iter().all(|byte| *byte == 0));
        assert_eq!(kbuf.len(), 10);
        bb_free(kbuf, 10);
    }
}
