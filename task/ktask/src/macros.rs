// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Crate-local macros used by timers and the run queue.

macro_rules! percpu_static {
    ($(
        $(#[$comment:meta])*
        $name:ident: $ty:ty = $init:expr
    ),* $(,)?) => {
        $(
            $(#[$comment])*
            #[percpu::def_percpu]
            static $name: $ty = $init;
        )*
    };
}
