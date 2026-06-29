// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    env, fs,
    sync::{Mutex, OnceLock},
};

use tempfile::TempDir;
use xconfig::cli::gen_cargo_command;

fn cwd_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn test_gen_cargo_uses_effective_kconfig_defaults_from_minimal_config() {
    let _guard = cwd_test_lock().lock().unwrap();
    let temp_dir = TempDir::new().unwrap();
    let old_cwd = env::current_dir().unwrap();

    fs::write(
        temp_dir.path().join("Kconfig"),
        r#"
choice
    prompt "Architecture"
    default ARCH_AARCH64

config ARCH_AARCH64
    bool "aarch64"

config ARCH_X86_64
    bool "x86_64"
endchoice

config ARCH
    string
    default "aarch64" if ARCH_AARCH64
    default "x86_64" if ARCH_X86_64

choice
    prompt "Platform"
    default PLATFORM_AARCH64_QEMU_VIRT

config PLATFORM_AARCH64_QEMU_VIRT
    bool "aarch64 qemu"
    depends on ARCH_AARCH64

config PLATFORM_X86_64_QEMU_VIRT
    bool "x86_64 qemu"
    depends on ARCH_X86_64
endchoice

config KFEAT_CHAR
    bool
    default y

config KFEAT_DRIVER_CONSOLE_PL011
    bool
    depends on KFEAT_CHAR && PLATFORM_AARCH64_QEMU_VIRT
    default y if PLATFORM_AARCH64_QEMU_VIRT
"#,
    )
    .unwrap();

    let config_path = temp_dir.path().join(".config");
    fs::write(
        &config_path,
        "ARCH_AARCH64=y\nPLATFORM_AARCH64_QEMU_VIRT=y\n",
    )
    .unwrap();

    env::set_current_dir(temp_dir.path()).unwrap();
    let result = gen_cargo_command(config_path, false, None, false);
    env::set_current_dir(old_cwd).unwrap();
    result.unwrap();

    let cargo_config = fs::read_to_string(temp_dir.path().join(".cargo/.xconfig.toml")).unwrap();
    assert!(cargo_config.contains("kfeat/driver_console_pl011"));
    assert!(cargo_config.contains("kfeat/platform_aarch64_qemu_virt"));
}

#[test]
fn test_gen_cargo_uses_transitive_select_imply_effective_features() {
    let _guard = cwd_test_lock().lock().unwrap();
    let temp_dir = TempDir::new().unwrap();
    let old_cwd = env::current_dir().unwrap();

    fs::write(
        temp_dir.path().join("Kconfig"),
        r#"
choice
    prompt "Architecture"
    default ARCH_AARCH64

config ARCH_AARCH64
    bool "aarch64"

config ARCH_X86_64
    bool "x86_64"
endchoice

choice
    prompt "Platform"
    default PLATFORM_AARCH64_QEMU_VIRT

config PLATFORM_AARCH64_QEMU_VIRT
    bool "aarch64 qemu"
    depends on ARCH_AARCH64

config PLATFORM_X86_64_QEMU_VIRT
    bool "x86_64 qemu"
    depends on ARCH_X86_64
endchoice

config KFEAT_TRANSITIVE_IMPLIED
    bool

config KFEAT_SELECTED_HELPER
    bool
    imply KFEAT_TRANSITIVE_IMPLIED

config KFEAT_ROOT
    bool
    select KFEAT_SELECTED_HELPER
"#,
    )
    .unwrap();

    let config_path = temp_dir.path().join(".config");
    fs::write(
        &config_path,
        "ARCH_AARCH64=y\nPLATFORM_AARCH64_QEMU_VIRT=y\nKFEAT_ROOT=y\n",
    )
    .unwrap();

    env::set_current_dir(temp_dir.path()).unwrap();
    let result = gen_cargo_command(config_path, false, None, false);
    env::set_current_dir(old_cwd).unwrap();
    result.unwrap();

    let cargo_config = fs::read_to_string(temp_dir.path().join(".cargo/.xconfig.toml")).unwrap();
    assert!(cargo_config.contains("kfeat/root"));
    assert!(cargo_config.contains("kfeat/selected_helper"));
    assert!(cargo_config.contains("kfeat/transitive_implied"));
}
