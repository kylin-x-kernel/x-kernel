// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Flat Fair PELT, using Linux v6.12's fixed-point arithmetic and time units.
//!
//! Reference: <https://github.com/torvalds/linux/blob/v6.12/kernel/sched/pelt.c>.
//! Time advances in PELT ticks of 1024 ns, with 1024 ticks per decay period. Capacity
//! and frequency scaling are neutral; callers serialize all state with the RQ.

const PERIOD_TICKS: u64 = 1024;
const LOAD_AVG_MAX: u64 = 47742;
const MIN_DIVIDER: u64 = LOAD_AVG_MAX - PERIOD_TICKS;
pub(crate) const CAPACITY: u64 = 1024;

// floor(2^32 * 2^(-n/32)), with Linux's n=0 approximation.
const DECAY: [u32; 32] = [
    0xffffffff, 0xfa83b2da, 0xf5257d14, 0xefe4b99a, 0xeac0c6e6, 0xe5b906e6, 0xe0ccdeeb, 0xdbfbb796,
    0xd744fcc9, 0xd2a81d91, 0xce248c14, 0xc9b9bd85, 0xc5672a10, 0xc12c4cc9, 0xbd08a39e, 0xb8fbaf46,
    0xb504f333, 0xb123f581, 0xad583ee9, 0xa9a15ab4, 0xa5fed6a9, 0xa2704302, 0x9ef5325f, 0x9b8d39b9,
    0x9837f050, 0x94f4efa8, 0x91c3d373, 0x8ea4398a, 0x8b95c1e3, 0x88980e80, 0x85aac367, 0x82cd8698,
];

/// Fair load history at the last scheduler update.
///
/// These are PELT averages, distinct from instantaneous runnable weights.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PeltAvg {
    /// Nice-weighted runnable history; a continuously runnable nice-0 task
    /// approaches 1024.
    pub load_avg: u64,
    /// Runnable history, including running tasks, in units of 1024 per task.
    pub runnable_avg: u64,
    /// Execution history in units of 1024 per fully occupied CPU. Migration
    /// and initial estimates can temporarily exceed a queue's CPU capacity.
    pub util_avg: u64,
}

fn decay(value: u64, periods: u64) -> u64 {
    if periods > 32 * 63 {
        return 0;
    }
    let value = value >> (periods / 32);
    ((u128::from(value) * u128::from(DECAY[(periods % 32) as usize])) >> 32) as u64
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pelt {
    pub(crate) avg: PeltAvg,
    last_update_ns: u64,
    period_contrib_ticks: u64,
    load_sum: u64,
    runnable_sum: u64,
    // Widen Linux's u32 accumulator so simultaneous incoming histories cannot
    // wrap when many CPUs migrate onto one queue.
    util_sum: u64,
}

impl Pelt {
    pub(crate) const fn new(now_ns: u64) -> Self {
        Self {
            avg: PeltAvg {
                load_avg: 0,
                runnable_avg: 0,
                util_avg: 0,
            },
            last_update_ns: now_ns,
            period_contrib_ticks: 0,
            load_sum: 0,
            runnable_sum: 0,
            util_sum: 0,
        }
    }

    fn divider(&self) -> u64 {
        MIN_DIVIDER + self.period_contrib_ticks
    }

    /// Account the interval preceding `now_ns`. Entity load is 0/1 with
    /// `weight` applied to its average; queue load is already weighted.
    pub(crate) fn update(
        &mut self,
        now_ns: u64,
        load: u64,
        runnable: u64,
        running: bool,
        weight: u64,
    ) {
        if now_ns < self.last_update_ns {
            self.last_update_ns = now_ns;
            return;
        }
        let elapsed = (now_ns - self.last_update_ns) >> 10;
        if elapsed == 0 {
            return;
        }
        self.last_update_ns += elapsed << 10;
        let units = elapsed + self.period_contrib_ticks;
        let periods = units / PERIOD_TICKS;
        let tail = units % PERIOD_TICKS;
        let contribution = if periods == 0 {
            elapsed
        } else {
            self.load_sum = decay(self.load_sum, periods);
            self.runnable_sum = decay(self.runnable_sum, periods);
            self.util_sum = decay(self.util_sum, periods);
            decay(PERIOD_TICKS - self.period_contrib_ticks, periods) + LOAD_AVG_MAX
                - decay(LOAD_AVG_MAX, periods)
                - PERIOD_TICKS
                + tail
        };
        self.period_contrib_ticks = tail;
        self.load_sum = self
            .load_sum
            .saturating_add(load.saturating_mul(contribution));
        if load != 0 {
            self.runnable_sum = self.runnable_sum.saturating_add(
                runnable
                    .saturating_mul(contribution)
                    .saturating_mul(CAPACITY),
            );
            if running {
                self.util_sum = self.util_sum.saturating_add(contribution * CAPACITY);
            }
        }
        if periods != 0 {
            self.reweight(weight);
            self.avg.runnable_avg = self.runnable_sum / self.divider();
            self.avg.util_avg = self.util_sum / self.divider();
        }
    }

    pub(crate) fn reweight(&mut self, weight: u64) {
        self.avg.load_avg = self.load_sum.saturating_mul(weight) / self.divider();
    }

    pub(crate) fn seed(&mut self, queue: &Self, weight: u64) {
        let cap = CAPACITY.saturating_sub(queue.avg.util_avg) / 2;
        let util = if queue.avg.util_avg == 0 {
            cap
        } else {
            (queue.avg.util_avg.saturating_mul(weight) / (queue.avg.load_avg + 1)).min(cap)
        };
        self.avg = PeltAvg {
            load_avg: weight,
            runnable_avg: util,
            util_avg: util,
        };
    }

    /// Reconstruct sums at the destination's decay phase, as on Linux attach.
    pub(crate) fn align(&mut self, queue: &Self, weight: u64) {
        self.last_update_ns = queue.last_update_ns;
        self.period_contrib_ticks = queue.period_contrib_ticks;
        let divider = self.divider();
        self.load_sum = (self.avg.load_avg.saturating_mul(divider) / weight).max(1);
        self.runnable_sum = self.avg.runnable_avg.saturating_mul(divider);
        self.util_sum = self.avg.util_avg.saturating_mul(divider);
    }

    pub(crate) fn attach(&mut self, entity: &Self, weight: u64) {
        self.avg.load_avg = self.avg.load_avg.saturating_add(entity.avg.load_avg);
        self.avg.runnable_avg = self
            .avg
            .runnable_avg
            .saturating_add(entity.avg.runnable_avg);
        self.avg.util_avg = self.avg.util_avg.saturating_add(entity.avg.util_avg);
        self.load_sum = self
            .load_sum
            .saturating_add(entity.load_sum.saturating_mul(weight));
        self.runnable_sum = self.runnable_sum.saturating_add(entity.runnable_sum);
        self.util_sum = self.util_sum.saturating_add(entity.util_sum);
    }

    pub(crate) fn detach(&mut self, entity: &Self, weight: u64) {
        // Entity and queue rounding differ. Saturating subtraction and the
        // minimum-divider floors prevent a later recompute losing live load.
        self.avg.load_avg = self.avg.load_avg.saturating_sub(entity.avg.load_avg);
        self.avg.runnable_avg = self
            .avg
            .runnable_avg
            .saturating_sub(entity.avg.runnable_avg);
        self.avg.util_avg = self.avg.util_avg.saturating_sub(entity.avg.util_avg);
        self.load_sum = self
            .load_sum
            .saturating_sub(entity.load_sum.saturating_mul(weight))
            .max(self.avg.load_avg.saturating_mul(MIN_DIVIDER));
        self.runnable_sum = self
            .runnable_sum
            .saturating_sub(entity.runnable_sum)
            .max(self.avg.runnable_avg.saturating_mul(MIN_DIVIDER));
        self.util_sum = self
            .util_sum
            .saturating_sub(entity.util_sum)
            .max(self.avg.util_avg.saturating_mul(MIN_DIVIDER));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoadState {
    New,
    Detached,
    Blocked,
    Runnable,
}

#[derive(Clone, Copy)]
pub(crate) struct EntityLoad {
    pub(crate) pelt: Pelt,
    pub(crate) state: LoadState,
}

impl EntityLoad {
    pub(crate) const fn new() -> Self {
        Self {
            pelt: Pelt::new(0),
            state: LoadState::New,
        }
    }

    pub(crate) fn is_attached(&self) -> bool {
        matches!(self.state, LoadState::Blocked | LoadState::Runnable)
    }

    pub(crate) fn update(&mut self, now_ns: u64, weight: u64, running: bool) {
        if self.state != LoadState::New {
            let runnable = u64::from(self.state == LoadState::Runnable);
            self.pelt
                .update(now_ns, runnable, runnable, running, weight);
        }
    }
}

unittest::path_mod_test!(mod tests, "../ut-tests/pelt.rs");
