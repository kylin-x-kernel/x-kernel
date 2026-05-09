// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(clippy::format_push_string)]

use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};

use fs_ng_vfs::{VfsError, VfsResult};
use kcore::vfs::{NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs};
use ktask::{KtaskRef, WeakKtaskRef};
use kthread::AsThread;
use tee_task_iface::tee_procfs::{
    has_ta_info as tee_has_ta_info, render_ta_ctx_uuid as tee_render_ta_ctx_uuid,
    render_ta_head as tee_render_ta_head,
};

pub fn has_ta_info(task: &KtaskRef) -> bool {
    let proc_state = &task.as_thread().proc_state;
    let ta_ctx = proc_state.tee_ta_ctx.read();
    tee_has_ta_info(&ta_ctx)
}

pub fn render_ta_ctx_uuid(task: &KtaskRef) -> Vec<u8> {
    let proc_state = &task.as_thread().proc_state;
    let ta_ctx = proc_state.tee_ta_ctx.read();
    tee_render_ta_ctx_uuid(&ta_ctx)
}

pub fn render_ta_head(task: &KtaskRef) -> Vec<u8> {
    let proc_state = &task.as_thread().proc_state;
    let ta_ctx = proc_state.tee_ta_ctx.read();
    tee_render_ta_head(&ta_ctx)
}

struct TaInfoDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
}

impl SimpleDirOps for TaInfoDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(["uuid", "ta_head"].into_iter().map(Cow::Borrowed))
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        if name != "ta_head" && name != "uuid" {
            return Err(VfsError::NotFound);
        }
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        if !has_ta_info(&task) {
            return Err(VfsError::NotFound);
        }
        Ok(match name {
            "ta_head" => SimpleFile::new_regular(fs, move || Ok(render_ta_head(&task))).into(),
            "uuid" => SimpleFile::new_regular(fs, move || Ok(render_ta_ctx_uuid(&task))).into(),
            _ => return Err(VfsError::NotFound),
        })
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

pub fn make_ta_info_dir(fs: Arc<SimpleFs>, task: WeakKtaskRef) -> NodeOpsMux {
    NodeOpsMux::Dir(SimpleDir::new_maker(
        fs.clone(),
        Arc::new(TaInfoDir { fs, task }),
    ))
}
