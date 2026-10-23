// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host probe of production completion logic with a mutex replacing IRQ guards.

extern crate self as kspin;
extern crate self as unittest;

pub struct SpinNoIrq<T>(std::sync::Mutex<T>);

impl<T> SpinNoIrq<T> {
    pub const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.0.lock().unwrap()
    }
}

#[macro_export]
macro_rules! path_mod_test {
    ($($tokens:tt)*) => {};
}

#[path = "../../../src/completion.rs"]
pub mod completion;

#[cfg(test)]
#[path = "../allocation.rs"]
mod allocation;

#[cfg(test)]
#[path = "../cases.rs"]
mod cases;
