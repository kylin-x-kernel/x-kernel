// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Basic virtual filesystem support

mod dev;
mod dir;
mod file;
mod fs;

use alloc::sync::Arc;

pub use dev::*;
pub use dir::*;
pub use file::*;
pub use fs::*;
use fs_ng_vfs::{DirNodeOps, FileNodeOps, WeakDirEntry};

/// A callback that builds a `Arc<dyn DirNodeOps>` for a given
/// `WeakDirEntry`.
pub type DirMaker = Arc<dyn Fn(WeakDirEntry) -> Arc<dyn DirNodeOps> + Send + Sync>;

/// An enum containing either a directory ([`DirMaker`]) or a file (`Arc<dyn
/// FileNodeOps>`).
#[derive(Clone)]
pub enum NodeOpsMux {
    /// A directory node.
    Dir(DirMaker),
    /// A file node.
    File(Arc<dyn FileNodeOps>),
}

impl From<DirMaker> for NodeOpsMux {
    fn from(maker: DirMaker) -> Self {
        Self::Dir(maker)
    }
}

impl<T: FileNodeOps> From<Arc<T>> for NodeOpsMux {
    fn from(ops: Arc<T>) -> Self {
        Self::File(ops)
    }
}

/// Unit tests.
#[cfg(unittest)]
pub mod tests_vfs {
    use alloc::sync::Arc;

    use fs_ng_vfs::{DirNodeOps, VfsError, WeakDirEntry};
    use unittest::{TestResult, def_test};

    use super::{DirMapping, NodeOpsMux, SimpleDirOps, dummy_stat_fs};

    fn dummy_dir_maker() -> super::DirMaker {
        Arc::new(|_this: WeakDirEntry| -> Arc<dyn DirNodeOps> {
            unimplemented!("dummy maker used only for tests")
        })
    }

    #[def_test]
    fn test_dummy_stat_fs_values() {
        let stat = dummy_stat_fs(0x1234);
        assert_eq!(stat.fs_type, 0x1234);
        assert_eq!(stat.block_size, 512);
        assert_eq!(stat.blocks, 100);
        assert_eq!(stat.name_length, fs_ng_vfs::path::MAX_NAME_LEN as _);
    }

    #[def_test]
    fn test_dirmapping_lookup() -> TestResult {
        let mut map = DirMapping::new();
        let maker = dummy_dir_maker();
        map.add("child", NodeOpsMux::from(maker));
        let entry = map.lookup_child("child").unwrap();
        match entry {
            NodeOpsMux::Dir(_) => {}
            _ => return TestResult::Failed,
        }
        TestResult::Ok
    }

    #[def_test]
    fn test_chained_dir_ops_lookup() {
        let mut left = DirMapping::new();
        let mut right = DirMapping::new();
        let maker = dummy_dir_maker();
        left.add("left", NodeOpsMux::from(maker.clone()));
        right.add("right", NodeOpsMux::from(maker));
        let chained = left.chain(right);
        assert!(chained.lookup_child("left").is_ok());
        assert!(chained.lookup_child("right").is_ok());
        assert!(matches!(
            chained.lookup_child("missing"),
            Err(VfsError::NotFound)
        ));
    }
}
