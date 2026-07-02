// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_uint;

use khal::uspace::UserContext;
use linux_sysno::Sysno;
use tee_raw_sys::TEE_ERROR_NOT_SUPPORTED;

use crate::tee::{
    tee_cancel::{
        sys_tee_scn_get_cancellation_flag, sys_tee_scn_mask_cancellation,
        sys_tee_scn_unmask_cancellation,
    },
    tee_generic::{sys_tee_scn_log, sys_tee_scn_panic, sys_tee_scn_return},
    tee_inter_ta::{
        sys_tee_scn_close_ta_session, sys_tee_scn_invoke_ta_command, sys_tee_scn_open_ta_session,
    },
    tee_property::{sys_tee_scn_get_property, sys_tee_scn_get_property_name_to_index},
    tee_svc_cryp::{
        syscall_cryp_obj_alloc, syscall_cryp_obj_close, syscall_cryp_obj_copy,
        syscall_cryp_obj_get_attr, syscall_cryp_obj_get_info, syscall_cryp_obj_populate,
        syscall_cryp_obj_reset, syscall_cryp_obj_restrict_usage, syscall_obj_generate_key,
    },
    tee_svc_cryp2::{
        syscall_asymm_operate, syscall_asymm_verify, syscall_authenc_dec_final,
        syscall_authenc_enc_final, syscall_authenc_init, syscall_authenc_update_aad,
        syscall_authenc_update_payload, syscall_cipher_final, syscall_cipher_init,
        syscall_cipher_update, syscall_cryp_random_number_generate, syscall_cryp_state_alloc,
        syscall_cryp_state_copy, syscall_cryp_state_free, syscall_hash_final, syscall_hash_init,
        syscall_hash_update,
    },
    tee_svc_storage::{
        syscall_storage_alloc_enum, syscall_storage_free_enum, syscall_storage_next_enum,
        syscall_storage_obj_create, syscall_storage_obj_del, syscall_storage_obj_open,
        syscall_storage_obj_read, syscall_storage_obj_rename, syscall_storage_obj_seek,
        syscall_storage_obj_trunc, syscall_storage_obj_write, syscall_storage_reset_enum,
        syscall_storage_start_enum,
    },
    tee_time::{sys_tee_scn_get_time, sys_tee_scn_set_ta_time, sys_tee_scn_wait},
};

#[macro_use]
mod macros;

mod arch;
mod bitstring;
mod common;
mod config;
mod crypto;
mod curve25519_key;
mod fs_dirfile;
mod fs_htree;
#[cfg(unittest)]
mod fs_htree_tests;
mod huk_subkey;

mod libutee;
mod memtag;
mod otp_stubs;
mod protocal;
mod ree_fs_rpc;
mod rng_software;
mod tee_api_defines_extensions;
mod tee_cancel;
mod tee_fs;
mod tee_fs_key_manager;
mod tee_generic;
mod tee_inter_ta;
mod tee_misc;
mod tee_obj;
mod tee_pobj;
mod tee_property;
mod tee_ree_fs;
mod tee_session;
mod tee_svc_cryp;
mod tee_svc_cryp2;
mod tee_svc_storage;
mod tee_ta_manager;
mod tee_time;
mod types_ext;
mod user_access;
mod user_ta;
mod utee_defines;
mod utils;
mod uuid;
mod vm;
pub type TeeResult<T = ()> = Result<T, u32>;

pub use tee_api_defines_extensions::*;
#[cfg(unittest)]
pub use tee_session::set_tee_session_ctx;
pub use tee_session::tee_session_release_state;

/// 7th syscall register argument (see NOTE TODO in `dispatch_irq_tee_syscall`).
fn tee_syscall_arg6(uctx: &UserContext) -> usize {
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "aarch64")] {
            uctx.x[6] as usize
        } else if #[cfg(target_arch = "x86_64")] {
            uctx.r12 as usize
        } else {
            0
        }
    }
}

/// 7th and 8th syscall register arguments for `tee_scn_storage_obj_create`.
fn tee_syscall_storage_create_extras(uctx: &UserContext) -> (usize, *mut c_uint) {
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "aarch64")] {
            (uctx.x[6] as usize, uctx.x[7] as *mut c_uint)
        } else if #[cfg(target_arch = "x86_64")] {
            (uctx.r12 as usize, uctx.r13 as *mut c_uint)
        } else {
            (0, core::ptr::null_mut())
        }
    }
}

/// Dispatch TEE-specific syscalls from the userspace context
pub fn dispatch_irq_tee_syscall(sysno: Sysno, uctx: &mut UserContext) -> TeeResult {
    // Handle TEE-specific syscalls here
    match sysno {
        Sysno::tee_scn_return => sys_tee_scn_return(uctx.arg0() as _),
        Sysno::tee_scn_log => sys_tee_scn_log(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::tee_scn_panic => sys_tee_scn_panic(uctx.arg0() as _),
        Sysno::tee_scn_get_property => {
            let prop_type: usize = 0;
            sys_tee_scn_get_property(
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
                prop_type as _,
            )
        }
        Sysno::tee_scn_get_property_name_to_index => sys_tee_scn_get_property_name_to_index(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::tee_scn_open_ta_session => sys_tee_scn_open_ta_session(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::tee_scn_close_ta_session => sys_tee_scn_close_ta_session(uctx.arg0() as _),
        Sysno::tee_scn_invoke_ta_command => sys_tee_scn_invoke_ta_command(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::tee_scn_get_cancellation_flag => sys_tee_scn_get_cancellation_flag(uctx.arg0() as _),
        Sysno::tee_scn_unmask_cancellation => sys_tee_scn_unmask_cancellation(uctx.arg0() as _),
        Sysno::tee_scn_mask_cancellation => sys_tee_scn_mask_cancellation(uctx.arg0() as _),
        Sysno::tee_scn_wait => sys_tee_scn_wait(uctx.arg0() as _),
        Sysno::tee_scn_get_time => sys_tee_scn_get_time(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::tee_scn_set_ta_time => sys_tee_scn_set_ta_time(uctx.arg1() as _),

        Sysno::tee_scn_cryp_state_alloc => syscall_cryp_state_alloc(
            uctx.arg0(),
            uctx.arg1(),
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),

        Sysno::tee_scn_cryp_state_copy => syscall_cryp_state_copy(uctx.arg0(), uctx.arg1()),

        Sysno::tee_scn_cryp_state_free => syscall_cryp_state_free(uctx.arg0()),

        Sysno::tee_scn_hash_init => syscall_hash_init(uctx.arg0()),

        Sysno::tee_scn_hash_update => syscall_hash_update(uctx.arg0(), uctx.arg1(), uctx.arg2()),

        Sysno::tee_scn_hash_final => syscall_hash_final(
            uctx.arg0(),
            uctx.arg1(),
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),

        Sysno::tee_scn_cipher_init => syscall_cipher_init(uctx.arg0(), uctx.arg1(), uctx.arg2()),

        Sysno::tee_scn_cipher_update => syscall_cipher_update(
            uctx.arg0(),
            uctx.arg1(),
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),

        Sysno::tee_scn_cipher_final => syscall_cipher_final(
            uctx.arg0(),
            uctx.arg1(),
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),

        Sysno::tee_scn_cryp_obj_get_info => {
            syscall_cryp_obj_get_info(uctx.arg0() as _, uctx.arg1() as _)
        }

        Sysno::tee_scn_cryp_obj_restrict_usage => {
            syscall_cryp_obj_restrict_usage(uctx.arg0() as _, uctx.arg1() as _)
        }

        Sysno::tee_scn_cryp_obj_get_attr => syscall_cryp_obj_get_attr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        Sysno::tee_scn_cryp_obj_alloc => {
            syscall_cryp_obj_alloc(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_cryp_obj_close => syscall_cryp_obj_close(uctx.arg0() as _),

        Sysno::tee_scn_cryp_obj_reset => syscall_cryp_obj_reset(uctx.arg0() as _),

        Sysno::tee_scn_cryp_obj_populate => {
            syscall_cryp_obj_populate(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_cryp_obj_copy => syscall_cryp_obj_copy(uctx.arg0() as _, uctx.arg1() as _),

        Sysno::tee_scn_cryp_random_number_generate => {
            syscall_cryp_random_number_generate(uctx.arg0() as _, uctx.arg1() as _)
        }

        Sysno::tee_scn_authenc_init => syscall_authenc_init(
            uctx.arg0(),
            uctx.arg1(),
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
            uctx.arg5(),
        ),

        Sysno::tee_scn_authenc_update_aad => {
            syscall_authenc_update_aad(uctx.arg0(), uctx.arg1(), uctx.arg2())
        }

        Sysno::tee_scn_authenc_update_payload => syscall_authenc_update_payload(
            uctx.arg0(),
            uctx.arg1(),
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),

        // NOTE TODO: Syscalls with 7+ register arguments — `tee_scn_authenc_enc_final`,
        // `tee_scn_authenc_dec_final`, `tee_scn_asymm_operate`, `tee_scn_asymm_verify`, and
        // `tee_scn_storage_obj_create` — pass arg0–arg5 via `uctx.arg0()`..`arg5()` on all
        // architectures; extra parameter(s) are read from arch-specific registers (see
        // rust-libutee `syscalls/arch/`). Only aarch64 (x6/x7) and x86_64 (r12/r13) are
        // wired up here. On riscv64/loongarch64 the extras default to 0 until kcpu and TA
        // stubs agree on the ABI (7th → a6; 8th → user stack or a spare register).
        Sysno::tee_scn_authenc_enc_final => {
            let tag_len = tee_syscall_arg6(uctx);
            syscall_authenc_enc_final(
                uctx.arg0(),
                uctx.arg1(),
                uctx.arg2(),
                uctx.arg3(),
                uctx.arg4(),
                uctx.arg5(),
                tag_len,
            )
        }

        Sysno::tee_scn_authenc_dec_final => {
            let tag_len = tee_syscall_arg6(uctx);
            syscall_authenc_dec_final(
                uctx.arg0(),
                uctx.arg1(),
                uctx.arg2(),
                uctx.arg3(),
                uctx.arg4(),
                uctx.arg5(),
                tag_len,
            )
        }

        Sysno::tee_scn_asymm_operate => {
            let dst_len = tee_syscall_arg6(uctx);
            syscall_asymm_operate(
                uctx.arg0(),
                uctx.arg1(),
                uctx.arg2(),
                uctx.arg3(),
                uctx.arg4(),
                uctx.arg5(),
                dst_len,
            )
        }

        Sysno::tee_scn_asymm_verify => {
            let sig_len = tee_syscall_arg6(uctx);
            syscall_asymm_verify(
                uctx.arg0(),
                uctx.arg1(),
                uctx.arg2(),
                uctx.arg3(),
                uctx.arg4(),
                uctx.arg5(),
                sig_len,
            )
        }

        Sysno::tee_scn_storage_obj_open => syscall_storage_obj_open(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),

        Sysno::tee_scn_storage_obj_create => {
            let (len, obj_ptr) = tee_syscall_storage_create_extras(uctx);
            syscall_storage_obj_create(
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
                len as _,
                obj_ptr as _,
            )
        }

        Sysno::tee_scn_storage_obj_del => syscall_storage_obj_del(uctx.arg0() as _),

        Sysno::tee_scn_storage_obj_rename => {
            syscall_storage_obj_rename(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_storage_enum_alloc => syscall_storage_alloc_enum(uctx.arg0() as _),

        Sysno::tee_scn_storage_enum_free => syscall_storage_free_enum(uctx.arg0() as _),

        Sysno::tee_scn_storage_enum_reset => syscall_storage_reset_enum(uctx.arg0() as _),

        Sysno::tee_scn_storage_enum_start => {
            syscall_storage_start_enum(uctx.arg0() as _, uctx.arg1() as _)
        }

        Sysno::tee_scn_storage_enum_next => syscall_storage_next_enum(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        Sysno::tee_scn_storage_obj_read => syscall_storage_obj_read(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        Sysno::tee_scn_storage_obj_write => {
            syscall_storage_obj_write(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_storage_obj_trunc => {
            syscall_storage_obj_trunc(uctx.arg0() as _, uctx.arg1() as _)
        }

        Sysno::tee_scn_storage_obj_seek => {
            syscall_storage_obj_seek(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_cryp_obj_generate_key => syscall_obj_generate_key(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}
