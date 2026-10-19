// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_window_size_default() {
    let term = Terminal::default();
    let ws = *term.window_size.lock();
    assert_eq!(ws.ws_row, 28);
    assert_eq!(ws.ws_col, 110);
    assert_eq!(ws.ws_xpixel, 0);
    assert_eq!(ws.ws_ypixel, 0);
}

#[def_test]
fn test_terminal_load_termios() {
    let term = Terminal::default();
    let t = term.load_termios();
    assert!(t.echo());
    assert!(t.canonical());
}

#[def_test]
fn test_window_size_layout() {
    assert_eq!(core::mem::size_of::<WindowSize>(), 8);
}
