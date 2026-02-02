# SMBEDTLS使用说明

## 主机环境配置
1. 操作系统OS: Kylin V10 SP1
2. 内核Kernel: Linux greatwall-pc 5.4.18-125-generic#114gwtest10u SMP Tue Oct 15 05:05:43 CEST 2024 aarch64 aarch64 aarch64 GNU/Linux
3. Openssl版本：OpenSSL 1.1.1f  31 Mar 2020
4. rustc: rustc 1.88.0 (6b00bc388 2025-06-23)
5. Cargo: cargo 1.88.0 (873a06493 2025-05-10)
6. rustup: rustup 1.28.2 (e4f3ad6f8 2025-04-28)

## 环境构建
1. mbedtls源码编译
```shell
    cd ../mbedtls-sys/vendor
    mkdir build && cd build
    cmake ..
    make -j8
    make test
    sudo make install
```
2. rust-mbedtls库封装
```shell
    cd ..
    cargo build
    cargo test
```

## rust-mbedtls方法使用说明
在正确封装后，crate为mbedtls_sys，根crate为mbedtls-sys-auto  
在开始使用前，在想要使用mbedtls的rust项目Cargo.toml中添加依赖项：
```rust
    [dependencies]
    mbedtls-sys-auto = { path = "xxx" }
```
xxx为mbedtls-sys的目录地址，如xxx/xxx/.../rust-mbedtls/mbedtls-sys  
或者使用cargo add进行添加

在rs文件中，一个调用mbedtls中方法的示例(部分)如下：
```rust
    extern crate mbedtls_sys as mbedtls;
    use mbedtls::types::raw_types::c_uchar;
    use mbedtls::sm3_ret;

    fn main() {
        // 输入十六进制字符串
        let input_hex = "616263";
        println!("输入十六进制: {}", input_hex);

        // 将输入十六进制字符串转换为字节
        let input_bytes = match hex_to_bytes(input_hex) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("输入错误: {}", e);
                return;
            }
        };

        let mut output_buffer = [0u8; 32];
        let output_ptr: *mut c_uchar = output_buffer.as_mut_ptr();
        let input_ptr: *const c_uchar = input_bytes.as_ptr();

        // 调用SM3函数
        let result = unsafe {
            sm3_ret(input_ptr, input_bytes.len(), output_ptr)
        };
    }
```
## rust-mbedtls国密算法接口
### SM2加解密
1. sm2_genkey()
```rust
    ///生成 SM2 密钥对，基于椭圆曲线参数gid(如SM2P256R1)
    pub fn sm2_genkey(
        ctx: *mut sm2_context,
        gid: ecp_group_id,
        f_rng: ::core::option::Option<
            unsafe extern "C" fn(
                arg1: *mut ::types::raw_types::c_void,
                arg2: *mut ::types::raw_types::c_uchar,
                arg3: usize,
            ) -> ::types::raw_types::c_int,
        >,
        p_rng: *mut ::types::raw_types::c_void,
    ) -> ::types::raw_types::c_int;
```
2. sm2_from_keypair()
```rust
    ///从现有的 ECC 密钥对初始化 SM2 上下文
    pub fn sm2_from_keypair(ctx: *mut sm2_context, key: *const ecp_keypair) -> ::types::raw_types::c_int;
```
3. sm2_init()
```rust
    ///初始化SM2上下文资源
    pub fn sm2_init(ctx: *mut sm2_context);
```
4. sm2_free()
```rust
    ///释放SM2上下文资源
    pub fn sm2_free(ctx: *mut sm2_context);
```
5. sm2_encrypt_raw()
```rust
    ///输出原始格式密文：04 || C1.x || C1.y || C3 || C2
    pub fn sm2_encrypt_raw(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        output: *mut ::types::raw_types::c_uchar,
        olen: *mut usize,
        f_rng: ::core::option::Option<
            unsafe extern "C" fn(
                arg1: *mut ::types::raw_types::c_void,
                arg2: *mut ::types::raw_types::c_uchar,
                arg3: usize,
            ) -> ::types::raw_types::c_int,
        >,
        p_rng: *mut ::types::raw_types::c_void,
    ) -> ::types::raw_types::c_int;
```
6. sm2_encrypt_asn1()
```rust
    ///输出ASN.1格式密文(序列化结构)
    pub fn sm2_encrypt_asn1(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        output: *mut ::types::raw_types::c_uchar,
        olen: *mut usize,
        f_rng: ::core::option::Option<
            unsafe extern "C" fn(
                arg1: *mut ::types::raw_types::c_void,
                arg2: *mut ::types::raw_types::c_uchar,
                arg3: usize,
            ) -> ::types::raw_types::c_int,
        >,
        p_rng: *mut ::types::raw_types::c_void,
    ) -> ::types::raw_types::c_int;
```
7. sm2_decrypt_raw()
```rust
    ///解密原始格式密文
    pub fn sm2_decrypt_raw(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        output: *mut ::types::raw_types::c_uchar,
        olen: *mut usize,
    ) -> ::types::raw_types::c_int;
```
8. sm2_decrypt_asn1()
```rust
    ///解析并解密ASN.1格式密文
    pub fn sm2_decrypt_asn1(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        output: *mut ::types::raw_types::c_uchar,
        olen: *mut usize,
    ) -> ::types::raw_types::c_int;
```
### SM2签名
1. sm2_sign_raw()
```rust
    ///输出原始格式签名:r || s
    pub fn sm2_sign_raw(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        hash: *const ::types::raw_types::c_uchar,
        sig: *mut ::types::raw_types::c_uchar,
        slen: *mut usize,
        f_rng: ::core::option::Option<
            unsafe extern "C" fn(
                arg1: *mut ::types::raw_types::c_void,
                arg2: *mut ::types::raw_types::c_uchar,
                arg3: usize,
            ) -> ::types::raw_types::c_int,
        >,
        p_rng: *mut ::types::raw_types::c_void,
    ) -> ::types::raw_types::c_int;
```
2. sm2_sign_asn1()
```rust
    ///输出ASN.1格式签名(SEQUENCE包含r和s)
    pub fn sm2_sign_asn1(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        hash: *const ::types::raw_types::c_uchar,
        sig: *mut ::types::raw_types::c_uchar,
        slen: *mut usize,
        f_rng: ::core::option::Option<
            unsafe extern "C" fn(
                arg1: *mut ::types::raw_types::c_void,
                arg2: *mut ::types::raw_types::c_uchar,
                arg3: usize,
            ) -> ::types::raw_types::c_int,
        >,
        p_rng: *mut ::types::raw_types::c_void,
    ) -> ::types::raw_types::c_int;
```
3. sm2_verify_raw()
```rust
    ///验证原始格式签名
    pub fn sm2_verify_raw(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        hash: *const ::types::raw_types::c_uchar,
        hlen: usize,
        sig: *const ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
4. sm2_verify_asn1()
```rust
    ///解析并验证ASN.1格式签名
    pub fn sm2_verify_asn1(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        hash: *const ::types::raw_types::c_uchar,
        hlen: usize,
        sig: *const ::types::raw_types::c_uchar,
        slen: usize,
    ) -> ::types::raw_types::c_int;
```
5. sm2_hash_z()
```rust
    ///计算用户标识哈希Z
    pub fn sm2_hash_z(
        ctx: *mut sm2_context,
        md_alg: md_type_t,
        id: *const ::types::raw_types::c_char,
        idlen: usize,
        z: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
6. sm2_hash_e()
```rust
    ///计算消息摘要E
    pub fn sm2_hash_e(
        md_alg: md_type_t,
        z: *const ::types::raw_types::c_uchar,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        e: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
7. md_sm2()
```rust
    ///组合调用上述函数，生成完整的SM2签名所需哈希值
    pub fn md_sm2(
        key_ctx: *mut sm2_context,
        md_alg: md_type_t,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
### SM3散列
1. sm3_init()
```rust
    ///初始化SM3上下文结构体
    pub fn sm3_init(ctx: *mut sm3_context);
```
2. sm3_free()
```rust
    ///安全清除上下文中的敏感数据
    pub fn sm3_free(ctx: *mut sm3_context);
```
3. sm3_clone()
```rust
    ///复制SM3上下文状态
    pub fn sm3_clone(dst: *mut sm3_context, src: *const sm3_context);
```
4. sm3_starts_ret()
```rust
    ///初始化哈希计算
    pub fn sm3_starts_ret(ctx: *mut sm3_context) -> ::types::raw_types::c_int;
```
5. sm3_update_ret()
```rust
    ///处理输入数据流
    pub fn sm3_update_ret(
        ctx: *mut sm3_context,
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
    ) -> ::types::raw_types::c_int;
```
6. sm3_finish_ret()
```rust
    ///生成最终哈希值
    pub fn sm3_finish_ret(ctx: *mut sm3_context, output: *mut ::types::raw_types::c_uchar) -> ::types::raw_types::c_int;
```
7. sm3_ret()
```rust
    ///一站式计算SM3哈希值
    ///input：输入数据
    ///ilen：输入长度
    ///output：输出缓冲区(32 字节)
    pub fn sm3_ret(
        input: *const ::types::raw_types::c_uchar,
        ilen: usize,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
### SM4加解密
1. sm4_init()
```rust
    ///初始化SM4上下文结构体
    pub fn sm4_init(ctx: *mut sm4_context);
```
2. sm4_free()
```rust
    ///安全清除上下文中的敏感数据
    pub fn sm4_free(ctx: *mut sm4_context);
```
3. sm4_setkey_enc()
```rust
    ///生成加密轮密钥
    pub fn sm4_setkey_enc(
        ctx: *mut sm4_context,
        key: *const ::types::raw_types::c_uchar,
        keybits: ::types::raw_types::c_uint,
    ) -> ::types::raw_types::c_int;
```
4. sm4_setkey_dec()
```rust
    ///生成解密轮密钥
    pub fn sm4_setkey_dec(
        ctx: *mut sm4_context,
        key: *const ::types::raw_types::c_uchar,
        keybits: ::types::raw_types::c_uint,
    ) -> ::types::raw_types::c_int;
```
5. sm4_crypt_ecb()
```rust
    ///ECB模式加解密(128位分组)
    pub fn sm4_crypt_ecb(
        ctx: *mut sm4_context,
        mode: ::types::raw_types::c_int,
        input: *const ::types::raw_types::c_uchar,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
6. sm4_crypt_cbc()
```rust
    ///CBC模式加解密(128位分组)
    pub fn sm4_crypt_cbc(
        ctx: *mut sm4_context,
        mode: ::types::raw_types::c_int,
        length: usize,
        iv: *mut ::types::raw_types::c_uchar,
        input: *const ::types::raw_types::c_uchar,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
7. sm4_crypt_ctr()
```rust
    ///CTR模式加解密(128位分组)
    pub fn sm4_crypt_ctr(
        ctx: *mut sm4_context,
        length: usize,
        nc_off: *mut usize,
        nonce_counter: *mut ::types::raw_types::c_uchar,
        stream_block: *mut ::types::raw_types::c_uchar,
        input: *const ::types::raw_types::c_uchar,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```

### GCM模式
1. gcm_init()
```rust
    ///初始化 GCM 上下文结构体，将所有字段置零
    pub fn gcm_init(ctx: *mut gcm_context);
```
2. gcm_free()
```rust
    ///释放 GCM 上下文资源，包括底层密码上下文，并安全清零敏感数据
    pub fn gcm_free(ctx: *mut gcm_context);
```
3. gcm_setkey()
```rust
    ///设置 GCM 加密密钥
    pub fn gcm_setkey(
        ctx: *mut gcm_context,
        cipher: cipher_id_t,
        key: *const ::types::raw_types::c_uchar,
        keybits: ::types::raw_types::c_uint,
    ) -> ::types::raw_types::c_int;
```
4. gcm_starts()
```rust
    ///初始化 GCM 加密/解密操作
    pub fn gcm_starts(
        ctx: *mut gcm_context,
        mode: ::types::raw_types::c_int,
        iv: *const ::types::raw_types::c_uchar,
        iv_len: usize,
        add: *const ::types::raw_types::c_uchar,
        add_len: usize,
    ) -> ::types::raw_types::c_int;
```
5. gcm_update()
```rust
    ///处理加密/解密数据流
    pub fn gcm_update(
        ctx: *mut gcm_context,
        length: usize,
        input: *const ::types::raw_types::c_uchar,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
6. gcm_finish()
```rust
    ///生成认证标签
    pub fn gcm_finish(
        ctx: *mut gcm_context,
        tag: *mut ::types::raw_types::c_uchar,
        tag_len: usize,
    ) -> ::types::raw_types::c_int;
```
7. gcm_crypt_and_tag()
```rust
    ///单次调用完成加密+标签生成
    pub fn gcm_crypt_and_tag(
        ctx: *mut gcm_context,
        mode: ::types::raw_types::c_int,
        length: usize,
        iv: *const ::types::raw_types::c_uchar,
        iv_len: usize,
        add: *const ::types::raw_types::c_uchar,
        add_len: usize,
        input: *const ::types::raw_types::c_uchar,
        output: *mut ::types::raw_types::c_uchar,
        tag_len: usize,
        tag: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```
8. gcm_auth_decrypt()
```rust
    ///验证 GCM 实现的正确性
    pub fn gcm_auth_decrypt(
        ctx: *mut gcm_context,
        length: usize,
        iv: *const ::types::raw_types::c_uchar,
        iv_len: usize,
        add: *const ::types::raw_types::c_uchar,
        add_len: usize,
        tag: *const ::types::raw_types::c_uchar,
        tag_len: usize,
        input: *const ::types::raw_types::c_uchar,
        output: *mut ::types::raw_types::c_uchar,
    ) -> ::types::raw_types::c_int;
```