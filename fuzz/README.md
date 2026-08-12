# x-kernel WFUZZ harness

Fuzz target for [`drivers/rs-fdtree`](../drivers/rs-fdtree): random bytes → `LinuxFdt::new()` → traverse nodes/properties.

## Layout

```
fuzz/
├── wfuzz.json                 # WFUZZ project config
├── fuzz_build.sh              # local build helper (nightly toolchain)
├── extract_entrypoints_list.py
├── Cargo.toml
├── fuzz_targets/fdt_parse.rs
└── .cargo/config.toml         # Kylin mirror (container)
```

Build artifacts (`entrypoints.json`, `entrypoints_list.txt`, `wfuzz-fuzz-targets/`, `wfuzz-test-*/`) are generated under `fuzz/` when running `./fuzz_build.sh` and are listed in the repo `.gitignore`.

## Container workflow (v11-2603)

```bash
docker run -it --name fuzz-xkernel -v /path/to/x-kernel:/code v11-2603:latest bash

# once as root
chown -R ubuntu:ubuntu /code

su ubuntu
cd /code/fuzz
source ~/.bashrc   # wfuzz in PATH after SDK install

./fuzz_build.sh

WFUZZ_TEST_ENTRYPOINT=fdt_parse wfuzz fuzz
```

`fuzz_build.sh` uses `RUSTUP_TOOLCHAIN=nightly` because the Kylin mirror may not ship the pinned `rust-toolchain.toml` version. The crates.io mirror is configured only in `fuzz/.cargo/config.toml` and does not affect root `make build`.

## Entrypoint

| Name | Harness | Crate under test |
|------|---------|------------------|
| `fdt_parse` | `fuzz/fuzz_targets/fdt_parse.rs` | `rs_fdtree` |
