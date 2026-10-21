# TA Context Tests

`ta_ctx/tests_ta_ctx.rs` is mounted at the original `ta_ctx::tests_ta_ctx`
scope with `unittest::path_mod_test!`. These are guest kernel tests, enabled
by `UNITTEST=y`, not host Cargo tests. They cover TA identity/path checks and
TA header byte conversion without exposing private helpers in the library API.
