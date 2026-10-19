// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Independent fixed VT with process lifetime and a serial-backed text plane.
//!
//! The generic TTY owns admission, session state and line discipline. This
//! adapter does not borrow the serial terminal or claim unimplemented modes.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};

use bytemuck::AnyBitPattern;
use kerrno::{KError, KResult};
use klazy::{Once, lazy_static};
use kprocess::{AsThread, ControllingTerminal, Process};
use ksync::Mutex;
use kvfs::{CharacterDeviceRegistration, DeviceId, cdev_add_named, cdev_del};
use osvm::{VirtMutPtr, VirtPtr};

use super::{Tty, TtyKind};
use crate::port::{InputIngress, ProcessMode, TtyConfig, TtyRead, TtyWrite};

const VT_OPENQRY: u32 = 0x5600;
const VT_GETMODE: u32 = 0x5601;
const VT_SETMODE: u32 = 0x5602;
const VT_GETSTATE: u32 = 0x5603;
const VT_RELDISP: u32 = 0x5605;
const VT_ACKACQ: usize = 2;
const VT_ACTIVATE: u32 = 0x5606;
const VT_WAITACTIVE: u32 = 0x5607;
const KDSETMODE: u32 = 0x4b3a;
const KDGETMODE: u32 = 0x4b3b;
const KDGKBMODE: u32 = 0x4b44;
const KDSKBMODE: u32 = 0x4b45;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, AnyBitPattern)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

impl VtMode {
    const fn automatic() -> Self {
        Self {
            mode: 0,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        }
    }

    fn validate(mut self) -> KResult<Self> {
        // Linux vt_ioctl.c accepts AUTO/PROCESS, retains the other fields,
        // and explicitly ignores frsig by setting it to zero.
        if self.mode > 1 {
            return Err(KError::InvalidInput);
        }
        self.frsig = 0;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
enum DisplayMode {
    Text     = 0,
    Graphics = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
enum KeyboardMode {
    Unicode = 3,
    Off     = 4,
}

struct ConsoleState {
    mode: VtMode,
    controller: Option<Weak<Process>>,
    display: DisplayMode,
    keyboard: KeyboardMode,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            mode: VtMode::automatic(),
            controller: None,
            display: DisplayMode::Text,
            // Linux vt.c defaults default_utf8=true, and keyboard.c's
            // kbd_init selects VC_UNICODE from that setting.
            keyboard: KeyboardMode::Unicode,
        }
    }
}

impl ConsoleState {
    fn refresh_controller(&mut self) {
        if self.mode.mode == 1
            && self
                .controller
                .as_ref()
                .and_then(Weak::upgrade)
                .is_none_or(|process| process.is_exited())
        {
            // A weak object identity cannot address a reused numeric PID.
            // Linux reset_vc also restores AUTO/TEXT/default keyboard policy
            // after finding a dead controller. This headless VT does it on use.
            *self = Self::default();
        }
    }

    fn set_mode(&mut self, mode: VtMode, caller: Option<&Arc<Process>>) -> KResult<()> {
        let mode = mode.validate()?;
        let controller = if mode.mode == 1 {
            let caller = caller
                .filter(|process| !process.is_exited())
                .ok_or(KError::NoSuchProcess)?;
            Some(Arc::downgrade(caller))
        } else {
            None
        };
        self.mode = mode;
        self.controller = controller;
        Ok(())
    }

    fn set_display_mode(&mut self, value: usize) -> KResult<()> {
        self.display = match value {
            0 => DisplayMode::Text,
            1 => DisplayMode::Graphics,
            _ => return Err(KError::InvalidInput),
        };
        Ok(())
    }

    fn set_keyboard_mode(&mut self, value: usize) -> KResult<()> {
        // No keycode translator is connected. Admit the Unicode/off policy
        // seatd uses; do not claim raw, medium-raw or XLATE implementations.
        self.keyboard = match value {
            3 => KeyboardMode::Unicode,
            4 => KeyboardMode::Off,
            0..=2 => return Err(KError::OperationNotSupported),
            _ => return Err(KError::InvalidInput),
        };
        Ok(())
    }

    fn acknowledge(&self, value: usize) -> KResult<()> {
        // Linux vt_reldisp: PROCESS with vt_newvt < 0 admits only ACKACQ.
        // There is no second VT and thus no pending switch to release. Sending
        // relsig/acqsig for activation of this same VT would invent an event:
        // Linux change_console returns immediately for the current console.
        if self.mode.mode == 1 && value == VT_ACKACQ {
            Ok(())
        } else {
            Err(KError::InvalidInput)
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct VtState {
    active: u16,
    signal: u16,
    allocated: u16,
}

fn fixed_vt_state() -> VtState {
    VtState {
        active: 1,
        signal: 0,
        // Linux vt_ioctl.c starts state=1 for tty0's current-VT alias and
        // mask=2 for VT 1. Both bits describe this same console here;
        // bit 2 (VT 2) stays clear, so the bitmap is not a console count.
        allocated: 1 | (1 << 1),
    }
}

fn require_active_console(number: usize) -> KResult<()> {
    if number == 1 {
        Ok(())
    } else {
        Err(KError::InvalidInput)
    }
}

fn configuration_permission(
    cred: &kcred::Cred,
    target: &Arc<dyn ControllingTerminal>,
    current_terminal: Option<&Arc<dyn ControllingTerminal>>,
) -> KResult<()> {
    // Linux vt_ioctl.c: current->signal->tty == tty || CAP_SYS_TTY_CONFIG.
    // kcred explicitly documents its current pre-capabilities approximation.
    if cred.has_sys_tty_config()
        || current_terminal.is_some_and(|current| Arc::ptr_eq(current, target))
    {
        Ok(())
    } else {
        Err(KError::OperationNotPermitted)
    }
}

fn require_current_configuration_permission<R: TtyRead, W: TtyWrite>(
    tty: &Tty<R, W>,
    cred: &kcred::Cred,
) -> KResult<()> {
    let target: Arc<dyn ControllingTerminal> = tty.this.upgrade().ok_or(KError::NoSuchDevice)?;
    let current = ktask::current();
    let terminal = current
        .try_as_thread()
        .and_then(|thread| thread.process().group().session().terminal());
    configuration_permission(cred, &target, terminal.as_ref())
}

pub(super) fn ioctl<R: TtyRead, W: TtyWrite>(
    tty: &Tty<R, W>,
    cred: &kcred::Cred,
    cmd: u32,
    arg: usize,
) -> Option<KResult<usize>> {
    if !matches!(
        cmd,
        VT_OPENQRY
            | VT_GETMODE
            | VT_SETMODE
            | VT_GETSTATE
            | VT_RELDISP
            | VT_ACTIVATE
            | VT_WAITACTIVE
            | KDSETMODE
            | KDGETMODE
            | KDGKBMODE
            | KDSKBMODE
    ) {
        return None;
    }
    Some((|| {
        if matches!(
            cmd,
            VT_SETMODE | KDSETMODE | KDSKBMODE | VT_ACTIVATE | VT_WAITACTIVE | VT_RELDISP
        ) {
            require_current_configuration_permission(tty, cred)?;
        }
        // Resolve this concrete VT, rather than selecting the global console;
        // CurrentTty and independently constructed terminals retain identity.
        let target: Arc<dyn ControllingTerminal> =
            tty.this.upgrade().ok_or(KError::NoSuchDevice)?;
        let console = target
            .into_any()
            .downcast::<VtConsole>()
            .map_err(|_| KError::NotATty)?;
        console.writer.state.lock().refresh_controller();
        match cmd {
            VT_GETSTATE => (arg as *mut VtState).write_vm(fixed_vt_state())?,
            VT_OPENQRY => (arg as *mut i32).write_vm(-1)?,
            VT_ACTIVATE | VT_WAITACTIVE => {
                require_active_console(arg)?;
                console.writer.state.lock().refresh_controller();
            }
            VT_GETMODE => {
                let mode = {
                    let mut state = console.writer.state.lock();
                    state.refresh_controller();
                    state.mode
                };
                (arg as *mut VtMode).write_vm(mode)?;
            }
            VT_SETMODE => {
                let mode = (arg as *const VtMode).read_vm()?.validate()?;
                let current = ktask::current();
                let caller = current
                    .try_as_thread()
                    .map(|thread| thread.process().clone());
                // Acquire the actual process identity before committing PROCESS.
                console
                    .writer
                    .state
                    .lock()
                    .set_mode(mode, caller.as_ref())?;
            }
            KDSETMODE | KDSKBMODE | VT_RELDISP => {
                let mut state = console.writer.state.lock();
                state.refresh_controller();
                match cmd {
                    KDSETMODE => state.set_display_mode(arg)?,
                    KDSKBMODE => state.set_keyboard_mode(arg)?,
                    _ => state.acknowledge(arg)?,
                }
            }
            KDGETMODE | KDGKBMODE => {
                let value = {
                    let mut state = console.writer.state.lock();
                    state.refresh_controller();
                    if cmd == KDGETMODE {
                        state.display as i32
                    } else {
                        state.keyboard as i32
                    }
                };
                (arg as *mut i32).write_vm(value)?;
            }
            _ => return Err(KError::NotATty),
        }
        Ok(0)
    })())
}

/// Independent fixed VT, with its own terminal/session and controller lifetime.
///
/// tty0/tty1 share this object. TEXT forwards output to the native console's
/// byte connection; GRAPHICS suppresses this VT's text without redirecting the
/// serial TTY's input, session or output. There is no VT keyboard byte source.
/// Current-terminal opens preserve this object's ioctl and controller state.
pub type VtConsole = Tty<VtInput, VtOutput>;

/// Unconnected VT input port; it never consumes the serial console's bytes.
pub struct VtInput(Arc<InputIngress>);

impl TtyRead for VtInput {
    fn read(&mut self, _buf: &mut [u8]) -> usize {
        0
    }

    fn input_ingress(&self) -> Option<Arc<InputIngress>> {
        Some(self.0.clone())
    }
}

/// VT-owned text policy over the native console's byte output connection.
#[derive(Clone)]
pub struct VtOutput {
    state: Arc<Mutex<ConsoleState>>,
}

impl TtyWrite for VtOutput {
    fn handles_output_processing(&self) -> bool {
        true
    }

    fn write(&self, buf: &[u8]) -> KResult<usize> {
        let mut state = self.state.lock();
        state.refresh_controller();
        // Serialize with KDSETMODE: after GRAPHICS is installed no older TEXT
        // writer can emit bytes. User copies and caller lookup occur outside it.
        if state.display == DisplayMode::Graphics {
            Ok(buf.len())
        } else {
            // Delay native-console initialization until actual text output.
            // Sharing this byte connection never borrows its terminal/session.
            super::N_TTY.writer.write(buf)
        }
    }
}

fn new_console() -> Arc<VtConsole> {
    let ingress = Arc::new(InputIngress::default());
    ingress.close();
    Tty::new(
        Arc::default(),
        TtyKind::VirtualConsole,
        TtyConfig {
            reader: VtInput(ingress),
            writer: VtOutput {
                state: Arc::new(Mutex::new(ConsoleState::default())),
            },
            process_mode: ProcessMode::Manual,
        },
    )
}

lazy_static! {
    /// Single fixed VT; its controlling-terminal identity is separate from N_TTY.
    pub static ref VT_CONSOLE: Arc<VtConsole> = new_console();
}

static VT_DEVICES: Once<KResult<[CharacterDeviceRegistration; 2]>> = Once::new();

/// Registers both fixed VT aliases, returning their common cdev-owned identities.
///
/// Requires sleepable task context. Initialization is cached; on a partial
/// failure the first registration is removed, so no filesystem advertises a
/// nonexistent second alias or a half-published pair.
///
/// # Errors
/// Propagates registration conflicts and invalid identities from KVFS.
pub fn register_fixed_vt_devices() -> KResult<[CharacterDeviceRegistration; 2]> {
    VT_DEVICES
        .call_once(|| {
            let first = cdev_add_named(
                DeviceId::new(4, 0),
                String::from("tty0"),
                String::from("tty"),
                VT_CONSOLE.clone(),
            )?;
            match cdev_add_named(
                DeviceId::new(4, 1),
                String::from("tty1"),
                String::from("tty"),
                VT_CONSOLE.clone(),
            ) {
                Ok(second) => Ok([first, second]),
                Err(error) => {
                    cdev_del(first.device_id(), 1);
                    Err(error)
                }
            }
        })
        .clone()
}

unittest::path_mod_test!(tests, "../../ut-tests/tty_vt.rs");
