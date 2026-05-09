// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod backend;
mod syscall;

pub use self::{
    backend::{Epoll, EpollEvent, EpollFlags},
    syscall::{sys_epoll_create1, sys_epoll_ctl, sys_epoll_pwait, sys_epoll_pwait2},
};
