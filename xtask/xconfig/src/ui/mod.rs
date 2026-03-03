// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub mod app;
pub mod dependency_resolver;
pub mod events;
pub mod rendering;
pub mod state;
pub mod utils;

pub use app::MenuConfigApp;
pub use dependency_resolver::{DependencyError, DependencyResolver};
pub use events::{EventHandler, EventResult};
pub use rendering::Theme;
pub use state::{ConfigState, MenuItem, NavigationState};
