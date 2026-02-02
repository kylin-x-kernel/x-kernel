use std::{env, path::PathBuf};

fn main() {
    // 1. 编译 C 源码为静态库
    for (key, val) in std::env::vars() {
        if key.contains("MBEDTLS") {
            println!("======================================={}={}", key, val);
        }
    }

    let src_files = [
        "opendice/dice/src/android.c",
        //"opendice/dice/src/boringssl_p256_ops.c",
        "opendice/dice/src/mbedtls_sm2dsa_ops.c",
        "opendice/dice/src/cbor_cert_op.c",
        "opendice/dice/src/cbor_reader.c",
        "opendice/dice/src/cbor_writer.c",
        "opendice/dice/src/clear_memory.c",
        "opendice/dice/src/dice.c",
        "opendice/dice/src/mbedtls_ops.c",
        //"opendice/dice/src/tee_ecdsa_utils.c",
        "opendice/dice/src/mbedtls_sm2dsa_utils.c",
        "opendice/dice/src/utils.c",
        "opendice/tee_dice.c",
    ];

    cc::Build::new()
        .include("opendice/dice/include") // 头文件路径
        .include(env::var("DEP_MBEDTLS_INCLUDE").unwrap()) // 头文件路径
        .files(src_files)
        .flag_if_supported("-fno-stack-protector") // 禁用栈保护
        .compile("opendice");

    // 2. 生成 Rust FFI 绑定
    let bindings = bindgen::Builder::default()
        .header("opendice/dice/include/dice/android.h") // 可以只暴露顶层 API 头文件
        .header("opendice/tee_dice.h")
        .header("opendice/dice/include/dice/mbedtls_sm2dsa_utils.h")
        // .header("opendice/dice/include/dice/tee_ecdsa_utils.h")
        .clang_arg("-Iopendice/dice/include")
        .use_core() // 让 bindgen 使用 core 而不是 std
        .ctypes_prefix("cty") // 用 cty crate 提供的 C 基本类型
        .layout_tests(false)
        .generate()
        .expect("Unable to generate bindings");

    // 3. 输出绑定到 $OUT_DIR/bindings.rs
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("dice-bindings.rs"))
        .expect("Couldn't write bindings!");

    // 4. 告诉 cargo 链接静态库
    println!("cargo:rustc-link-lib=static=opendice");
    println!("cargo:rerun-if-changed=opendice/dice/include/dice/android.h");
    println!("cargo:rerun-if-changed=opendice/dice/include/dice/tee_dice.h");

    for file in src_files {
        println!("cargo:rerun-if-changed={}", file);
    }

    // 5. 告诉 cargo 链接 mbedtls 静态库
    // let dep_root = std::env::var("DEP_MBEDTLS_ROOT")
    //     .expect("DEP_MBEDTLS_ROOT not found");
    // let lib_dir = format!("{}/build/library", dep_root); // 库路径
    // // let include_dir = format!("{}/include", dep_root);   // 如果有暴露的 include 路径，可用此替换相对路径
    //
    // println!("cargo:rustc-link-search=native={}", lib_dir);
    // println!("cargo:rustc-link-lib=static=mbedtls");     // 链接 libmbedtls.a
}
