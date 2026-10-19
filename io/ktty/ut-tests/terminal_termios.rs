// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_termios_default_flags() {
    let t = Termios::default();
    assert!(t.has_iflag(ICRNL));
    assert!(t.has_iflag(IXON));
    assert!(t.has_oflag(OPOST));
    assert!(t.has_oflag(ONLCR));
    assert!(t.has_cflag(CREAD));
    assert!(t.has_cflag(CS8));
    assert!(t.has_lflag(ICANON));
    assert!(t.has_lflag(ECHO));
    assert!(t.has_lflag(ISIG));
    assert!(t.has_lflag(IEXTEN));
}

#[def_test]
fn test_termios_echo_and_canonical() {
    let t = Termios::default();
    assert!(t.echo());
    assert!(t.canonical());
    assert!(t.contains_iexten());
}

#[def_test]
fn test_termios_special_chars() {
    let t = Termios::default();
    assert_eq!(t.special_char(VINTR), b'C' - 0x40); // Ctrl+C
    assert_eq!(t.special_char(VQUIT), b'\\' - 0x40); // Ctrl+backslash
    assert_eq!(t.special_char(VERASE), b'\x7f'); // DEL
    assert_eq!(t.special_char(VKILL), b'U' - 0x40); // Ctrl+U
    assert_eq!(t.special_char(VEOF), b'D' - 0x40); // Ctrl+D
    assert_eq!(t.special_char(VWERASE), b'W' - 0x40); // Ctrl+W
}

#[def_test]
fn test_termios_is_eol() {
    let t = Termios::default();
    assert!(t.is_eol(b'\n'));
    assert!(!t.is_eol(b'a'));
    assert!(!t.is_eol(b' '));
}

#[def_test]
fn test_termios_signo_for() {
    let t = Termios::default();
    let ctrl_c = t.special_char(VINTR);
    let ctrl_backslash = t.special_char(VQUIT);
    assert!(matches!(t.signo_for(ctrl_c), Some(Signo::SIGINT)));
    assert!(matches!(t.signo_for(ctrl_backslash), Some(Signo::SIGQUIT)));
    assert!(t.signo_for(b'a').is_none());
    assert!(t.signo_for(b'\n').is_none());
}

#[def_test]
fn test_termios2_default_speed() {
    let t2 = Termios2::default();
    assert_eq!(t2.c_ispeed, B38400);
    assert_eq!(t2.c_ospeed, B38400);
}

#[def_test]
fn test_termios2_deref() {
    let t2 = Termios2::default();
    assert!(t2.echo());
    assert!(t2.canonical());
    assert_eq!(t2.special_char(VINTR), b'C' - 0x40);
}
