// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel filesystem initialization and high-level APIs.
#![cfg_attr(all(not(test), not(doc)), no_std)]
#![allow(dead_code, unused_imports, rustdoc::broken_intra_doc_links)]
#![feature(doc_cfg)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

#[macro_use]
extern crate log;

mod test_path_resolver;
mod test_working_context;

use kdriver::{BlockDevice as KBlockDevice, DeviceContainer, prelude::*};

#[cfg(feature = "fat")]
mod disk;
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod fs;

// New refactored components
mod fs_operations;
mod path_resolver;
mod working_context;

mod highlevel;
// Export new components (FsOperations for advanced use)
pub use fs_operations::FsOperations;
pub use highlevel::*;
pub use path_resolver::PathResolver;
pub use working_context::WorkingContext;

/// Initialize the filesystem subsystem and mount the root filesystem.
pub fn init_filesystems(mut block_devs: DeviceContainer<KBlockDevice>) {
    info!("Initialize filesystem subsystem...");

    let dev = {
        #[cfg(feature = "crosvm")]
        {
            // must have two block devices: secure and non-secure
            // we only use the second blk
            block_devs
                .take_nth(1)
                .expect("Less than two block devices found!")
        }
        #[cfg(not(feature = "crosvm"))]
        {
            block_devs.take_one().expect("No block device found!")
        }
    };
    info!("  use block device 0: {:?}", dev.name());

    let fs = fs::new_default(dev).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());

    let mp = fs_ng_vfs::Mountpoint::new_root(&fs);
    ROOT_FS_CONTEXT.call_once(|| FsContext::new(mp.root_location()));
}
