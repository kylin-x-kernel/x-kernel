// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-host comparison of independent CPU slots, with pinned worker threads.

use std::{process::Command, sync::Barrier, time::Instant};

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    control::TimerControl,
    deadline::Expiration,
    set::TimerSet,
    timer::{CancelResult, Timer},
};

fn pin_current_thread(cpu: usize) -> std::io::Result<()> {
    let thread = std::fs::read_link("/proc/thread-self")?;
    let tid = thread.file_name().unwrap();
    let output = Command::new("taskset")
        .args(["-pc", &cpu.to_string()])
        .arg(tid)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(())
}

pub(super) fn run(host_cpus: [usize; 2], iterations: usize) {
    assert_ne!(host_cpus[0], host_cpus[1]);
    assert!((1..=100_000_000).contains(&iterations));
    static TIMERS: TimerSet<2> = TimerSet::new();
    let barrier = Barrier::new(3);
    let elapsed = std::thread::scope(|scope| {
        let workers = host_cpus.map(|host_cpu| {
            let barrier = &barrier;
            scope.spawn(move || {
                let pin_result = pin_current_thread(host_cpu);
                if pin_result.is_err() {
                    // Release successful peers before reporting setup failure.
                    barrier.wait();
                    return pin_result.map(|()| 0);
                }
                let owner = usize::from(host_cpu == host_cpus[1]);
                // Prepare on each worker before timing, avoiding shared allocator
                // traffic and including neither preparation nor thread startup.
                let timer = Timer::new(&TIMERS, |_: TimerControl<'_>, _: Expiration| {});
                let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(100));
                let cycle = || {
                    // SAFETY: each worker owns its timer and models one CPU;
                    // no dispatcher or hardware service runs in this workload.
                    let (actual, _) = unsafe { timer.start_on(owner.into(), deadline) }.unwrap();
                    assert_eq!(actual.as_usize(), owner);
                    assert_eq!(timer.try_cancel(), CancelResult::Removed);
                };
                for _ in 0..10_000 {
                    cycle();
                }
                barrier.wait();
                let start = Instant::now();
                for _ in 0..iterations {
                    cycle();
                }
                Ok(start.elapsed().as_nanos())
            })
        });
        barrier.wait();
        workers.map(|worker| worker.join().unwrap().expect("worker CPU pinning failed"))
    });
    // Report the slower worker; each cycle is one start plus one cancellation.
    let ns_per_cycle = *elapsed.iter().max().unwrap() as f64 / iterations as f64;
    println!("cpu0,cpu1,iterations,set_bytes,set_align,worker0_ns,worker1_ns,ns_per_cycle");
    println!(
        "{},{},{},{},{},{},{},{:.3}",
        host_cpus[0],
        host_cpus[1],
        iterations,
        size_of::<TimerSet<2>>(),
        align_of::<TimerSet<2>>(),
        elapsed[0],
        elapsed[1],
        ns_per_cycle
    );
}
