// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use raw_cpuid::{CpuId, CpuIdReader, CpuIdResult};
use unittest::def_test;

use super::{
    CalibrationProbe, Frequency, PIT_CALIBRATION_SAMPLES, PIT_CALIBRATION_TICKS,
    PIT_MAX_SAMPLE_TIMEOUTS, PIT_SAMPLE_SPREAD_PERCENT, PIT_TICK_RATE, PitObservation,
    SampleRejection,
    TscFrequency::{Cpuid, Hypervisor, Pit, PlatformStatic},
    calibrate_samples, cpuid_tsc_frequency, discover_tsc_frequency_with, filter_tsc_frequency,
    frequency_from_ticks, measure_with,
};

fn cpuid_with_frequencies(
    signature: &'static [u8; 12],
    hypervisor_mhz: u32,
    cpuid_mhz: u32,
) -> CpuId<impl CpuIdReader> {
    CpuId::with_cpuid_reader(move |leaf, _| {
        let [eax, ebx, ecx, edx] = match leaf {
            0 => [
                0x15,
                u32::from_le_bytes(*b"Genu"),
                u32::from_le_bytes(*b"ntel"),
                u32::from_le_bytes(*b"ineI"),
            ],
            1 => [0, 0, 1 << 31, 0],
            0x15 => [1, 1, cpuid_mhz * 1_000_000, 0],
            0x4000_0000 => [
                0x4000_0010,
                u32::from_le_bytes(signature[..4].try_into().unwrap()),
                u32::from_le_bytes(signature[4..8].try_into().unwrap()),
                u32::from_le_bytes(signature[8..].try_into().unwrap()),
            ],
            0x4000_0010 => [hypervisor_mhz * 1_000, 0, 0, 0],
            _ => [0; 4],
        };
        CpuIdResult { eax, ebx, ecx, edx }
    })
}

/// One scripted operation for [`ScriptProbe`].
#[derive(Clone, Copy)]
enum Step {
    Tsc(u64),
    Start,
    Elapsed(bool),
}

/// Replays a fixed operation sequence so a test can verify the sampling order
/// and inject a host pause at an exact point.
struct ScriptProbe<'a> {
    steps: &'a [Step],
    next: usize,
}

impl<'a> ScriptProbe<'a> {
    fn new(steps: &'a [Step]) -> Self {
        Self { steps, next: 0 }
    }

    fn take(&mut self) -> Step {
        let step = self
            .steps
            .get(self.next)
            .copied()
            .expect("calibration probe overran its script");
        self.next += 1;
        step
    }
}

impl CalibrationProbe for ScriptProbe<'_> {
    fn tsc(&mut self) -> u64 {
        match self.take() {
            Step::Tsc(value) => value,
            _ => panic!("expected a TSC read"),
        }
    }

    fn start_pit(&mut self) {
        assert!(matches!(self.take(), Step::Start), "expected a PIT start");
    }

    fn pit_elapsed(&mut self) -> bool {
        match self.take() {
            Step::Elapsed(value) => value,
            _ => panic!("expected a PIT poll"),
        }
    }
}

fn observation(low_tsc_ticks: u64, high_tsc_ticks: u64) -> PitObservation {
    PitObservation {
        low_tsc_ticks,
        high_tsc_ticks,
    }
}

/// Replays one scripted sample result per call and records how many samples
/// the calibration requested, so a test can detect a lost early abort.
struct ScriptedSamples<'a> {
    sequence: &'a [Result<PitObservation, SampleRejection>],
    calls: usize,
}

impl<'a> ScriptedSamples<'a> {
    fn new(sequence: &'a [Result<PitObservation, SampleRejection>]) -> Self {
        Self { sequence, calls: 0 }
    }

    fn sample(
        &mut self,
        _timeout_tsc_ticks: u64,
        _start_gap_tsc_ticks: u64,
    ) -> Result<PitObservation, SampleRejection> {
        let item = *self
            .sequence
            .get(self.calls)
            .expect("calibration overran its sample script");
        self.calls += 1;
        item
    }
}

#[def_test]
fn tsc_frequency_discovery_respects_sources_and_fallback() {
    let pit = Frequency::from_hz(1_000_000_000);
    let cpuid = Frequency::from_hz(2_400_000_000);
    let hypervisor = Frequency::from_hz(2_500_000_000);
    let platform = Frequency::from_hz(4_000_000_000);
    let qemu = b"TCGTCGTCGTCG";
    let kvm = b"KVMKVMKVM\0\0\0";
    let acrn = b"ACRNACRNACRN";
    let vmware = b"VMwareVMware";
    for (signature, hypervisor_mhz, cpuid_mhz, measured, expected, should_calibrate) in [
        (qemu, 4000, 2400, Some(pit), Pit(pit), true),
        (qemu, 4000, 2400, None, Cpuid(cpuid), true),
        (kvm, 2500, 2400, None, Hypervisor(hypervisor), false),
        (acrn, 2500, 2400, None, Hypervisor(hypervisor), false),
        (vmware, 2500, 2400, None, Cpuid(cpuid), false),
        (kvm, 0, 0, Some(pit), Pit(pit), true),
        (qemu, 0, 0, None, PlatformStatic(platform), true),
        (kvm, 0, 0, None, PlatformStatic(platform), true),
    ] {
        let cpuid_reader = cpuid_with_frequencies(signature, hypervisor_mhz, cpuid_mhz);
        let mut has_calibrated = false;
        let selected =
            discover_tsc_frequency_with(&cpuid_reader, platform, |nominal, is_qemu_tcg| {
                assert_eq!(nominal, platform);
                assert_eq!(is_qemu_tcg, signature == b"TCGTCGTCGTCG");
                has_calibrated = true;
                measured.map(Pit)
            });
        assert_eq!(selected, expected, "hypervisor: {:?}", signature);
        assert_eq!(has_calibrated, should_calibrate);
    }
}

#[def_test]
fn tsc_frequency_calculations_preserve_reference_units() {
    assert_eq!(
        filter_tsc_frequency(Some(Frequency::from_hz(999_999))),
        None
    );
    for (base_mhz, numerator, denominator, expected) in [
        (2400, 7, 3, Some(Frequency::from_hz(2_400_000_000))),
        (2400, 0, 3, None),
        (2400, 7, 0, None),
        (0, 7, 3, None),
    ] {
        let cpuid = CpuId::with_cpuid_reader(|leaf, _| {
            let [eax, ebx, ecx, edx] = match leaf {
                0 => [0x16, 0x756e6547, 0x6c65746e, 0x49656e69], // GenuineIntel
                0x15 => [denominator, numerator, 0, 0],
                0x16 => [base_mhz, 0, 0, 0],
                _ => [0; 4],
            };
            CpuIdResult { eax, ebx, ecx, edx }
        });
        assert_eq!(cpuid_tsc_frequency(&cpuid), expected);
    }
}

#[def_test]
fn cpuid_rejects_out_of_range_crystal_rate_and_uses_base_frequency() {
    // Leaf 0x15 reports a usable ratio but a crystal rate whose product
    // overflows the accepted kHz range, so discovery must fall back to the
    // leaf 0x16 processor base frequency.
    let cpuid = CpuId::with_cpuid_reader(|leaf, _| {
        let [eax, ebx, ecx, edx] = match leaf {
            0 => [0x16, 0x756e6547, 0x6c65746e, 0x49656e69], // GenuineIntel
            0x15 => [1, u32::MAX, u32::MAX, 0],
            0x16 => [2400, 0, 0, 0],
            _ => [0; 4],
        };
        CpuIdResult { eax, ebx, ecx, edx }
    });
    assert_eq!(
        cpuid_tsc_frequency(&cpuid),
        Some(Frequency::from_hz(2_400_000_000))
    );
}

#[def_test]
fn pit_frequency_uses_the_reference_interval() {
    // 25 million TSC ticks over the reference countdown is about 2.5 GHz.
    let expected_hz = u128::from(25_000_000u64) * u128::from(PIT_TICK_RATE.as_hz())
        / u128::from(PIT_CALIBRATION_TICKS);
    assert_eq!(
        frequency_from_ticks(25_000_000).map(|frequency| u128::from(frequency.as_hz())),
        Some(expected_hz)
    );
    assert_eq!(frequency_from_ticks(0), None);
}

#[def_test]
fn pit_sample_reports_a_tight_interval_without_preemption() {
    // The last false poll is one poll before the terminal count, so the sound
    // interval is narrow and usable.
    let script = [
        Step::Tsc(0),
        Step::Start,
        Step::Tsc(2),
        Step::Tsc(2000),
        Step::Elapsed(false),
        Step::Tsc(2080),
        Step::Elapsed(false),
        Step::Tsc(2090),
        Step::Elapsed(true),
        Step::Tsc(2091),
    ];
    let observation = measure_with(&mut ScriptProbe::new(&script), 1_000_000, 1_000)
        .expect("the sample completes");
    assert_eq!(observation.low_tsc_ticks, 2079);
    assert_eq!(observation.high_tsc_ticks, 2090);
    assert!(
        observation
            .estimate()
            .expect("bounds convert")
            .is_within(PIT_SAMPLE_SPREAD_PERCENT)
    );
}

#[def_test]
fn pit_interval_bounds_survive_a_pause_after_a_false_poll() {
    // 0: pre-start read, 1: reload, 2: start read
    // 3/4: probe + false poll (low = 1000)
    // 5: probe after a host pause, 6: true poll, 7: upper bound read
    let script = [
        Step::Tsc(0),
        Step::Start,
        Step::Tsc(2),
        Step::Tsc(1000),
        Step::Elapsed(false),
        Step::Tsc(5000),
        Step::Elapsed(true),
        Step::Tsc(5001),
    ];
    let observation = measure_with(&mut ScriptProbe::new(&script), 1_000_000, 1_000)
        .expect("the sample completes");
    // The lower bound stays before the false poll even though the vCPU was
    // paused before the next probe, so the pause widens the interval.
    assert_eq!(observation.low_tsc_ticks, 999);
    assert_eq!(observation.high_tsc_ticks, 5000);
    assert!(
        !observation
            .estimate()
            .expect("bounds convert")
            .is_within(PIT_SAMPLE_SPREAD_PERCENT),
        "a pause after a false poll must not look like a tight sample"
    );
}

#[def_test]
fn pit_sample_discards_a_pause_while_programming_the_countdown() {
    let script = [Step::Tsc(0), Step::Start, Step::Tsc(10_000)];
    assert!(matches!(
        measure_with(&mut ScriptProbe::new(&script), 1_000_000, 1_000),
        Err(SampleRejection::Transient)
    ));
}

#[def_test]
fn pit_sample_discards_a_stale_first_terminal_count() {
    // The first poll already reports completion, so the output level belongs
    // to the previous countdown rather than this sample.
    let script = [
        Step::Tsc(0),
        Step::Start,
        Step::Tsc(10),
        Step::Tsc(20),
        Step::Elapsed(true),
    ];
    assert!(matches!(
        measure_with(&mut ScriptProbe::new(&script), 1_000_000, 1_000),
        Err(SampleRejection::Transient)
    ));
}

#[def_test]
fn pit_sample_discards_a_programming_window_large_for_the_interval() {
    // The pre-check budget is wide, but the measured interval is much shorter
    // than the programming window, so the start position is not trustworthy.
    let script = [
        Step::Tsc(0),
        Step::Start,
        Step::Tsc(100),
        Step::Tsc(1000),
        Step::Elapsed(false),
        Step::Tsc(1001),
        Step::Elapsed(true),
        Step::Tsc(1002),
    ];
    assert!(matches!(
        measure_with(&mut ScriptProbe::new(&script), 1_000_000, 10_000),
        Err(SampleRejection::Transient)
    ));
}

#[def_test]
fn pit_sample_timeout_discards_a_stalled_countdown() {
    let script = [
        Step::Tsc(0),
        Step::Start,
        Step::Tsc(10),
        Step::Tsc(2_000),
        Step::Elapsed(false),
    ];
    assert!(matches!(
        measure_with(&mut ScriptProbe::new(&script), 1_000, 1_000),
        Err(SampleRejection::TimedOut)
    ));
}

#[def_test]
fn pit_calibration_accepts_only_a_tight_interval() {
    let nominal = Frequency::from_hz(4_000_000_000);
    // The first sample is 20% wide; the second is under 1% and must win.
    let sequence = [
        Ok(observation(100_000_000, 120_000_000)),
        Ok(observation(120_000_000, 121_000_000)),
    ];
    let mut samples = ScriptedSamples::new(&sequence);
    let (frequency, is_degraded) = calibrate_samples(nominal, |timeout, start_gap| {
        samples.sample(timeout, start_gap)
    })
    .expect("a tight sample");
    assert!(!is_degraded);
    let low = frequency_from_ticks(120_000_000).expect("lower bound converts");
    let high = frequency_from_ticks(121_000_000).expect("upper bound converts");
    assert!((low..=high).contains(&frequency));
}

#[def_test]
fn pit_calibration_reports_a_bounded_degraded_result() {
    let nominal = Frequency::from_hz(4_000_000_000);
    // Every sample is 20% wide: not trustworthy, but still a bounded bound.
    let sequence = [Ok(observation(100_000_000, 120_000_000)); PIT_CALIBRATION_SAMPLES];
    let mut samples = ScriptedSamples::new(&sequence);
    let (frequency, is_degraded) = calibrate_samples(nominal, |timeout, start_gap| {
        samples.sample(timeout, start_gap)
    })
    .expect("degraded result");
    assert!(is_degraded);
    assert_eq!(samples.calls, PIT_CALIBRATION_SAMPLES);
    let low = frequency_from_ticks(100_000_000).expect("lower bound converts");
    let high = frequency_from_ticks(120_000_000).expect("upper bound converts");
    assert!((low..=high).contains(&frequency));
}

#[def_test]
fn pit_calibration_fails_instead_of_trusting_a_wide_guess() {
    let nominal = Frequency::from_hz(4_000_000_000);
    let sequence = [Ok(observation(50_000_000, 150_000_000)); PIT_CALIBRATION_SAMPLES];
    let mut samples = ScriptedSamples::new(&sequence);
    assert_eq!(
        calibrate_samples(nominal, |timeout, start_gap| samples
            .sample(timeout, start_gap)),
        None
    );
    assert_eq!(samples.calls, PIT_CALIBRATION_SAMPLES);
}

#[def_test]
fn pit_calibration_stops_after_repeated_timeouts() {
    let nominal = Frequency::from_hz(4_000_000_000);
    let sequence = [Err(SampleRejection::TimedOut); PIT_CALIBRATION_SAMPLES];
    let mut samples = ScriptedSamples::new(&sequence);
    assert_eq!(
        calibrate_samples(nominal, |timeout, start_gap| samples
            .sample(timeout, start_gap)),
        None
    );
    // The early abort must stop after the timeout budget, not spend the whole
    // sample budget on a stalled countdown.
    assert_eq!(samples.calls, PIT_MAX_SAMPLE_TIMEOUTS);
}

#[def_test]
fn pit_calibration_retries_after_transient_rejections() {
    let nominal = Frequency::from_hz(4_000_000_000);
    // Two spoiled samples must not abort PIT calibration: the tight interval
    // that follows is still accepted.
    let sequence = [
        Err(SampleRejection::Transient),
        Err(SampleRejection::Transient),
        Ok(observation(120_000_000, 121_000_000)),
    ];
    let mut samples = ScriptedSamples::new(&sequence);
    let (frequency, is_degraded) = calibrate_samples(nominal, |timeout, start_gap| {
        samples.sample(timeout, start_gap)
    })
    .expect("a tight sample after transient rejections");
    assert!(!is_degraded);
    assert_eq!(samples.calls, 3);
    let low = frequency_from_ticks(120_000_000).expect("lower bound converts");
    let high = frequency_from_ticks(121_000_000).expect("upper bound converts");
    assert!((low..=high).contains(&frequency));
}
