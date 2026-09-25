// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional best-effort backlog warnings with ordinary Arc/Weak lifetimes.

mod directory;
pub use directory::Directory;
mod record;

pub(crate) use record::StallRecord;
