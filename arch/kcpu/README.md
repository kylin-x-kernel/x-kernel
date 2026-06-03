# kcpu

This crate provides privileged instruction and structure abstractions for various CPU architectures. It is designed to implement the hardware abstraction layer of an operating system kernel.

## Supported Architectures

* x86_64
* AArch64
* RISC-V
* LoongArch64

## x86_64 syscall context notes

The x86_64 trap frame stores `orig_rax` for contexts entered through the
`syscall` instruction. Non-syscall traps set this field to all bits set.

Signal and syscall-restart code must use this marker before rewinding the user
instruction pointer. A restartable syscall is re-entered by restoring `rax` from
`orig_rax` and subtracting the two-byte `syscall` instruction length from
`rip`. `restart_syscall` uses the same instruction-pointer rollback but replaces
`rax` with the restart syscall number instead of the original syscall number.
