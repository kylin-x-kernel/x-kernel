// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 LinuxBoot and UEFI artifact construction.

use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use goblin::elf::Elf;

use crate::{
    context::BuildContext,
    error::{Error, IoResultExt, Result},
    process::Process,
};

const SECTOR_SIZE: usize = 512;
const MIN_SETUP_SECTORS: usize = 5;
const SETUP_SECTORS_OFFSET: usize = 0x1f1;
const SYSSIZE_OFFSET: usize = 0x1f4;
const PAYLOAD_OFFSET_OFFSET: usize = 0x248;
const PAYLOAD_LENGTH_OFFSET: usize = 0x24c;
const INIT_SIZE_OFFSET: usize = 0x260;
const UEFI_IMAGE_SIZE_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) struct X86BootInputs {
    boot_stub_elf: PathBuf,
    uefi_loader_efi: PathBuf,
}

impl X86BootInputs {
    pub(crate) fn prepare(context: &BuildContext) -> Result<Self> {
        let inputs = Self::new(context);
        build_boot_stub(context)?;
        build_uefi_loader(context)?;
        Ok(inputs)
    }

    pub(crate) fn latest_modified(&self, context: &BuildContext) -> Result<Option<SystemTime>> {
        let source_paths = [
            context.workspace_root.join("boot/x86_64-linuxboot"),
            context.workspace_root.join("boot/x86_64-boot-stub"),
            context.workspace_root.join("boot/x86_64-uefi-loader"),
            context.workspace_root.join("boot/x86-boot-common"),
            context.workspace_root.join("xtask/xkmake/src/x86.rs"),
        ];

        let mut latest = None;
        for path in source_paths {
            collect_latest_modified(&path, &mut latest)?;
        }
        for path in [&self.boot_stub_elf, &self.uefi_loader_efi] {
            if !path.is_file() {
                return Ok(None);
            }
            update_latest(path, &mut latest)?;
        }
        Ok(latest)
    }

    pub(crate) fn create_linuxboot_image(
        &self,
        context: &BuildContext,
        kernel_elf: &Path,
        output: &Path,
    ) -> Result<()> {
        let work_dir = context.target_dir.join("tools/xkmake/x86_64");
        fs::create_dir_all(&work_dir).with_path(&work_dir)?;

        let setup_source = context.workspace_root.join("boot/x86_64-linuxboot/setup.S");
        let setup_linker = context
            .workspace_root
            .join("boot/x86_64-linuxboot/linker.lds");
        let setup_object = work_dir.join("linuxboot-setup.o");
        let setup_elf = work_dir.join("linuxboot-setup.elf");
        let setup_bin = work_dir.join("linuxboot-setup.bin");
        let boot_stub_bin = work_dir.join("x86_64-boot-stub.bin");
        let tool_prefix = context.config.arch().cross_compile_prefix();

        run(context, format!("{tool_prefix}gcc"))
            .args(["-m32", "-c", "-nostdlib", "-o"])
            .arg(&setup_object)
            .arg(&setup_source)
            .run()?;
        run(context, format!("{tool_prefix}ld"))
            .args(["-m", "elf_i386", "-T"])
            .arg(&setup_linker)
            .arg("-o")
            .arg(&setup_elf)
            .arg(&setup_object)
            .run()?;
        run(context, format!("{tool_prefix}objcopy"))
            .arg("-O")
            .arg("binary")
            .arg(&setup_elf)
            .arg(&setup_bin)
            .run()?;
        run(context, "rust-objcopy")
            .arg("--binary-architecture=x86_64")
            .arg(&self.boot_stub_elf)
            .args(["--strip-all", "-O", "binary"])
            .arg(&boot_stub_bin)
            .run()?;
        write_linuxboot_image(
            &setup_bin,
            &self.boot_stub_elf,
            &boot_stub_bin,
            kernel_elf,
            output,
        )
    }

    pub(crate) fn create_uefi_image(
        &self,
        context: &BuildContext,
        kernel_elf: &Path,
        output: &Path,
    ) -> Result<()> {
        let work_dir = context.target_dir.join("tools/xkmake/x86_64");
        fs::create_dir_all(&work_dir).with_path(&work_dir)?;
        let config_path = work_dir.join("axboot.toml");
        fs::write(
            &config_path,
            "# bootloader config\nkernel_paths = [\"kernel.elf\"]\n",
        )
        .with_path(&config_path)?;

        let image = fs::File::create(output).with_path(output)?;
        image.set_len(UEFI_IMAGE_SIZE_BYTES).with_path(output)?;
        drop(image);

        run(context, "mkfs.fat")
            .args(["-F", "32"])
            .arg(output)
            .run()?;
        run(context, "mmd")
            .arg("-i")
            .arg(output)
            .args(["::/EFI", "::/EFI/BOOT"])
            .run()?;
        run(context, "mcopy")
            .arg("-i")
            .arg(output)
            .arg(&self.uefi_loader_efi)
            .arg("::/EFI/BOOT/BOOTX64.EFI")
            .run()?;
        run(context, "mcopy")
            .arg("-i")
            .arg(output)
            .arg(kernel_elf)
            .arg("::/kernel.elf")
            .run()?;
        run(context, "mcopy")
            .arg("-i")
            .arg(output)
            .arg(&config_path)
            .arg("::/axboot.toml")
            .run()
    }

    fn new(context: &BuildContext) -> Self {
        Self {
            boot_stub_elf: context
                .target_dir
                .join("x86_64-unknown-none/release/x86_64-boot-stub"),
            uefi_loader_efi: context
                .target_dir
                .join("x86_64-unknown-uefi/release/x86_64-uefi-loader.efi"),
        }
    }
}

fn build_boot_stub(context: &BuildContext) -> Result<()> {
    cargo_build(context, "x86_64-boot-stub", "x86_64-unknown-none").run()
}

fn build_uefi_loader(context: &BuildContext) -> Result<()> {
    cargo_build(context, "x86_64-uefi-loader", "x86_64-unknown-uefi").run()
}

fn cargo_build(context: &BuildContext, package: &str, target: &str) -> Process {
    let mut command = run(context, "cargo");
    command
        .arg("build")
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(target)
        .arg("--target-dir")
        .arg(&context.target_dir)
        .arg("--release")
        .env("RUSTC_BOOTSTRAP", "1")
        .env("TARGET_DIR", &context.target_dir)
        .env("K_ARCH", context.config.arch().as_str())
        .env("K_TARGET", context.config.target())
        .env("K_PLAT_NAME", context.config.platform())
        .env("K_MODE", context.config.profile().as_str())
        .env("K_IP", context.guest_ip.to_string())
        .env("K_GW", context.gateway.to_string())
        // Empty explicit values isolate boot helpers from kernel target flags in
        // `.cargo/.xconfig.toml` while retaining Cargo's normal target defaults.
        .env("RUSTFLAGS", "")
        .env("CARGO_ENCODED_RUSTFLAGS", "");
    command
}

fn run(context: &BuildContext, program: impl Into<std::ffi::OsString>) -> Process {
    let mut command = Process::new(program, context.dry_run, context.verbosity);
    command.current_dir(&context.workspace_root);
    command
}

fn collect_latest_modified(path: &Path, latest: &mut Option<SystemTime>) -> Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path).with_path(path)? {
            let entry = entry.with_path(path)?;
            collect_latest_modified(&entry.path(), latest)?;
        }
    } else {
        update_latest(path, latest)?;
    }
    Ok(())
}

fn update_latest(path: &Path, latest: &mut Option<SystemTime>) -> Result<()> {
    let modified = fs::metadata(path)
        .with_path(path)?
        .modified()
        .with_path(path)?;
    if latest.is_none_or(|current| modified > current) {
        *latest = Some(modified);
    }
    Ok(())
}

fn write_linuxboot_image(
    setup_path: &Path,
    boot_stub_elf_path: &Path,
    boot_stub_bin_path: &Path,
    kernel_elf_path: &Path,
    output_path: &Path,
) -> Result<()> {
    let setup = fs::read(setup_path).with_path(setup_path)?;
    let boot_stub_elf = fs::read(boot_stub_elf_path).with_path(boot_stub_elf_path)?;
    let boot_stub = fs::read(boot_stub_bin_path).with_path(boot_stub_bin_path)?;
    let kernel_elf_size = fs::metadata(kernel_elf_path)
        .with_path(kernel_elf_path)?
        .len();
    let kernel_elf_size = usize::try_from(kernel_elf_size)
        .map_err(|_| Error::Message("x86 kernel ELF size does not fit usize".to_string()))?;

    let image_start = elf_symbol_value(&boot_stub_elf, "__image_start")?;
    let image_end = elf_symbol_value(&boot_stub_elf, "__image_end")?;
    let boot_stub_image_size = image_end.checked_sub(image_start).ok_or_else(|| {
        Error::Message("x86 boot stub has an invalid linked image range".to_string())
    })?;
    let boot_stub_image_size = usize::try_from(boot_stub_image_size).map_err(|_| {
        Error::Message("x86 boot stub linked image size does not fit usize".to_string())
    })?;
    let prefix =
        assemble_linuxboot_prefix(&setup, &boot_stub, boot_stub_image_size, kernel_elf_size)?;
    let output = fs::File::create(output_path).with_path(output_path)?;
    let mut output = BufWriter::new(output);
    output.write_all(&prefix).with_path(output_path)?;
    let mut kernel_elf = fs::File::open(kernel_elf_path).with_path(kernel_elf_path)?;
    std::io::copy(&mut kernel_elf, &mut output).with_path(output_path)?;
    output.flush().with_path(output_path)
}

fn elf_symbol_value(elf_bytes: &[u8], name: &str) -> Result<u64> {
    let elf = Elf::parse(elf_bytes)
        .map_err(|error| Error::Message(format!("invalid x86 boot stub ELF: {error}")))?;
    elf.syms
        .iter()
        .find_map(|symbol| {
            (elf.strtab.get_at(symbol.st_name) == Some(name)).then_some(symbol.st_value)
        })
        .ok_or_else(|| Error::Message(format!("missing symbol {name} in x86 boot stub ELF")))
}

fn assemble_linuxboot_prefix(
    setup: &[u8],
    boot_stub: &[u8],
    boot_stub_image_size: usize,
    kernel_elf_size: usize,
) -> Result<Vec<u8>> {
    if boot_stub.len() > boot_stub_image_size {
        return Err(Error::Message(
            "flat x86 boot stub exceeds its linked image size".to_string(),
        ));
    }

    let setup_sectors = setup.len().div_ceil(SECTOR_SIZE).max(MIN_SETUP_SECTORS);
    let setup_size = setup_sectors
        .checked_mul(SECTOR_SIZE)
        .ok_or_else(|| Error::Message("x86 LinuxBoot setup size overflow".to_string()))?;
    let protected_mode_size = boot_stub_image_size
        .checked_add(kernel_elf_size)
        .ok_or_else(|| Error::Message("x86 LinuxBoot payload size overflow".to_string()))?;

    let mut image = setup.to_vec();
    image.resize(setup_size, 0);
    write_u8(
        &mut image,
        SETUP_SECTORS_OFFSET,
        u8::try_from(setup_sectors - 1)
            .map_err(|_| Error::Message("x86 LinuxBoot setup is too large".to_string()))?,
    )?;
    write_u32(
        &mut image,
        SYSSIZE_OFFSET,
        u32::try_from(protected_mode_size.div_ceil(16))
            .map_err(|_| Error::Message("x86 LinuxBoot syssize is too large".to_string()))?,
    )?;
    write_u32(
        &mut image,
        PAYLOAD_OFFSET_OFFSET,
        u32::try_from(boot_stub_image_size)
            .map_err(|_| Error::Message("x86 LinuxBoot payload offset is too large".to_string()))?,
    )?;
    write_u32(
        &mut image,
        PAYLOAD_LENGTH_OFFSET,
        u32::try_from(kernel_elf_size)
            .map_err(|_| Error::Message("x86 LinuxBoot kernel payload is too large".to_string()))?,
    )?;
    write_u32(
        &mut image,
        INIT_SIZE_OFFSET,
        u32::try_from(protected_mode_size)
            .map_err(|_| Error::Message("x86 LinuxBoot init size is too large".to_string()))?,
    )?;

    image.extend_from_slice(boot_stub);
    image.resize(setup_size + boot_stub_image_size, 0);
    Ok(image)
}

fn write_u8(image: &mut [u8], offset: usize, value: u8) -> Result<()> {
    let byte = image.get_mut(offset).ok_or_else(|| {
        Error::Message(format!(
            "x86 LinuxBoot setup is missing header byte at offset {offset:#x}"
        ))
    })?;
    *byte = value;
    Ok(())
}

fn write_u32(image: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let end = offset
        .checked_add(size_of::<u32>())
        .ok_or_else(|| Error::Message("x86 LinuxBoot header offset overflow".to_string()))?;
    let destination = image.get_mut(offset..end).ok_or_else(|| {
        Error::Message(format!(
            "x86 LinuxBoot setup is missing header field at offset {offset:#x}"
        ))
    })?;
    destination.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        INIT_SIZE_OFFSET, PAYLOAD_LENGTH_OFFSET, PAYLOAD_OFFSET_OFFSET, SECTOR_SIZE,
        SETUP_SECTORS_OFFSET, SYSSIZE_OFFSET, assemble_linuxboot_prefix,
    };

    #[test]
    fn linuxboot_image_contains_patched_header_stub_and_kernel() {
        let setup = vec![0x11; 0x280];
        let boot_stub = vec![0x22; 7];
        let kernel = vec![0x33; 13];
        let mut image = assemble_linuxboot_prefix(&setup, &boot_stub, 16, kernel.len()).unwrap();
        image.extend_from_slice(&kernel);
        let setup_size = 5 * SECTOR_SIZE;

        assert_eq!(image[SETUP_SECTORS_OFFSET], 4);
        assert_eq!(read_u32(&image, SYSSIZE_OFFSET), 2);
        assert_eq!(read_u32(&image, PAYLOAD_OFFSET_OFFSET), 16);
        assert_eq!(read_u32(&image, PAYLOAD_LENGTH_OFFSET), 13);
        assert_eq!(read_u32(&image, INIT_SIZE_OFFSET), 29);
        assert_eq!(&image[setup_size..setup_size + 7], &boot_stub);
        assert!(
            image[setup_size + 7..setup_size + 16]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(&image[setup_size + 16..], &kernel);
    }

    fn read_u32(image: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap())
    }
}
