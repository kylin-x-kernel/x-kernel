// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use kerrno::KResult;
use ktask::KtaskRef;

use crate::{Pid, Process, ProcessGroup, Tid, publication::process_publication};

pub(crate) fn published_processes() -> Vec<Arc<Process>> {
    process_publication().published_processes()
}

pub(crate) fn published_process_count() -> usize {
    process_publication().published_process_count()
}

pub(crate) fn published_process(pid: Pid) -> KResult<Arc<Process>> {
    process_publication().published_process(pid)
}

pub(crate) fn task(tid: Tid) -> KResult<KtaskRef> {
    process_publication().task(tid)
}

pub(crate) fn process_group(pgid: Pid) -> KResult<Arc<ProcessGroup>> {
    process_publication().process_group(pgid)
}

pub(crate) fn live_process(pid: Pid) -> KResult<Arc<Process>> {
    process_publication().live_process(pid)
}

pub(crate) fn live_processes() -> Vec<Arc<Process>> {
    process_publication().live_processes()
}

pub(crate) fn unpublish_process_if_matches(process: &Arc<Process>) -> bool {
    process_publication().unpublish_process_if_matches(process)
}

pub(crate) fn cleanup_directory() {
    process_publication().cleanup();
}

pub(crate) fn task_snapshot() -> Vec<KtaskRef> {
    process_publication().tasks()
}

pub(crate) fn process_tasks(process: &Process) -> Vec<KtaskRef> {
    process.thread_tasks()
}

pub(crate) fn representative_task_for_process(process: &Process) -> KResult<KtaskRef> {
    process.representative_task()
}

pub(crate) fn process_has_published_tid(process: &Process, tid: Tid) -> bool {
    process.contains_published_tid(tid)
}
