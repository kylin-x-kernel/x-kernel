use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=KBUILD_BUILD_MACHINE");
    println!("cargo:rerun-if-env-changed=KBUILD_BUILD_TIME");
    println!("cargo:rerun-if-env-changed=KBUILD_BUILD_INFO");
    let build_machine = env::var("KBUILD_BUILD_MACHINE").unwrap_or_default();
    let build_time = env::var("KBUILD_BUILD_TIME").unwrap_or_default();
    let build_info = env::var("KBUILD_BUILD_INFO").unwrap_or_default();
    println!("cargo:rustc-env=KBUILD_BUILD_MACHINE={build_machine}");
    println!("cargo:rustc-env=KBUILD_BUILD_TIME={build_time}");
    println!("cargo:rustc-env=KBUILD_BUILD_INFO={build_info}");
}
