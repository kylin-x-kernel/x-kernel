// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_os_api::PortOp`] provider contract.

use driver_os_api::{PortOp, PortWidth, ResError, ResResult};

use crate::XKernelResourceProvider;

/// Returns whether `port` plus a `width` access stays inside the 16-bit port
/// space.
#[cfg(any(target_arch = "x86_64", unittest))]
fn checked_port(port: u16, width: PortWidth) -> ResResult<()> {
    let last = port as u32 + width.bytes() as u32 - 1;
    if last > u16::MAX as u32 {
        return Err(ResError::InvalidResource);
    }
    Ok(())
}

#[cfg(any(target_arch = "x86_64", unittest))]
fn checked_write(port: u16, width: PortWidth, value: u32) -> ResResult<()> {
    checked_port(port, width)?;
    if !width.accepts(value) {
        return Err(ResError::InvalidResource);
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
impl PortOp for XKernelResourceProvider {
    fn read_port(&self, port: u16, width: PortWidth) -> ResResult<u32> {
        checked_port(port, width)?;
        use x86_64::instructions::port::Port;

        Ok(match width {
            // SAFETY: `checked_port` keeps the access inside the 16-bit port
            // space; reading an unbacked port has no memory-safety effect.
            PortWidth::U8 => (unsafe { Port::<u8>::new(port).read() }) as u32,
            // SAFETY: as above, and the access fits the 16-bit port space.
            PortWidth::U16 => (unsafe { Port::<u16>::new(port).read() }) as u32,
            // SAFETY: as above, and the access fits the 16-bit port space.
            PortWidth::U32 => unsafe { Port::<u32>::new(port).read() },
        })
    }

    fn write_port(&self, port: u16, width: PortWidth, value: u32) -> ResResult<()> {
        checked_write(port, width, value)?;
        use x86_64::instructions::port::Port;

        match width {
            // SAFETY: `checked_write` keeps the access inside the 16-bit port
            // space and the value fits this width; a port write has no
            // memory-safety effect.
            PortWidth::U8 => unsafe { Port::<u8>::new(port).write(value as u8) },
            // SAFETY: as above, and `checked_write` keeps the access inside
            // the 16-bit port space.
            PortWidth::U16 => unsafe { Port::<u16>::new(port).write(value as u16) },
            // SAFETY: as above, and `checked_write` keeps the access inside
            // the 16-bit port space.
            PortWidth::U32 => unsafe { Port::<u32>::new(port).write(value) },
        }
        Ok(())
    }
}

#[cfg(not(target_arch = "x86_64"))]
impl PortOp for XKernelResourceProvider {
    fn read_port(&self, port: u16, width: PortWidth) -> ResResult<u32> {
        let _ = (port, width);
        Err(ResError::Unsupported)
    }

    fn write_port(&self, port: u16, width: PortWidth, value: u32) -> ResResult<()> {
        let _ = (port, width, value);
        Err(ResError::Unsupported)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/port.rs");
