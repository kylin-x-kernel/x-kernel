// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-side backtrace symbolication.
//!
//! The kernel prints raw addresses (`Backtrace:` blocks); this tool extracts
//! them from a panic/exception log and resolves them against the unstripped
//! `kernel.debug.elf` via `addr2line`:
//!
//! ```text
//! $ make symbolize LOG=panic.log
//! Backtrace:
//! 0: 0xffff000040123456
//!     panic at kernel/foo.rs:42
//! ```

use std::{
    fs, io,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use xconfig::build_config::resolve_kernel_config;

use crate::{
    build::Bundle,
    cli::SymbolizeArgs,
    context::{ensure_config_exists, workspace_root},
    error::{Error, IoResultExt, Result},
};

/// Extract a raw backtrace address from a frame line.
///
/// Accepts the stable kernel format `0: 0xffff000040123456` (with or without
/// a trailing `func+0xoff/0xsize` annotation). Addresses shorter than 32
/// bits are ignored to avoid matching line numbers and other log noise.
fn extract_address(line: &str) -> Option<u64> {
    let trimmed = line.trim_start();
    let (index, rest) = trimmed.split_once(':')?;
    if !index.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = rest.trim_start();
    let hex = rest.strip_prefix("0x")?;
    let hex = &hex[..hex
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hex.len())];
    if hex.len() < 8 {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

/// Frame lines and their extracted addresses, in log order.
struct Frames {
    addresses: Vec<u64>,
    /// 1-based line numbers of frame lines.
    line_numbers: Vec<usize>,
}

/// Collect backtrace frames from a log.
///
/// Strict mode first: lines matching the kernel frame format
/// (`<index>: 0x<address>`). When none match but the log carries panic
/// markers, fall back to scanning for high-half kernel addresses anywhere
/// in a line -- this tolerates serial logs with injected line prefixes
/// (timestamps, ANSI escapes) or partially captured frames.
fn collect_frames(log: &str) -> Frames {
    let mut line_numbers = Vec::new();
    let mut addresses = Vec::new();
    for (number, line) in log.lines().enumerate() {
        if let Some(address) = extract_address(line) {
            line_numbers.push(number + 1);
            addresses.push(address);
        }
    }
    if !line_numbers.is_empty() {
        return Frames {
            addresses,
            line_numbers,
        };
    }

    let lower = log.to_ascii_lowercase();
    if (!lower.contains("backtrace:")
        && !lower.contains("panicked")
        && !lower.contains("kernel panic"))
        // Already symbolicated in-kernel (KFEAT_DWARF frame lines carry
        // `with fp=..., ip=...`): nothing left for the host to resolve.
        || lower.contains(" with fp=")
    {
        return Frames {
            addresses: Vec::new(),
            line_numbers: Vec::new(),
        };
    }
    for (number, line) in log.lines().enumerate() {
        if let Some(address) = first_kernel_address(line) {
            line_numbers.push(number + 1);
            addresses.push(address);
        }
    }
    Frames {
        addresses,
        line_numbers,
    }
}

/// First `0x`-prefixed address in the kernel high half (`>= 0xffff_0000_0000_0000`)
/// found anywhere in the line; user-space addresses are excluded.
fn first_kernel_address(line: &str) -> Option<u64> {
    let mut rest = line;
    while let Some(index) = rest.find("0x") {
        // Only standalone address tokens: the `0x` must follow the line
        // start or whitespace, so `fp=0x...`, `dtb=0x...` and similar
        // key=value noise never counts as a frame.
        let is_boundary = index == 0 || rest.as_bytes()[index - 1].is_ascii_whitespace();
        let hex = &rest[index + 2..];
        let end = hex
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(hex.len());
        let hex = &hex[..end];
        if is_boundary
            && hex.len() >= 8
            && let Ok(address) = u64::from_str_radix(hex, 16)
            && address >= 0xffff_0000_0000_0000
        {
            return Some(address);
        }
        rest = &rest[index + 2 + end..];
    }
    None
}

pub(crate) fn run(args: &SymbolizeArgs) -> Result<()> {
    let workspace_root = workspace_root()?;

    let elf = match &args.elf {
        // An explicit ELF must work standalone (e.g. an archived log plus
        // the matching debug ELF in a clean checkout) — no kernel config
        // required.
        Some(path) => workspace_root.join(path),
        None => {
            // Lightweight path derivation, mirroring `context::print_config`:
            // only resolve the kernel config and compute the bundle
            // location, without fabricating a full build context
            // (app/guest_ip/gateway are irrelevant to symbolication).
            let config_path = workspace_root.join(&args.workspace.config);
            ensure_config_exists(&config_path)?;
            let config = resolve_kernel_config(
                &config_path,
                workspace_root.join("Kconfig"),
                &workspace_root,
            )?;
            workspace_root
                .join(&args.workspace.target_dir)
                .join("xkmake")
                .join(config.platform())
                .join(config.profile().as_str())
                .join("kernel.debug.elf")
        }
    };
    if !elf.is_file() {
        return Err(Error::Message(format!(
            "no debug ELF at {}; run `make build` first (or pass --elf)",
            elf.display()
        )));
    }

    let log = read_log(&args.log)?;
    let frames = collect_frames(&log);
    if frames.addresses.is_empty() {
        eprintln!("no raw backtrace addresses found in the log");
        return Ok(());
    }

    let offset = parse_offset(args.offset.as_deref())?;
    let tool = find_tool(args.tool.as_deref())?;
    let output = run_addr2line(&tool, &elf, &frames.addresses, offset)?;
    if groups_all_unknown(&output) {
        warn_unresolved(&elf, frames.addresses.len());
    }
    print_resolved(&log, &output, &frames);
    Ok(())
}

/// Automatically symbolicate backtrace frames found in a QEMU log.
///
/// Called after QEMU exits. Silently does nothing when the log contains no
/// raw backtrace frames; missing debug ELF or addr2line degrade to a warning
/// instead of failing the run.
pub(crate) fn auto(bundle: &Bundle, log_path: &std::path::Path) -> Result<()> {
    let log = match fs::read_to_string(log_path) {
        Ok(log) => log,
        Err(_) => return Ok(()),
    };
    let frames = collect_frames(&log);
    if frames.addresses.is_empty() {
        return Ok(());
    }
    let addresses = frames.addresses.clone();

    let elf = &bundle.context.bundle_debug_elf;
    if !elf.is_file() {
        eprintln!(
            "[xkmake] {} backtrace frame(s) found in QEMU output, but {} is missing; run `make \
             build`",
            addresses.len(),
            elf.display()
        );
        return Ok(());
    }
    let tool = match find_tool(None) {
        Ok(tool) => tool,
        Err(error) => {
            eprintln!("[xkmake] {error}");
            return Ok(());
        }
    };
    let output = run_addr2line(&tool, elf, &addresses, 0)?;
    if groups_all_unknown(&output) {
        warn_unresolved(elf, addresses.len());
    }
    println!(
        "\n[xkmake] {} backtrace frame(s) detected in QEMU output; resolved against {}:",
        frames.addresses.len(),
        elf.display()
    );
    print_resolved_frames(&log, &output, &frames);
    Ok(())
}

/// Print only the backtrace frame lines with their symbolication, skipping
/// the surrounding log (which was already shown live by the tee).
fn print_resolved_frames(log: &str, output: &str, frames: &Frames) {
    print!("{}", render_frames(log, output, frames));
}

fn render_frames(log: &str, output: &str, frames: &Frames) -> String {
    let groups = parse_groups(output);
    let mut rendered = String::new();
    let mut group_index = 0;
    for (number, line) in log.lines().enumerate() {
        if frames.line_numbers.binary_search(&(number + 1)).is_err() {
            continue;
        }
        rendered.push_str(&format!("  {line}\n"));
        if let Some(group) = groups.get(group_index) {
            let function = group.first().map(String::as_str).unwrap_or("");
            let location = group.get(1).map(String::as_str).unwrap_or("");
            if function.is_empty() || function == "??" {
                rendered.push_str("    <unknown>\n");
            } else {
                rendered.push_str(&format!("    {function}\n"));
                if !location.is_empty() && location != "??" {
                    rendered.push_str(&format!("    {location}\n"));
                }
            }
        }
        group_index += 1;
    }
    rendered
}

/// Whether every addr2line group failed to produce a function symbol.
fn groups_all_unknown(output: &str) -> bool {
    let groups = parse_groups(output);
    !groups.is_empty()
        && groups.iter().all(|group| {
            group
                .first()
                .is_none_or(|function| function.is_empty() || function == "??")
        })
}

/// Diagnose a fully-unresolved symbolication pass.
fn warn_unresolved(elf: &std::path::Path, addresses: usize) {
    eprintln!(
        "[xkmake] warning: none of the {addresses} address(es) resolved to a symbol in {}; the \
         log may come from a different build, or the kernel may have been loaded at an offset \
         (see --offset)",
        elf.display()
    );
}

fn read_log(path: &Option<PathBuf>) -> Result<String> {
    match path {
        Some(path) => fs::read_to_string(path).with_path(path),
        None => {
            let mut buffer = String::new();
            io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|source| Error::Io {
                    path: PathBuf::from("<stdin>"),
                    source,
                })?;
            Ok(buffer)
        }
    }
}

fn parse_offset(value: Option<&str>) -> Result<u64> {
    let Some(value) = value else {
        return Ok(0);
    };
    let value = value.trim();
    let (radix, digits) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(digits) => (16, digits),
        None => (10, value),
    };
    u64::from_str_radix(digits, radix)
        .map_err(|_| Error::Message(format!("invalid --offset: {value}")))
}

/// Locate an addr2line implementation: preferred tool, then `llvm-addr2line`
/// and `addr2line` on `PATH`, then common LLVM install locations.
fn find_tool(preferred: Option<&Path>) -> Result<PathBuf> {
    if let Some(tool) = preferred {
        return Ok(tool.to_path_buf());
    }
    for name in ["llvm-addr2line", "addr2line"] {
        if let Some(path) = search_path(name) {
            return Ok(path);
        }
    }
    let Ok(entries) = fs::read_dir("/usr/lib") else {
        return Err(no_tool_error());
    };
    let mut candidates = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("llvm-"))
        })
        .map(|path| path.join("bin").join("llvm-addr2line"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.first().cloned().ok_or_else(no_tool_error)
}

fn no_tool_error() -> Error {
    Error::Message(
        "no addr2line tool found; install llvm (llvm-addr2line) or binutils, or pass --tool"
            .to_string(),
    )
}

fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn run_addr2line(tool: &Path, elf: &Path, addresses: &[u64], offset: u64) -> Result<String> {
    let mut command = Command::new(tool);
    command.arg("-afiC").arg("-e").arg(elf);
    for address in addresses {
        command.arg(format!("{:#x}", address + offset));
    }
    let output = command
        .output()
        .map_err(|err| Error::Message(format!("failed to run {}: {err}", tool.display())))?;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "{} failed: {}",
            tool.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Split addr2line output into per-address groups. `-a` prefixes each group
/// with a `0x...` address line.
fn parse_groups(output: &str) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if is_address_line(trimmed) {
            groups.push(Vec::new());
        } else if let Some(last) = groups.last_mut() {
            last.push(trimmed.to_string());
        }
    }
    groups
}

fn is_address_line(line: &str) -> bool {
    line.len() <= 20
        && line.starts_with("0x")
        && line.len() > 2
        && line[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Print the log with symbolication appended to each backtrace frame.
fn print_resolved(log: &str, output: &str, frames: &Frames) {
    let groups = parse_groups(output);
    let mut group_index = 0;
    for (number, line) in log.lines().enumerate() {
        println!("{line}");
        if frames.line_numbers.binary_search(&(number + 1)).is_err() {
            continue;
        }
        let Some(group) = groups.get(group_index) else {
            continue;
        };
        let function = group.first().map(String::as_str).unwrap_or("");
        let location = group.get(1).map(String::as_str).unwrap_or("");
        if function.is_empty() || function == "??" {
            println!("    <unknown>");
        } else {
            println!("    {function}");
            if !location.is_empty() && location != "??" {
                println!("    {location}");
            }
        }
        group_index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_kernel_frame_lines() {
        assert_eq!(
            extract_address("0: 0xffff000040123456"),
            Some(0xffff_0000_4012_3456)
        );
        assert_eq!(
            extract_address("  12: 0xffff000040102abc  panic+0x2f9/0x330"),
            Some(0xffff_0000_4010_2abc)
        );
        // Log noise without a frame index or an address is ignored.
        assert_eq!(extract_address("panic: out of memory"), None);
        assert_eq!(extract_address("0x1234"), None);
        assert_eq!(extract_address(""), None);
    }

    #[test]
    fn parses_offsets() {
        assert_eq!(parse_offset(None).unwrap(), 0);
        assert_eq!(parse_offset(Some("0x1000")).unwrap(), 0x1000);
        assert_eq!(parse_offset(Some("4096")).unwrap(), 4096);
        assert!(parse_offset(Some("nope")).is_err());
    }

    #[test]
    fn groups_addr2line_output() {
        let output = concat!(
            "0x0000000000401234\n",
            "panic\n",
            "/workspace/kernel/foo.rs:42:7\n",
            "0x000000000040102a\n",
            "bar\n",
            "??\n",
        );
        let groups = parse_groups(output);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec!["panic", "/workspace/kernel/foo.rs:42:7"]);
        assert_eq!(groups[1], vec!["bar", "??"]);
    }

    #[test]
    fn render_frames_only_includes_frame_lines() {
        let log = concat!(
            "some log line\n",
            "Backtrace:\n",
            "0: 0xffff800000401234\n",
            "1: 0xffff80000040102a  foo+0x4/0x8\n",
            "trailing noise\n",
        );
        let output = concat!(
            "0x0000000000401234\n",
            "panic\n",
            "/workspace/kernel/foo.rs:42:7\n",
            "0x000000000040102a\n",
            "bar\n",
            "??\n",
        );
        let frames = collect_frames(log);
        let rendered = render_frames(log, output, &frames);
        assert_eq!(
            rendered,
            concat!(
                "  0: 0xffff800000401234\n",
                "    panic\n",
                "    /workspace/kernel/foo.rs:42:7\n",
                "  1: 0xffff80000040102a  foo+0x4/0x8\n",
                "    bar\n",
            )
        );
    }

    #[test]
    fn unresolved_groups_are_detected() {
        let all_unknown = concat!(
            "0x0000000000401234\n",
            "??\n",
            "??:0\n",
            "0x000000000040102a\n",
            "??\n",
            "??:0\n",
        );
        assert!(groups_all_unknown(all_unknown));

        let partially_resolved = concat!(
            "0x0000000000401234\n",
            "panic\n",
            "foo.rs:42\n",
            "0x000000000040102a\n",
            "??\n",
            "??:0\n",
        );
        assert!(!groups_all_unknown(partially_resolved));
    }

    #[test]
    fn loose_mode_recovers_prefixed_and_partial_logs() {
        // Serial tool injected a timestamp prefix: strict frame format misses.
        let prefixed = concat!(
            "[12:34:56] U-Boot 2024.01\n",
            "[12:34:57] panicked at kernel/foo.rs:42:\n",
            "[12:34:57] Backtrace:\n",
            "[12:34:58] 0: 0xffff800000401234\n",
            "[12:34:58] 1: 0xffff80000040102a\n",
        );
        let frames = collect_frames(prefixed);
        assert_eq!(
            frames.addresses,
            vec![0xffff_8000_0040_1234, 0xffff_8000_0040_102a]
        );
        assert_eq!(frames.line_numbers, vec![4, 5]);

        // No panic markers: nothing to recover.
        let unrelated = "some output with 0xffff800000401234 and user addr 0x7ffefffffac0";
        assert!(collect_frames(unrelated).addresses.is_empty());

        // Strict frame lines take priority even without markers.
        let strict = "Backtrace:\n0: 0xffff800000401234\n";
        assert_eq!(
            collect_frames(strict).addresses,
            vec![0xffff_8000_0040_1234]
        );
    }

    #[test]
    fn loose_scan_ignores_key_value_addresses_and_dwarf_output() {
        // key=value addresses must not count as frames.
        let kv = "kimage_voffset=0xffff7fffbfe00000 boot_info=0xffff8000087e7008";
        assert!(first_kernel_address(kv).is_none());
        // Standalone token still matches.
        assert_eq!(
            first_kernel_address(" 0: 0xffff800000401234").unwrap(),
            0xffff_8000_0040_1234
        );
        // In-kernel DWARF output (already symbolicated) suppresses loose scan.
        let dwarf_log = concat!(
            "panicked at entry/src/main.rs:79:\n",
            "Backtrace:\n",
            "   0: rust_begin_unwind\n",
            "        at /workspace/lang_items.rs:11:21 with fp=0xffff00004f2fd5b0, \
             ip=0xffff80000000fb6c\n",
        );
        assert!(collect_frames(dwarf_log).addresses.is_empty());
    }
}
