// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

fn main() {
    println!("Hello, tee_app1!");
    let mut count = 0u64;
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        count += 1;
        println!("tee_app1 tick #{count}");
    }
}
