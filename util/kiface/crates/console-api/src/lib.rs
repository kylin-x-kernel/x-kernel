// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordinary console I/O, emergency output and input activation contracts.

#![no_std]

/// Ordinary console byte I/O; provider may use console locks.
#[kiface::interface]
pub trait ConsoleIoIf {
    /// Writes ordinary console output; not an emergency/NMI entry.
    fn write_data(buf: &[u8]);

    /// Copies currently available bytes and returns the number copied.
    fn read_data(buf: &mut [u8]) -> usize;
}

/// Best-effort output in emergency and NMI contexts.
#[kiface::interface]
pub trait EmergencyConsoleWriteIf {
    /// Attempt emergency output without ordinary locks or allocation.
    ///
    /// Implementations must tolerate NMI interruption of normal console I/O
    /// and bound hardware polling. Output may be interleaved, truncated, or
    /// discarded if the console is unavailable.
    fn write_data_atomic(buf: &[u8]);
}

/// One-time input activation and installed input IRQ discovery.
#[kiface::interface]
pub trait ConsoleInputSetupIf {
    /// Attempt input IRQ setup once, in sleepable boot-task context after
    /// runtime services start and before any TTY chooses its input mode.
    /// The boot task serializes this call with consumer publication.
    /// Failure preserves polled I/O; the provider reports its diagnostic.
    fn prepare_input();

    /// Return the installed input IRQ, not merely firmware IRQ metadata.
    fn interrupt_id() -> Option<usize>;
}
