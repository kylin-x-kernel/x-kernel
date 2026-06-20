// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[unittest::mod_test]
pub mod tests_user_access {
    use alloc::{boxed::Box, vec};

    use unittest::{assert, assert_eq};

    use crate::tee::TeeResult;

    fn bb_alloc(len: usize) -> TeeResult<Box<[u8]>> {
        Ok(vec![0u8; len as _].into_boxed_slice())
    }

    fn bb_free(kbuf: Box<[u8]>, len: usize) {
        drop(kbuf);
        let _ = len;
    }

    #[unittest::def_test]
    fn test_bb_alloc_free() {
        let kbuf = bb_alloc(10).unwrap();
        assert!(kbuf.iter().all(|byte| *byte == 0));
        assert_eq!(kbuf.len(), 10);
        bb_free(kbuf, 10);
    }
}
