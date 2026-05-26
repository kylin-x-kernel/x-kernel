// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ops::{Deref, DerefMut};

use bytemuck::AnyBitPattern;
use ksignal::Signo;
use linux_raw_sys::general::{
    B38400, CREAD, CS8, ECHO, ECHOCTL, ECHOE, ECHOK, ECHOKE, ICANON, ICRNL, IEXTEN, ISIG, IXON,
    ONLCR, OPOST, VDISCARD, VEOF, VEOL, VEOL2, VERASE, VINTR, VKILL, VLNEXT, VQUIT, VREPRINT,
    VWERASE, speed_t, tcflag_t,
};

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Termios {
    c_iflag: tcflag_t,
    c_oflag: tcflag_t,
    c_cflag: tcflag_t,
    c_lflag: tcflag_t,
    c_line: u8,
    c_cc: [u8; 19usize],
}

impl Default for Termios {
    fn default() -> Self {
        let mut result = Self {
            c_iflag: ICRNL | IXON,
            c_oflag: OPOST | ONLCR,
            c_cflag: B38400 | CS8 | CREAD,
            c_lflag: ICANON | ECHO | ISIG | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            c_line: 0,
            c_cc: [0; 19],
        };

        fn ctl(ch: u8) -> u8 {
            ch - 0x40
        }
        for (i, ch) in [
            (VINTR, ctl(b'C')),
            (VQUIT, ctl(b'\\')),
            (VERASE, b'\x7f'),
            (VKILL, ctl(b'U')),
            (VEOF, ctl(b'D')),
            (VEOL, b'\0'),
            (VREPRINT, ctl(b'R')),
            (VDISCARD, ctl(b'O')),
            (VWERASE, ctl(b'W')),
            (VLNEXT, ctl(b'V')),
            (VEOL2, b'\0'),
        ] {
            result.c_cc[i as usize] = ch;
        }

        result
    }
}

impl Termios {
    /// Get a special character (e.g., interrupt, kill, eof) by index
    pub fn special_char(&self, index: u32) -> u8 {
        self.c_cc[index as usize]
    }

    pub fn has_iflag(&self, flag: u32) -> bool {
        self.c_iflag & flag != 0
    }

    pub fn has_oflag(&self, flag: u32) -> bool {
        self.c_oflag & flag != 0
    }

    pub fn has_cflag(&self, flag: u32) -> bool {
        self.c_cflag & flag != 0
    }

    pub fn has_lflag(&self, flag: u32) -> bool {
        self.c_lflag & flag != 0
    }

    /// Check if echo mode is enabled
    pub fn echo(&self) -> bool {
        self.has_lflag(ECHO)
    }

    /// Check if canonical (line-based) mode is enabled
    pub fn canonical(&self) -> bool {
        self.has_lflag(ICANON)
    }

    pub fn contains_iexten(&self) -> bool {
        self.has_lflag(IEXTEN)
    }

    /// Check if a character is an end-of-line character
    pub fn is_eol(&self, ch: u8) -> bool {
        if ch == b'\n' || ch == self.special_char(VEOL) {
            return true;
        }

        if self.contains_iexten() && ch == self.special_char(VEOL2) {
            return true;
        }

        false
    }

    /// Get the signal number for a control character (e.g., SIGINT for Ctrl+C)
    pub fn signo_for(&self, ch: u8) -> Option<Signo> {
        Some(match ch {
            ch if ch == self.special_char(VINTR) => Signo::SIGINT,
            ch if ch == self.special_char(VQUIT) => Signo::SIGQUIT,
            _ => return None,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Termios2 {
    termios: Termios,
    c_ispeed: speed_t,
    c_ospeed: speed_t,
}

impl Default for Termios2 {
    fn default() -> Self {
        Self::new(Termios::default())
    }
}
impl Termios2 {
    pub fn new(termios: Termios) -> Self {
        Self {
            termios,
            c_ispeed: B38400,
            c_ospeed: B38400,
        }
    }
}

impl Deref for Termios2 {
    type Target = Termios;

    fn deref(&self) -> &Self::Target {
        &self.termios
    }
}

impl DerefMut for Termios2 {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.termios
    }
}

#[cfg(unittest)]
mod termios_tests {
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
}
