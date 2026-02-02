# ruyi 下使用rust-mbedtls
 
## 引入 rust-mbedtls

1. checkout 到workspace

2. 修改 workspace 的 Cargo.toml

workspace/Cargo.toml

```toml
[patch.crates-io]
mbedtls = { path = "./rust-mbedtls/mbedtls" }
```
3. crate 引入

手动指定 features

```toml
mbedtls = { version = "0.13.3", default-features = false, features = ["no_std_deps"],  optional = true}
```

5. 将 tools/aarch64-linux-musl-ar 拷贝到项目的 musl-gcc 同目录，并更改为 musl-ar

参考 workspace 的 makefile:

```Makefile
MUSL_AR_FILE_DST := $(CURDIR)/../musl/build/aarch64/release/bin/musl-ar
MUSL_AR_FILE_SRC := $(CURDIR)/rust-mbedtls/tools/aarch64-linux-musl-ar

musl-ar:
	@if [ ! -f $(MUSL_AR_FILE_DST) ]; then \
		echo "$(MUSL_AR_FILE_DST) not found, copying from $(MUSL_AR_FILE_SRC)..."; \
		cp $(MUSL_AR_FILE_SRC) $(MUSL_AR_FILE_DST); \
	else \
		echo "$(MUSL_AR_FILE_DST) already exists."; \
	fi
```

6. 编译时指定 CC:

```makefile
CC="$(CURDIR)/../musl/build/aarch64/release/bin/musl-gcc" cargo build --no-default-features  --features "ruyi" --target=aarch64-unknown-none
```

