// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Control files for the memory controller.
//!
//! Accounting state lives in `memacct`; this adapter only renders the
//! hierarchical counters and translates control values. Every numeric limit is
//! stored as bytes, which matches both the cgroup v2 interface and the page
//! accounting it constrains.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kcgroup::Cgroup;
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsResult};

use crate::{command, control};

pub(crate) fn files(
    fs: Arc<SimpleFs>,
    cgroup: Arc<Cgroup>,
) -> Vec<(&'static str, Arc<SimpleFile>)> {
    let mut files = alloc::vec![
        ("memory.current", current_file(fs.clone(), cgroup.clone())),
        ("memory.peak", peak_file(fs.clone(), cgroup.clone())),
        ("memory.max", max_file(fs.clone(), cgroup.clone())),
        ("memory.high", high_file(fs.clone(), cgroup.clone())),
        ("memory.low", low_file(fs.clone(), cgroup.clone())),
        (
            "memory.swap.current",
            swap_current_file(fs.clone(), cgroup.clone()),
        ),
        ("memory.swap.max", swap_max_file(fs.clone(), cgroup.clone()),),
        ("memory.events", events_file(fs.clone(), cgroup.clone())),
        (
            "memory.events.local",
            events_local_file(fs.clone(), cgroup.clone()),
        ),
        ("memory.stat", stat_file(fs, cgroup.clone())),
    ];
    if cgroup.parent().is_none() {
        // Linux does not expose writable memory knobs on the hierarchy root.
        files.retain(|(name, _)| {
            !matches!(
                *name,
                "memory.peak" | "memory.max" | "memory.high" | "memory.low" | "memory.swap.max"
            )
        });
    }
    files
}

fn current_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        Ok(format!(
            "{}\n",
            cgroup.effective_memcg().hierarchical_usage()
        ))
    })
}

fn peak_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                Ok(Some(format!(
                    "{}\n",
                    cgroup.effective_memcg().hierarchical_peak()
                )))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                // Linux accepts any numeric write and resets the peak to the
                // current usage; the written value is not a new peak.
                let _ = parse_bytes(command::parse_command(data)?)?;
                cgroup.memory_controller()?.memcg().reset_peak();
                Ok(None)
            }
        }),
    )
}

fn max_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    limit_file(fs, cgroup, read_max, write_max)
}

fn high_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    limit_file(fs, cgroup, read_high, write_high)
}

fn low_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                let low = cgroup.memory_controller()?.memcg().low();
                Ok(Some(format!("{}\n", format_low(low))))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let value = command::parse_command(data)?;
                let low = if value == "max" {
                    u64::MAX
                } else {
                    parse_bytes(value)?
                };
                cgroup.memory_controller()?.memcg().set_low(low);
                Ok(None)
            }
        }),
    )
}

fn swap_current_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        // No swap backend exists, so no cgroup can have swapped-out pages.
        let _ = cgroup.memory_controller()?;
        Ok("0\n".to_string())
    })
}

fn swap_max_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                Ok(Some(match cgroup.memory_controller()?.memcg().swap_max() {
                    Some(limit) => format!("{limit}\n"),
                    None => "max\n".to_string(),
                }))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let value = command::parse_command(data)?;
                let limit = match value {
                    "max" => None,
                    "0" => Some(0),
                    // Without swap there is no size unit to admit another
                    // value: accepting it would expose a limit that cannot
                    // bound anything.
                    _ => return Err(VfsError::InvalidInput),
                };
                cgroup.memory_controller()?.memcg().set_swap_max(limit);
                Ok(None)
            }
        }),
    )
}

fn events_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        Ok(format_events(
            cgroup.effective_memcg().hierarchical_events(),
        ))
    })
}

fn events_local_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        Ok(format_events(cgroup.memory_controller()?.memcg().events()))
    })
}

fn stat_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        let stats = cgroup.effective_memcg().hierarchical_stats();
        Ok(format!("anon {}\nfile {}\n", stats.anon, stats.file))
    })
}

fn format_events(events: memacct::MemoryEvents) -> String {
    format!(
        "low {}\nhigh {}\nmax {}\noom {}\noom_kill {}\n",
        events.low, events.high, events.max, events.oom, events.oom_kill
    )
}

fn format_low(low: u64) -> String {
    if low == u64::MAX {
        "max".to_string()
    } else {
        format!("{low}")
    }
}

fn read_max(cgroup: &Cgroup) -> VfsResult<Option<u64>> {
    Ok(cgroup.memory_controller()?.memcg().max())
}

fn write_max(cgroup: &Cgroup, limit: Option<u64>) -> VfsResult<()> {
    cgroup.memory_controller()?.memcg().set_max(limit);
    Ok(())
}

fn read_high(cgroup: &Cgroup) -> VfsResult<Option<u64>> {
    Ok(cgroup.memory_controller()?.memcg().high())
}

fn write_high(cgroup: &Cgroup, limit: Option<u64>) -> VfsResult<()> {
    cgroup.memory_controller()?.memcg().set_high(limit);
    Ok(())
}

fn parse_bytes(value: &str) -> VfsResult<u64> {
    let (digits, shift) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 30),
        Some(b't' | b'T') => (&value[..value.len() - 1], 40),
        Some(b'p' | b'P') => (&value[..value.len() - 1], 50),
        Some(b'e' | b'E') => (&value[..value.len() - 1], 60),
        _ => (value, 0),
    };
    let number = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|_| VfsError::InvalidInput)?
    } else {
        digits.parse::<u64>().map_err(|_| VfsError::InvalidInput)?
    };
    number
        .checked_mul(1u64 << shift)
        .ok_or(VfsError::InvalidInput)
}

fn limit_file(
    fs: Arc<SimpleFs>,
    cgroup: Arc<Cgroup>,
    read: fn(&Cgroup) -> VfsResult<Option<u64>>,
    write: fn(&Cgroup, Option<u64>) -> VfsResult<()>,
) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                Ok(Some(match read(&cgroup)? {
                    Some(value) => format!("{value}\n"),
                    None => "max\n".to_string(),
                }))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let value = command::parse_command(data)?;
                let limit = match value {
                    "max" => None,
                    _ => Some(parse_bytes(value)?),
                };
                write(&cgroup, limit)?;
                Ok(None)
            }
        }),
    )
}
