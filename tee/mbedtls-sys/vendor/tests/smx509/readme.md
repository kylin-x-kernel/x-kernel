# Makefile说明
- 版本说明  
操作系统OS: Kylin V10 SP1  
内核Kernel: Linux greatwall-pc 5.4.18-125-generic #114gwtest10u SMP Tue Oct 15 05:05:43 CEST 2024 aarch64 aarch64 aarch64 GNU/Linux  
Openssl版本：OpenSSL 1.1.1f  31 Mar 2020  

- 前置条件：确保项目make通过且make test成功。具体步骤如下：  
```shell
    cd vendor
    mkdir build && cd build
    cmake ..
    make -j8
    make test
```

- 功能：使用国密算法SM2和SM3生成一个x509证书链，包括:  
CA根密钥 ca_key.pem  
CA自签名证书 ca_cert.pem  
服务器密钥 server_key.pem  
服务器请求 server_csr.pem  
服务器证书 server_cert.pem  
客户端密钥 client_key.pem  
客户端请求 client_csr.pem  
客户端证书 client_cert.pem   
server和client的证书由CA进行签名和颁发  
test文件夹下的文件由make生成，可直接执行make check以验证证书正确性

- 运行：  
```shell
    make    #生成x509证书链
    make check  #验证证书正确性
    make clean  #清理证书文件
```

- 其他测试命令
```shell
    #查看服务器证书
    openssl x509 -in test/server_cert.pem -text -noout
    #查看客户端证书
    openssl x509 -in test/client_cert.pem -text -noout
    #验证服务器证书
    openssl verify -CAfile ca_cert.pem server_cert.pem
    #验证客户端证书
    openssl verify -CAfile ca_cert.pem client_cert.pem
```