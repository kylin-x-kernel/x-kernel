// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResult {
    Continue,
    Quit,
}

pub struct EventHandler;

impl EventHandler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}
