// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::manifest::{AutostartEntry, Uapp};

pub fn render(uapps: &[Uapp]) -> String {
    let mut script = String::new();
    script.push_str("#!/bin/sh\n\n");
    script.push_str("[ -n \"${XKERNEL_AUTOSTART_DONE:-}\" ] && return 0\n");
    script.push_str("export XKERNEL_AUTOSTART_DONE=1\n\n");
    script.push_str("echo \"[autostart] x-kernel uapp startup hook triggered\"\n");
    script.push_str("date\n\n");
    script.push_str(
        r#"start_background() {
    name="$1"
    check_alive="$2"
    command="$3"

    echo "[autostart] starting ${name}"
    sh -c "${command}" &
    pid="$!"

    if [ "${check_alive}" = "yes" ]; then
        sleep 1
        if ! kill -0 "${pid}" 2>/dev/null; then
            echo "[autostart] ${name} failed to start"
            return 1
        fi
    fi
}

start_foreground() {
    name="$1"
    command="$2"

    echo "[autostart] starting ${name}"
    sh -c "${command}"
}

"#,
    );

    for uapp in uapps {
        let entries: Vec<&AutostartEntry> = uapp
            .manifest
            .autostart
            .iter()
            .filter(|entry| entry.is_enabled())
            .collect();
        if entries.is_empty() {
            continue;
        }

        script.push_str(&format!("# uapp: {}\n", uapp.name()));
        for entry in entries {
            render_entry(&mut script, entry);
        }
        script.push('\n');
    }

    script
}

pub fn count_entries(uapps: &[Uapp]) -> usize {
    uapps
        .iter()
        .map(|uapp| {
            uapp.manifest
                .autostart
                .iter()
                .filter(|entry| entry.is_enabled())
                .count()
        })
        .sum()
}

fn render_entry(script: &mut String, entry: &AutostartEntry) {
    let command = if let Some(workdir) = &entry.workdir {
        format!("cd {} && {}", shell_quote(workdir), entry.command)
    } else {
        entry.command.clone()
    };
    let name = shell_quote(&entry.name);
    let command = shell_quote(&command);

    if entry.runs_in_background() {
        let check_alive = if entry.should_check_alive() {
            "yes"
        } else {
            "no"
        };
        script.push_str(&format!(
            "start_background {name} {check_alive} {command} || return 1\n"
        ));
    } else {
        script.push_str(&format!("start_foreground {name} {command} || return 1\n"));
    }

    if entry.should_exit() {
        script.push_str("exit 0\n");
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
