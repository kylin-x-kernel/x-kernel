//! Initialize the user stack for the application
//!
//! The structure of the user stack is described in the following figure:
//! position            content                     size (bytes) + comment
//!   ------------------------------------------------------------------------
//! stack pointer ->  [ argc = number of args ]     8
//!                   [ argv[0] (pointer) ]         8   (program name)
//!                   [ argv[1] (pointer) ]         8
//!                   [ argv[..] (pointer) ]        8 * x
//!                   [ argv[n - 1] (pointer) ]     8
//!                   [ argv[n] (pointer) ]         8   (= NULL)
//!                   [ envp[0] (pointer) ]         8
//!                   [ envp[1] (pointer) ]         8
//!                   [ envp[..] (pointer) ]        8
//!                   [ envp[term] (pointer) ]      8   (= NULL)
//!                   [ auxv[0] (Elf32_auxv_t) ]    16
//!                   [ auxv[1] (Elf32_auxv_t) ]    16
//!                   [ auxv[..] (Elf32_auxv_t) ]   16
//!                   [ auxv[term] (Elf32_auxv_t) ] 16  (= AT_NULL vector)
//!                   [ padding ]                   0 - 16
//!                   [ argument ASCIIZ strings ]   >= 0
//!                   [ environment ASCIIZ str. ]   >= 0
//!
//! (0xbffffff8)      [ end marker ]                8   (= NULL)
//!
//! (0xc0000000)      < bottom of stack >           0   (virtual)
//!
//! More details can be found in the link: <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>

use alloc::{collections::VecDeque, string::String, vec::Vec};

use zerocopy::IntoBytes;

use crate::auxv::{AuxEntry, AuxType};

/// Generate initial stack frame for user stack
///
/// # Arguments
///
/// * `args` - Arguments of the application
/// * `envs` - Environment variables of the application
/// * `auxv` - Auxiliary vectors of the application
/// * `sp`   - Highest address of the stack
///
/// # Return
///
/// * [`Vec<u8>`] - Initial stack frame of the application
///
/// # Notes
///
/// The detailed format is described in <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>
pub fn app_stack_region(args: &[String], envs: &[String], auxv: &[AuxEntry], sp: usize) -> Vec<u8> {
    let mut data = VecDeque::new();
    let mut push = |src: &[u8]| -> usize {
        data.extend(src.iter().cloned());
        data.rotate_right(src.len());
        sp - data.len()
    };

    // define a random string with 16 bytes
    let random_str_pos = push("0123456789abcdef".as_bytes());
    // Push arguments and environment variables
    let envs_slice: Vec<_> = envs
        .iter()
        .map(|env| {
            push(b"\0");
            push(env.as_bytes())
        })
        .collect();
    let argv_slice: Vec<_> = args
        .iter()
        .map(|arg| {
            push(b"\0");
            push(arg.as_bytes())
        })
        .collect();
    let padding_null = "\0".repeat(8);
    let sp = push(padding_null.as_bytes());

    push(&b"\0".repeat(sp % 16));

    // Align stack to 16 bytes by padding if needed.
    // We will push following 8-byte items into stack:
    // - auxv (each entry is 2 * usize, so item count = auxv.len() * 2)
    // - envp (len + 1 for NULL terminator)
    // - argv (len + 1 for NULL terminator)
    // - argc (1 item)
    // Total items = auxv.len() * 2 + (envs.len() + 1) + (args.len() + 1) + 1
    //             = auxv.len() * 2 + envs.len() + args.len() + 3
    // If odd, the stack top will not be aligned to 16 bytes unless we add 8-byte
    // padding
    if (envs.len() + args.len() + 3) & 1 != 0 {
        push(padding_null.as_bytes());
    }

    // Push auxiliary vectors
    let mut has_random = false;
    let mut has_execfn = false;
    for entry in auxv.iter() {
        if entry.get_type() == AuxType::RANDOM {
            has_random = true;
        }
        if entry.get_type() == AuxType::EXECFN {
            has_execfn = true;
        }
        if has_random && has_execfn {
            break;
        }
    }
    push(auxv.as_bytes());
    if !has_random {
        push(AuxEntry::new(AuxType::RANDOM, random_str_pos).as_bytes());
    }
    if !has_execfn {
        push(AuxEntry::new(AuxType::EXECFN, argv_slice[0]).as_bytes());
    }

    // Push the argv and envp pointers
    push(padding_null.as_bytes());
    push(envs_slice.as_bytes());
    push(padding_null.as_bytes());
    push(argv_slice.as_bytes());
    // Push argc
    let sp = push(args.len().as_bytes());

    assert!(sp % 16 == 0);

    let mut result = Vec::with_capacity(data.len());
    let (first, second) = data.as_slices();
    result.extend_from_slice(first);
    result.extend_from_slice(second);
    result
}
