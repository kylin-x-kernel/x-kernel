// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consuming cleanup callbacks for lifecycle fixtures.

use kalloc_types::KBox;

pub(crate) struct Callback(KBox<dyn FnMut() + Send>);

impl Callback {
    pub(crate) fn call(mut self) {
        self.0();
    }
}

pub(crate) fn new(callback: impl FnOnce() + Send + 'static) -> Callback {
    let mut callback = Some(callback);
    Callback(KBox::new_nowait(move || callback.take().unwrap()()).unwrap())
}
