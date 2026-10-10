// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared UI-test harness for the lints in this crate.
//!
//! Each lint module embeds its test input and expected diagnostics as string
//! constants and calls [`run_ui`]. The harness writes them to a per-lint
//! directory under the Cargo target directory, builds the mock crates the
//! lint asks for (such as [`MOCK_KSPIN`]), and hands the directory to
//! `dylint_testing`.

use std::{
    env, fs,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::Command,
};

/// Test-only stand-in for the kernel `kspin` crate.
pub(crate) const MOCK_KSPIN: MockCrate = MockCrate {
    name: "kspin",
    source: r#"//! Test-only stand-in for the kernel `kspin` crate.
//!
//! The lint identifies lock types by (crate name, item name) and the
//! registration macro by (crate name, macro name), so this mock only needs to
//! reproduce those names and the constructor shapes.

use core::{cell::UnsafeCell, marker::PhantomData};

/// Per-lock-class statistics placeholder.
pub struct LockClassStats;

/// Shared class bound by `SpinLock::new` under the real `stats` feature.
pub static NOOP_CLASS: LockClassStats = LockClassStats;

/// Guard marker: no protection.
pub struct NoOp;
/// Guard marker: disables preemption.
pub struct NoPreempt;
/// Guard marker: disables preemption and IRQs.
pub struct NoPreemptIrqSave;
/// Guard marker: disables IRQs only; it has no `Spin*` alias.
pub struct IrqSave;

/// Spin lock parameterised by a guard marker.
pub struct SpinLock<G, T> {
    _g: PhantomData<G>,
    data: UnsafeCell<T>,
    stats: &'static LockClassStats,
}

impl<G, T> SpinLock<G, T> {
    /// Creates an unregistered lock bound to [`NOOP_CLASS`].
    pub const fn new(data: T) -> Self {
        Self::new_with_stats(data, &NOOP_CLASS)
    }

    /// Creates a lock bound to an explicit statistics class.
    pub const fn new_with_stats(data: T, stats: &'static LockClassStats) -> Self {
        Self {
            _g: PhantomData,
            data: UnsafeCell::new(data),
            stats,
        }
    }

    /// Returns the bound statistics class.
    pub fn stats(&self) -> &'static LockClassStats {
        self.stats
    }

    /// Returns a raw pointer to the protected data.
    pub fn data_ptr(&self) -> *mut T {
        self.data.get()
    }
}

// SAFETY: test-only mock; never used to share data across threads.
unsafe impl<G, T> Sync for SpinLock<G, T> {}

/// Raw spinlock with no guards.
pub type SpinRaw<T> = SpinLock<NoOp, T>;
/// Spinlock that disables preemption.
pub type SpinNoPreempt<T> = SpinLock<NoPreempt, T>;
/// Spinlock that disables preemption and IRQs.
pub type SpinNoIrq<T> = SpinLock<NoPreemptIrqSave, T>;

/// Reader-writer spin lock; out of scope for the lint.
pub struct SpinRwLock<G, T> {
    _g: PhantomData<G>,
    data: UnsafeCell<T>,
}

impl<G, T> SpinRwLock<G, T> {
    /// Creates a reader-writer lock.
    pub const fn new(data: T) -> Self {
        Self {
            _g: PhantomData,
            data: UnsafeCell::new(data),
        }
    }

    /// Returns a raw pointer to the protected data.
    pub fn data_ptr(&self) -> *mut T {
        self.data.get()
    }
}

// SAFETY: test-only mock; never used to share data across threads.
unsafe impl<G, T> Sync for SpinRwLock<G, T> {}

/// Reader-writer spinlock that disables preemption and IRQs.
pub type SpinRwNoIrq<T> = SpinRwLock<NoPreemptIrqSave, T>;

/// Pass-through registration macro, as in `kspin` without `stats`.
#[macro_export]
macro_rules! static_lock {
    ($item:item) => {
        $item
    };
}

/// Test-only external macro that emits an unregistered static.
#[macro_export]
macro_rules! declare_unregistered {
    ($name:ident) => {
        static $name: $crate::SpinNoIrq<u32> = $crate::SpinNoIrq::new(0);
    };
}
"#,
};

/// A test-only crate that the UI test source can `use`, built from one file.
pub(crate) struct MockCrate {
    /// Crate name, as passed to `--extern`.
    pub(crate) name: &'static str,
    /// Contents of the crate root.
    pub(crate) source: &'static str,
}

/// Compiles `source` with the Dylint driver against the `mocks` crates and
/// compares the diagnostics with `expected_stderr`.
///
/// Files live in `<target>/ui/<lint>/` and are kept after the run. On a
/// mismatch, the actual output is copied to `<target>/ui/<lint>/actual.stderr`.
///
/// # Panics
///
/// Panics if a mock crate cannot be built or the output differs.
pub(crate) fn run_ui(lint: &str, source: &str, expected_stderr: &str, mocks: &[MockCrate]) {
    let base = target_dir().join("ui").join(lint);
    if base.exists() {
        fs::remove_dir_all(&base)
            .unwrap_or_else(|error| panic!("cannot remove {}: {error}", base.display()));
    }
    let deps = base.join("deps");
    let src = base.join("src");
    for dir in [&deps, &src] {
        fs::create_dir_all(dir)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", dir.display()));
    }
    println!("ui test `{lint}`: files in {}", base.display());

    let mut rustc_flags = vec![
        "--edition=2024".to_owned(),
        "-L".to_owned(),
        format!("dependency={}", deps.display()),
    ];
    for mock in mocks {
        let rlib = build_mock(&deps, mock);
        rustc_flags.push("--extern".to_owned());
        rustc_flags.push(format!("{}={}", mock.name, rlib.display()));
    }
    write(&src.join(format!("{lint}.rs")), source);
    write(&src.join(format!("{lint}.stderr")), expected_stderr);

    // compiletest saves a mismatching output as `<temp dir>/<lint>.stage-id.stderr`.
    let saved = env::temp_dir().join(format!("{lint}.stage-id.stderr"));
    let _ = fs::remove_file(&saved);
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        dylint_testing::ui::Test::src_base(env!("CARGO_PKG_NAME"), &src)
            .rustc_flags(rustc_flags)
            .run();
    }));
    if let Err(payload) = result {
        let actual = base.join("actual.stderr");
        match fs::copy(&saved, &actual) {
            Ok(_) => eprintln!(
                "ui test `{lint}`: actual stderr kept at {}",
                actual.display()
            ),
            Err(_) => eprintln!("ui test `{lint}`: no diagnostics were emitted"),
        }
        panic::resume_unwind(payload);
    }
}

/// Returns the Cargo target directory that holds the running test binary.
fn target_dir() -> PathBuf {
    // The binary is `<target>/<profile>/deps/<name>-<hash>`.
    env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(Path::to_path_buf))
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
}

/// Builds `mock` into `deps/lib<name>.rlib` with the lint toolchain.
fn build_mock(deps: &Path, mock: &MockCrate) -> PathBuf {
    let source = deps.join(format!("{}.rs", mock.name));
    let rlib = deps.join(format!("lib{}.rlib", mock.name));
    write(&source, mock.source);
    let toolchain = toolchain();
    let status = Command::new("rustup")
        .args(["run", &toolchain, "rustc"])
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "rlib",
            "--crate-name",
            mock.name,
        ])
        .arg("-o")
        .arg(&rlib)
        .arg(&source)
        .status()
        .unwrap_or_else(|error| panic!("cannot run `rustup run {toolchain} rustc`: {error}"));
    assert!(
        status.success(),
        "building the mock {} crate with `rustup run {toolchain} rustc` failed: {status}",
        mock.name
    );
    rlib
}

/// Returns the active rustup toolchain, falling back to `rust-toolchain.toml`.
fn toolchain() -> String {
    env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| {
        include_str!("../rust-toolchain.toml")
            .lines()
            .find_map(|line| line.trim().strip_prefix("channel = "))
            .map(|channel| channel.trim_matches('"').to_owned())
            .expect("rust-toolchain.toml has no channel")
    })
}

fn write(path: &Path, content: &str) {
    fs::write(path, content)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}
