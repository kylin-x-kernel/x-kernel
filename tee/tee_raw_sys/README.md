# tee_raw_sys
This is a tee_raw_sys refer to optee_utee_sys  and linux_raw_sys

`utee_attribute` is a 24-byte user ABI record. Its explicit `abi_pad` field
occupies the four tail bytes and is initialized by `Default`, allowing test
fixtures to copy the complete record to user memory under `UserWrite`.
`utee_object_info` contains only integer fields and supports both user read
and write contracts.
