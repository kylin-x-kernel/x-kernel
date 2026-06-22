// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

use crate::loader::sanitize_kernel_path;

const AXBOOT_CONFIG_PATH: &str = "axboot.toml";

pub(crate) struct AxBootConfig {
    pub(crate) kernel_paths: Vec<String>,
}

impl AxBootConfig {
    fn defaults() -> Self {
        Self {
            kernel_paths: alloc::vec!["hello-kernel".to_string()],
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
        "{}: kernel_paths={:?}",
        AXBOOT_CONFIG_PATH, cfg.kernel_paths
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
                let mut valid_paths = Vec::new();
                for path in paths {
                    match sanitize_kernel_path(&path) {
                        Ok(path) => valid_paths.push(path),
                        Err(_) => warn!("ignoring invalid kernel path from config: {:?}", path),
                    }
                }
                if !valid_paths.is_empty() {
                    cfg.kernel_paths = valid_paths;
                } else {
                    warn!("kernel_paths contains no valid entries, keeping defaults");
                }
            }
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
