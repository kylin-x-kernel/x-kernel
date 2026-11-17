// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Control files for the cpu controller.
//!
//! `cpu.weight` is the enforced channel: a write re-applies the translated
//! nice value to every member process, so the fair scheduler's arbitration
//! changes immediately. `cpu.max` follows Linux's interface contract (parse
//! and validation) and stores the values a future scheduler bandwidth hook
//! will consume; `cpu.stat` reports the counters that exist today.

use alloc::{format, sync::Arc, vec::Vec};

use kcgroup::{Cgroup, CpuController, DEFAULT_PERIOD};
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsResult};

use crate::{command, control};

pub(crate) fn files(
    fs: Arc<SimpleFs>,
    cgroup: Arc<Cgroup>,
) -> Vec<(&'static str, Arc<SimpleFile>)> {
    // Linux marks cpu.max and cpu.weight CFTYPE_NOT_ON_ROOT; only cpu.stat is
    // visible at the hierarchy root.
    if cgroup.parent().is_none() {
        return alloc::vec![("cpu.stat", stat_file(fs, cgroup))];
    }
    alloc::vec![
        ("cpu.max", max_file(fs.clone(), cgroup.clone())),
        ("cpu.weight", weight_file(fs.clone(), cgroup.clone())),
        ("cpu.stat", stat_file(fs, cgroup)),
    ]
}

fn controller(cgroup: &Cgroup) -> VfsResult<Arc<CpuController>> {
    cgroup
        .cpu_controller()
        .map_err(|_| VfsError::OperationNotPermitted)
}

fn max_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let controller = controller(&cgroup)?;
                let text = match controller.quota() {
                    Some(quota) => format!("{} {}\n", quota, controller.period()),
                    None => format!("max {}\n", controller.period()),
                };
                Ok(Some(text))
            }
            SimpleFileOperation::Write { data, .. } => {
                let controller = controller(&cgroup)?;
                let (quota, period_us) = parse_cpu_max(command::parse_command(data)?)?;
                controller.set_bandwidth(quota, period_us)?;
                Ok(None)
            }
        }),
    )
}

/// Parses `cpu.max` body: `"$MAX $PERIOD"` or `"max $PERIOD"`.
fn parse_cpu_max(text: &str) -> VfsResult<(Option<u64>, u64)> {
    let text = text.trim();
    let mut fields = text.split_ascii_whitespace();
    let quota = match fields.next() {
        Some("max") => None,
        Some(value) => Some(value.parse::<u64>().map_err(|_| VfsError::InvalidInput)?),
        None => return Err(VfsError::InvalidInput),
    };
    let period_us = fields
        .next()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| VfsError::InvalidInput)?
        .unwrap_or(DEFAULT_PERIOD);
    Ok((quota, period_us))
}

fn weight_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => Ok(Some(format!("{}\n", controller(&cgroup)?.weight()))),
            SimpleFileOperation::Write { data, .. } => {
                let controller = controller(&cgroup)?;
                let weight: u32 = command::parse_command(data)?
                    .trim()
                    .parse()
                    .map_err(|_| VfsError::InvalidInput)?;
                controller.set_weight(weight)?;
                apply_weight(cgroup.as_ref(), weight)?;
                Ok(None)
            }
        }),
    )
}

fn stat_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let controller = controller(&cgroup)?;
        let (nr_periods, nr_throttled, throttled_usec) = controller.bandwidth_counters();
        Ok(format!(
            "usage_usec 0\nuser_usec 0\nsystem_usec 0\nnr_periods {}\nnr_throttled \
             {}\nthrottled_usec {}\n",
            nr_periods, nr_throttled, throttled_usec
        ))
    })
}

/// Re-applies the cgroup weight to every member process.
fn apply_weight(cgroup: &Cgroup, weight: u32) -> VfsResult<()> {
    for pid in kprocess::cgroup_member_process_ids(cgroup) {
        let Ok(process) = kprocess::scheduler::target_process(pid) else {
            continue;
        };
        for task in kprocess::scheduler::process_tasks(&process) {
            ktask::set_task_weight(&task, weight);
        }
    }
    Ok(())
}
