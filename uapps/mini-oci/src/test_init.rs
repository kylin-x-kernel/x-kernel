// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

fn main() {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").expect("read cgroup membership");
    assert!(
        cgroup.starts_with("0::/oci-smoke"),
        "unexpected cgroup: {cgroup:?}"
    );
    assert_eq!(std::env::current_dir().unwrap().to_str(), Some("/"));
    assert_eq!(std::env::var("OCI_SMOKE").as_deref(), Ok("1"));
    println!("OCI_SMOKE_PASS pid={}", std::process::id());
}
