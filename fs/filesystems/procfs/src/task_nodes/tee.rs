// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(clippy::format_push_string)]

use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};

use kprocess::AsThread;
use ktask::{KtaskRef, WeakKtaskRef};
use kvfs::{
    Dentry, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile, SimpleFs, VfsError, VfsResult,
};
use tee_task_iface::tee_procfs::{
    has_ta_info as tee_has_ta_info, render_ta_ctx_uuid as tee_render_ta_ctx_uuid,
    render_ta_head as tee_render_ta_head,
};

use super::root;

pub fn has_ta_info(task: &KtaskRef) -> bool {
    task.as_thread()
        .process()
        .with_tee_ta_ctx(tee_has_ta_info)
        .unwrap_or(false)
}

pub fn render_ta_ctx_uuid(task: &KtaskRef) -> Vec<u8> {
    task.as_thread()
        .process()
        .with_tee_ta_ctx(tee_render_ta_ctx_uuid)
        .unwrap_or_default()
}

pub fn render_ta_head(task: &KtaskRef) -> Vec<u8> {
    task.as_thread()
        .process()
        .with_tee_ta_ctx(tee_render_ta_head)
        .unwrap_or_default()
}

struct TaInfoDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
}

impl SimpleDirOps for TaInfoDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(["uuid", "ta_head"].into_iter().map(Cow::Borrowed)))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        if name != "ta_head" && name != "uuid" {
            return Err(VfsError::NotFound);
        }
        let fs = self.fs.clone();
        let task_weak = self.task.clone();
        if task_weak.upgrade().is_none_or(|task| !has_ta_info(&task)) {
            return Err(VfsError::NotFound);
        }
        match name {
            "ta_head" => lookup.file(
                name,
                SimpleFile::new_regular(fs, {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = root::upgrade_task(&task_weak)?;
                        Ok(render_ta_head(&task))
                    }
                }),
            ),
            "uuid" => lookup.file(
                name,
                SimpleFile::new_regular(fs, {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = root::upgrade_task(&task_weak)?;
                        Ok(render_ta_ctx_uuid(&task))
                    }
                }),
            ),
            _ => return Err(VfsError::NotFound),
        }
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

pub fn make_ta_info_dir(
    lookup: SimpleDirLookup<'_>,
    name: &str,
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
) -> Dentry {
    lookup.dir(
        name,
        SimpleDir::new_maker(fs.clone(), Arc::new(TaInfoDir { fs, task })),
    )
}
