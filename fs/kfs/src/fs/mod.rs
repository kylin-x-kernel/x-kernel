#[cfg(feature = "fat")]
mod fat;

#[cfg(feature = "ext4")]
mod ext4;

use cfg_if::cfg_if;
use fs_ng_vfs::{Filesystem, VfsResult};
use kdriver::BlockDevice as KBlockDevice;

pub fn new_default(_dev: KBlockDevice) -> VfsResult<Filesystem> {
    cfg_if! {
        if #[cfg(feature = "ext4")] {
            ext4::Ext4Filesystem::new(_dev)
        } else if #[cfg(feature = "fat")] {
            Ok(fat::FatFilesystem::new(_dev))
        } else {
            panic!("No filesystem feature enabled");
        }
    }
}
