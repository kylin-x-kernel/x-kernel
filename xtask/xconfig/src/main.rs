// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use xconfig::cli::run_cli;

fn main() {
    if let Err(e) = run_cli() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
