// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(non_camel_case_types, non_snake_case)]
#![allow(unused)]
#![allow(missing_docs)]
#![allow(non_upper_case_globals)]

use core::{arch::asm, ffi::c_uint};

use cfg_if::cfg_if;
use khal::uspace::UserContext;
use linux_sysno::Sysno;
use tee_raw_sys::{TEE_ERROR_NOT_SUPPORTED, TeeTime};

#[cfg(feature = "tee_test")]
use crate::tee::test_unit_test::sys_tee_scn_test;
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
    tee_svc_cryp2::{sys_tee_scn_hash_final, sys_tee_scn_hash_init, sys_tee_scn_hash_update},
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

mod bitstring;
mod common;
mod config;
mod crypto;
mod crypto_temp;
mod fs_dirfile;
mod fs_htree;
#[cfg(feature = "tee_test")]
mod fs_htree_tests;
mod huk_subkey;
mod libmbedtls;
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
#[cfg(feature = "x86_csv")]
mod tee_get_sealing_key;
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
#[cfg(feature = "tee_test")]
pub mod test_unit_test;
mod types_ext;
mod user_access;
mod user_ta;
mod utee_defines;
mod utils;
mod uuid;
mod vm;
pub type TeeResult<T = ()> = Result<T, u32>;

pub use tee_api_defines_extensions::*;

/// Dispatch TEE-specific syscalls from the userspace context
pub fn dispatch_irq_tee_syscall(sysno: Sysno, uctx: &mut UserContext) -> TeeResult {
    // Handle TEE-specific syscalls here
    match sysno {
        Sysno::tee_scn_return => sys_tee_scn_return(uctx.arg0() as _),
        Sysno::tee_scn_log => sys_tee_scn_log(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::tee_scn_panic => sys_tee_scn_panic(uctx.arg0() as _),
        Sysno::tee_scn_get_property => {
            let prop_type: usize = 0;
            // unsafe {
            //     asm!(
            //         "mov {0}, x6",
            //         out(reg) prop_type,
            //     );
            // }
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
        Sysno::tee_scn_get_time => {
            let teetime_ptr = uctx.arg1() as *mut TeeTime;
            let teetime_ref = unsafe { &mut *teetime_ptr };
            sys_tee_scn_get_time(uctx.arg0() as _, teetime_ref)
        }
        Sysno::tee_scn_set_ta_time => {
            let teetime_ptr = uctx.arg1() as *const TeeTime;
            let teetime_ref = unsafe { &*teetime_ptr };
            sys_tee_scn_set_ta_time(teetime_ref)
        }

        // Sysno::tee_scn_hash_init => sys_tee_scn_hash_init(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::tee_scn_hash_init => {
            sys_tee_scn_hash_init(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_hash_update => {
            sys_tee_scn_hash_update(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        Sysno::tee_scn_hash_update => sys_tee_scn_hash_final(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
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

        Sysno::tee_scn_storage_obj_open => syscall_storage_obj_open(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),

        Sysno::tee_scn_storage_obj_create => {
            let mut len: usize = 0;
            let mut obj_ptr: *mut c_uint = core::ptr::null_mut();

            cfg_if::cfg_if! {
                if #[cfg(target_arch = "aarch64")] {
                    unsafe {
                        asm!(
                            "mov {len}, x6",
                            "mov {obj}, x7",
                            len = out(reg) len,
                            obj = out(reg) obj_ptr,
                            options(nostack, preserves_flags),
                        );
                    }
                }
            }
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
        #[cfg(feature = "tee_test")]
        Sysno::tee_scn_test => sys_tee_scn_test(),
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}
