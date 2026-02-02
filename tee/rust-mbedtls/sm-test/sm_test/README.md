# Rust-SM-Test说明

## 文件
- main.rs  
主程序，包括测试用例
- sm2.rs  
SM2的rust版本使用示例，包括SM2加解密和SM2签名验签
- sm3.rs  
SM3的rust版本使用示例，包括SM3的散列生成
- sm4.rs  
SM4的rust版本使用示例，包括SM4的GCM、ECB、CBC、CTR模式

## 运行
```shell
    #清除bindings
    cargo clean
    #build生成bindings
    cargo build
    #运行main函数
    cargo run
    #运行测试
    cargo test
```
## 常见错误
- 无法链接某个函数  
mbedtls库未编译安装到本地，重新进行make install流程
- 加解密错误  
注意数据的输入输出格式，一般均为十六进制的字符形式，如"A56D8F",不区分大小写
- 找不到mbedtls_sys crate  
未在rust-mbedtls下执行cargo build
