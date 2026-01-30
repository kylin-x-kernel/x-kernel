use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::str;

use log::{error, info, warn};
use uefi::{
    prelude::*,
    proto::media::file::{File, FileAttribute, FileMode, FileType, RegularFile},
};

const AXBOOT_CONFIG_PATH: &str = "axboot.toml";

pub(crate) struct AxBootConfig {
    pub(crate) kernel_paths: Vec<String>,
    pub(crate) kernel_base_paddr: u64,
    pub(crate) phys_virt_offset: u64,
    pub(crate) multiboot_magic: u64,
}

impl AxBootConfig {
    fn defaults() -> Self {
        Self {
            kernel_paths: alloc::vec!["hello-kernel".to_string(), "hello-kernel.bin".to_string()],
            kernel_base_paddr: 0x20_0000,
            phys_virt_offset: 0xffff_8000_0000_0000,
            multiboot_magic: 0x2bad_b002,
        }
    }
}

pub(crate) fn load_config(image: Handle) -> AxBootConfig {
    let mut cfg = AxBootConfig::defaults();

    let mut fs = match uefi::boot::get_image_file_system(image) {
        Ok(v) => v,
        Err(err) => {
            warn!("config: get_image_file_system failed: {:?}", err.status());
            return cfg;
        }
    };

    let mut root = match fs.open_volume() {
        Ok(v) => v,
        Err(err) => {
            warn!("config: open_volume failed: {:?}", err.status());
            return cfg;
        }
    };

    let path16 = match uefi::CString16::try_from(AXBOOT_CONFIG_PATH) {
        Ok(v) => v,
        Err(_) => return cfg,
    };

    let file = match root.open(&path16, FileMode::Read, FileAttribute::empty()) {
        Ok(handle) => match handle.into_type().map_err(|e| e.status()) {
            Ok(FileType::Regular(f)) => Some(f),
            Ok(_) => None,
            Err(err) => {
                warn!("{}: open failed: {:?}", AXBOOT_CONFIG_PATH, err);
                None
            }
        },
        Err(_) => None,
    };

    let mut file = match file {
        Some(f) => f,
        None => {
            info!("{} not found, using defaults", AXBOOT_CONFIG_PATH);
            return cfg;
        }
    };

    let data = match read_file(&mut file) {
        Ok(v) => v,
        Err(err) => {
            warn!("{} read failed: {:?}", AXBOOT_CONFIG_PATH, err);
            return cfg;
        }
    };

    let content = match str::from_utf8(&data) {
        Ok(v) => v,
        Err(_) => {
            warn!("{} is not valid UTF-8, using defaults", AXBOOT_CONFIG_PATH);
            return cfg;
        }
    };

    if let Err(err) = parse_config(content, &mut cfg) {
        error!("{} parse error: {}", AXBOOT_CONFIG_PATH, err);
    }

    info!(
        "{}: kernel_paths={:?} kernel_base_paddr={:#x} phys_virt_offset={:#x} \
         multiboot_magic={:#x}",
        AXBOOT_CONFIG_PATH,
        cfg.kernel_paths,
        cfg.kernel_base_paddr,
        cfg.phys_virt_offset,
        cfg.multiboot_magic
    );

    cfg
}

fn read_file(file: &mut RegularFile) -> Result<Vec<u8>, Status> {
    let info = file
        .get_boxed_info::<uefi::proto::media::file::FileInfo>()
        .map_err(|e| e.status())?;
    let file_size = info.file_size() as usize;
    let mut data = alloc::vec![0u8; file_size];
    let mut read = 0usize;
    while read < file_size {
        let slice = &mut data[read..];
        let len = file.read(slice).map_err(|e| e.status())?;
        if len == 0 {
            break;
        }
        read += len;
    }
    data.truncate(read);
    Ok(data)
}

fn parse_config(content: &str, cfg: &mut AxBootConfig) -> Result<(), &'static str> {
    for line in content.lines() {
        let mut line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(idx) = line.find('#') {
            line = &line[..idx];
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap().trim();
        let value = parts.next().ok_or("missing '='")?.trim();

        match key {
            "kernel_paths" => {
                let paths = parse_string_list(value)?;
                if !paths.is_empty() {
                    cfg.kernel_paths = paths;
                }
            }
            "kernel_base_paddr" => cfg.kernel_base_paddr = parse_u64(value)?,
            "phys_virt_offset" => cfg.phys_virt_offset = parse_u64(value)?,
            "multiboot_magic" => cfg.multiboot_magic = parse_u64(value)?,
            _ => {}
        }
    }
    Ok(())
}

fn parse_string_list(value: &str) -> Result<Vec<String>, &'static str> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return Err("kernel_paths must be an array");
    }
    let inner = &value[1..value.len() - 1];
    let mut out = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let item = item
            .trim_start_matches('"')
            .trim_end_matches('"')
            .to_string();
        if !item.is_empty() {
            out.push(item);
        }
    }
    Ok(out)
}

fn parse_u64(value: &str) -> Result<u64, &'static str> {
    let v = value.trim().trim_matches('"').replace('_', "");
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| "invalid hex number")
    } else {
        v.parse::<u64>().map_err(|_| "invalid number")
    }
}
