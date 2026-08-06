// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel entropy pool used by `/dev/random`, `getrandom(2)`, and TEE RNG paths.
//!
//! Hardware entropy is mixed into a ChaCha20 pool before output. Registered
//! sources in [`source`] are all mixed during reseed when available:
//!
//! 1. Architecture CPU RNG (AArch64 RNDR / x86 RDSEED·RDRAND when `KFEAT_ENTROPY_ARCH_CPU` is set)
//! 2. SMCCC firmware TRNG (when `KFEAT_ENTROPY_SMCCC_TRNG` is set)
//! 3. VirtIO RNG (when `KFEAT_DRIVER_VIRTIO_RNG` and `KFEAT_ENTROPY_TRUST_HOST` are set)
//! 4. Software jitter (when `KFEAT_ENTROPY_JITTER` is set; timer / interrupt timing noise)
//!
//! VirtIO reads are deferred until the first [`fill_random`] call so boot does
//! not block in a pre-interrupt virtqueue poll. CPU, SMCCC, and jitter sources
//! are mixed eagerly during [`init`]. The pool's bootstrap seed also mixes
//! jitter (when enabled) so the ChaCha20 state is not derived from ticks alone.
//! [`is_ready`] becomes true once quality entropy (hardware or jitter) has been
//! mixed and is used by `getrandom(2)` for `GRND_NONBLOCK` → `EAGAIN`. Userspace
//! writes via [`add_entropy`] mix into the pool but do not establish readiness.

#![no_std]

extern crate alloc;

mod arch_cpu;
mod jitter;
mod smccc_trng;
mod source;
mod virtio;

use core::sync::atomic::{AtomicBool, Ordering};

use khal::time::now_ticks;
use klazy::Lazy;
use ksync::Mutex;
use ktask::WaitQueue;
use rand_chacha::{
    ChaCha20Rng,
    rand_core::{RngCore, SeedableRng},
};

const SEED_SIZE: usize = 32;
const HW_RESEED_BYTES: usize = 64;
const HW_RESEED_INTERVAL: u64 = 4096;

static POOL: Lazy<Mutex<EntropyPool>> = Lazy::new(|| Mutex::new(EntropyPool::bootstrap()));
static HW_READY: AtomicBool = AtomicBool::new(false);
static HW_ENABLE_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static INIT_DONE: AtomicBool = AtomicBool::new(false);
/// Set once quality entropy (hardware or jitter) has been mixed.
/// Used by `getrandom(2)` for `GRND_NONBLOCK` / readiness checks.
/// Userspace `/dev/random` writes mix into the pool but do not set this flag.
static CRNG_READY: AtomicBool = AtomicBool::new(false);
/// Waiters for [`wait_until_ready`]; notified on the false→true transition of
/// [`CRNG_READY`].
static READY_WQ: WaitQueue = WaitQueue::new();

struct EntropyPool {
    rng: ChaCha20Rng,
    write_counter: u64,
    bytes_since_reseed: u64,
}

impl EntropyPool {
    fn bootstrap() -> Self {
        Self {
            rng: ChaCha20Rng::from_seed(bootstrap_seed()),
            write_counter: 0,
            bytes_since_reseed: 0,
        }
    }

    fn output_bytes(&mut self, buf: &mut [u8]) {
        self.rng.fill_bytes(buf);
    }

    fn mix_bytes(&mut self, input: &[u8]) {
        let mut seed = [0u8; SEED_SIZE];
        self.rng.fill_bytes(&mut seed);

        for (index, byte) in input.iter().enumerate() {
            seed[index % seed.len()] ^= *byte;
        }

        self.write_counter = self.write_counter.wrapping_add(1);

        if !kbuild_config::KFEAT_ENTROPY_JITTER {
            let ticks = now_ticks()
                .as_raw()
                .wrapping_add(self.write_counter.rotate_left(17));
            for (index, byte) in ticks.to_le_bytes().iter().enumerate() {
                seed[index] ^= *byte;
            }
        }

        self.rng = ChaCha20Rng::from_seed(seed);
    }
}

fn bootstrap_seed() -> [u8; SEED_SIZE] {
    let mut seed = [0u8; SEED_SIZE];
    let mut state = now_ticks().as_raw() ^ 0x9e37_79b9_7f4a_7c15;

    for chunk in seed.chunks_mut(8) {
        state ^= state.rotate_left(7);
        state = state.wrapping_mul(0xd134_2543_de82_ef95);
        chunk.copy_from_slice(&state.to_le_bytes());
    }

    // Mix software jitter as early as the pool is created so the first seed is
    // not only a predictable tick expansion. Jitter does not depend on POOL, so
    // this cannot recurse through `Lazy`.
    if let Some(jitter_bytes) = jitter::read(SEED_SIZE) {
        for (index, byte) in jitter_bytes.iter().enumerate() {
            seed[index % seed.len()] ^= *byte;
        }
        mark_ready();
    }

    seed
}

/// Initialize the entropy pool and probe optional hardware sources.
///
/// Must be called after driver probe so VirtIO RNG char devices are visible.
/// HRNG sources (CPU RNDR, SMCCC TRNG) are mixed immediately; VirtIO and any
/// remaining sources are mixed on the first [`fill_random`] call.
pub fn init() {
    if INIT_DONE.swap(true, Ordering::AcqRel) {
        return;
    }

    source::init_all();
    try_eager_hardware_reseed();

    log::info!(
        "entropy pool initialized ({}, hardware_ready={})",
        source::available_summary(),
        is_hardware_ready()
    );
}

/// Fill `buf` with random bytes from the kernel entropy pool.
pub fn fill_random(buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }

    try_enable_hardware_once();
    maybe_reseed_from_hardware(buf.len());
    POOL.lock().output_bytes(buf);
}

/// Mix additional entropy into the pool, for example from a `/dev/random` write.
///
/// Like Linux, userspace writes are mixed for pool diversity but do **not**
/// mark the CRNG as initialized — otherwise any process could bypass
/// `getrandom` unreadiness checks with known data.
pub fn add_entropy(input: &[u8]) {
    if input.is_empty() {
        return;
    }
    POOL.lock().mix_bytes(input);
}

/// Returns whether hardware entropy has been successfully mixed into the pool.
fn is_hardware_ready() -> bool {
    HW_READY.load(Ordering::Acquire)
}

/// Returns whether the ChaCha20 pool has been seeded with quality entropy.
///
/// Corresponds roughly to Linux CRNG initialization: true after hardware or
/// jitter has been mixed. Pure tick bootstrap and userspace [`add_entropy`]
/// alone do not count. Used by `getrandom(2)` for `GRND_NONBLOCK` → `EAGAIN`
/// and for blocking until ready.
pub fn is_ready() -> bool {
    CRNG_READY.load(Ordering::Acquire)
}

/// Attempt deferred hardware seeding once (for example VirtIO on first use).
///
/// Safe to call repeatedly; only the first attempt performs I/O. Callers such
/// as `getrandom` use this before deciding whether to return `EAGAIN`.
pub fn try_seed_from_hardware() {
    try_enable_hardware_once();
}

/// Block until the ChaCha20 pool has been seeded with quality entropy.
///
/// Used by `getrandom(2)` when the pool is not yet ready and `GRND_NONBLOCK` is
/// not set. Returns immediately if [`is_ready`] is already true.
///
/// Signal-interruptible waiting is not implemented yet; a waiting task stays
/// blocked until readiness (matching the common non-signal path).
pub fn wait_until_ready() {
    READY_WQ.wait_until(is_ready);
}

fn mark_ready() {
    // Only wake waiters on the false→true transition.
    if !CRNG_READY.swap(true, Ordering::AcqRel) {
        READY_WQ.notify_all(false);
    }
}

fn try_eager_hardware_reseed() {
    let mixed = mix_eager_hardware_entropy(HW_RESEED_BYTES);
    if mixed.is_empty() {
        return;
    }

    HW_READY.store(true, Ordering::Release);
    log::info!(
        "entropy: early hardware reseed ({})",
        format_source_list(&mixed)
    );
}

fn try_enable_hardware_once() {
    if HW_ENABLE_ATTEMPTED.swap(true, Ordering::AcqRel) {
        return;
    }

    let mixed = mix_hardware_entropy(HW_RESEED_BYTES);
    if mixed.is_empty() {
        if !is_hardware_ready() && source::any_available() {
            log::warn!("entropy: hardware sources present but initial reseed failed");
        }
        return;
    }

    HW_READY.store(true, Ordering::Release);
    log::info!(
        "entropy: hardware RNG enabled ({})",
        format_source_list(&mixed)
    );
}

fn maybe_reseed_from_hardware(request_len: usize) {
    if !is_hardware_ready() {
        return;
    }

    // Claim the reseed under the pool lock so concurrent callers do not each
    // decide to hit hardware once the interval is crossed (TOCTOU).
    let claimed = {
        let mut pool = POOL.lock();
        pool.bytes_since_reseed = pool.bytes_since_reseed.saturating_add(request_len as u64);
        if pool.bytes_since_reseed >= HW_RESEED_INTERVAL {
            pool.bytes_since_reseed = 0;
            true
        } else {
            false
        }
    };

    if !claimed {
        return;
    }

    // Hardware reads happen outside the pool lock (may block). Mixing still
    // takes the lock inside `mix_samples`. If mixing yields nothing, force the
    // counter back to the threshold so a later caller can retry the claim.
    let mixed = mix_hardware_entropy(HW_RESEED_BYTES);
    if mixed.is_empty() {
        POOL.lock().bytes_since_reseed = HW_RESEED_INTERVAL;
    }
}

fn mix_eager_hardware_entropy(len: usize) -> alloc::vec::Vec<&'static str> {
    mix_samples(source::read_all_eager(len))
}

fn mix_hardware_entropy(len: usize) -> alloc::vec::Vec<&'static str> {
    mix_samples(source::read_all_available(len))
}

fn mix_samples(samples: alloc::vec::Vec<source::SourceSample>) -> alloc::vec::Vec<&'static str> {
    if samples.is_empty() {
        return alloc::vec::Vec::new();
    }

    let mut mixed = alloc::vec::Vec::with_capacity(samples.len());
    {
        let mut pool = POOL.lock();
        for sample in samples {
            pool.mix_bytes(&sample.data);
            mixed.push(sample.name);
        }
    }
    // Trusted kernel sources (CPU / SMCCC / VirtIO / jitter) credit readiness.
    mark_ready();
    mixed
}

fn format_source_list(sources: &[&'static str]) -> alloc::string::String {
    if sources.is_empty() {
        return alloc::string::String::new();
    }
    sources.join(", ")
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, assert_ne, def_test};

    use super::*;

    #[def_test]
    fn test_fill_random_without_init() {
        let mut buf = [0u8; 32];
        fill_random(&mut buf);
        assert_ne!(buf, [0u8; 32]);
    }

    #[def_test]
    fn test_fill_random_empty_is_noop() {
        let mut buf = [];
        fill_random(&mut buf);
        assert_eq!(buf.len(), 0);
    }

    #[def_test]
    fn test_fill_random_various_lengths() {
        init();
        for len in [1usize, 7, 32, 64, 256, 1024] {
            let mut buf = alloc::vec![0u8; len];
            fill_random(&mut buf);
            assert_eq!(buf.len(), len);
            assert!(buf.iter().any(|&b| b != 0), "all-zero output for len {len}");
        }
    }

    #[def_test]
    fn test_add_entropy_empty_is_noop() {
        // Empty input must return immediately without mixing or panicking.
        add_entropy(&[]);
        let mut buf = [0u8; 16];
        fill_random(&mut buf);
        assert_ne!(buf, [0u8; 16]);
    }

    #[def_test]
    fn test_add_entropy_changes_output() {
        init();
        let mut before = [0u8; 32];
        let mut after = [0u8; 32];
        fill_random(&mut before);
        add_entropy(b"extra-entropy-input");
        fill_random(&mut after);
        assert_ne!(before, after);
    }

    #[def_test]
    fn test_add_entropy_does_not_mark_ready() {
        // Capture readiness before an untrusted mix. Userspace writes must not
        // be what establishes CRNG readiness (Linux write_pool semantics).
        let ready_before = is_ready();
        add_entropy(b"attacker-controlled-known-bytes");
        if !ready_before {
            assert!(!is_ready());
        }
        // Mixing still affects subsequent output when the pool is already live.
        let mut before = [0u8; 32];
        let mut after = [0u8; 32];
        fill_random(&mut before);
        add_entropy(b"more-untrusted-bytes");
        fill_random(&mut after);
        assert_ne!(before, after);
    }

    #[def_test]
    fn test_wait_until_ready_returns_when_ready() {
        init();
        try_seed_from_hardware();
        // Default qemu defconfig enables jitter and/or HW sources.
        assert!(is_ready());
        wait_until_ready();
    }

    #[def_test]
    fn test_two_reads_differ() {
        init();
        let mut buf1 = [0u8; 32];
        let mut buf2 = [0u8; 32];
        fill_random(&mut buf1);
        fill_random(&mut buf2);
        assert_ne!(buf1, buf2);
    }

    #[def_test]
    fn test_init_is_idempotent() {
        init();
        let ready_before = is_ready();
        let hw_before = is_hardware_ready();
        init();
        init();
        assert_eq!(is_ready(), ready_before);
        assert_eq!(is_hardware_ready(), hw_before);
    }

    #[def_test]
    fn test_try_seed_from_hardware_is_safe() {
        init();
        try_seed_from_hardware();
        try_seed_from_hardware();
        let mut buf = [0u8; 16];
        fill_random(&mut buf);
        assert_ne!(buf, [0u8; 16]);
    }

    #[def_test]
    fn test_format_source_list() {
        assert_eq!(format_source_list(&[]), "");
        assert_eq!(format_source_list(&["cpu-rng"]), "cpu-rng");
        assert_eq!(
            format_source_list(&["cpu-rng", "jitter"]),
            "cpu-rng, jitter"
        );
    }

    #[def_test]
    fn test_hardware_presence_matches_sources() {
        init();
        // `any_available` is the discovery side of hardware readiness.
        let present = source::any_available();
        if is_hardware_ready() {
            assert!(present);
        }
    }
}
