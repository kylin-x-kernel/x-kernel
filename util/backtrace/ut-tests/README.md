# Kernel Unit Tests

`dwarf_unittest.rs` and `symtab_unittest.rs` are mounted by their matching
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.
They retain the `dwarf` and `symtab` feature gates of those source modules.
For guest validation, enable `KFEAT_DWARF` and `KFEAT_SYMTAB` respectively;
a crate filter alone does not enable these features or register their tests.
The host unwinder tests remain separately mounted from `tests/host/`.
