// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `FUTEX_WAKE_OP` ABI decoding and arithmetic.

use kerrno::{KError, KResult};
use linux_raw_sys::general::{
    FUTEX_OP_ADD, FUTEX_OP_ANDN, FUTEX_OP_CMP_EQ, FUTEX_OP_CMP_GE, FUTEX_OP_CMP_GT,
    FUTEX_OP_CMP_LE, FUTEX_OP_CMP_LT, FUTEX_OP_CMP_NE, FUTEX_OP_OPARG_SHIFT, FUTEX_OP_OR,
    FUTEX_OP_SET, FUTEX_OP_XOR,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Set,
    Add,
    Or,
    AndNot,
    Xor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

/// Decoded operation carried by the sixth argument of `FUTEX_WAKE_OP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FutexWakeOp {
    operation: Operation,
    operation_argument: i32,
    comparison: Comparison,
    comparison_argument: i32,
}

impl FutexWakeOp {
    /// Decodes Linux's `FUTEX_OP()` bit layout.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` (`InvalidInput`) when the shift form is used with an
    /// operation argument outside `0..=31`, or `ENOSYS` (`Unsupported`) when
    /// the operation or comparison opcode is not one of the defined values.
    pub fn decode(encoded: u32) -> KResult<Self> {
        let operation_raw = (encoded >> 28) & 0x7;
        let comparison_raw = (encoded >> 24) & 0xf;
        let mut operation_argument = sign_extend_12((encoded >> 12) & 0xfff);
        let comparison_argument = sign_extend_12(encoded & 0xfff);

        if encoded & (FUTEX_OP_OPARG_SHIFT << 28) != 0 {
            if !(0..=31).contains(&operation_argument) {
                return Err(KError::InvalidInput);
            }
            operation_argument = 1i32.wrapping_shl(operation_argument as u32);
        }

        let operation = match operation_raw {
            value if value == FUTEX_OP_SET => Operation::Set,
            value if value == FUTEX_OP_ADD => Operation::Add,
            value if value == FUTEX_OP_OR => Operation::Or,
            value if value == FUTEX_OP_ANDN => Operation::AndNot,
            value if value == FUTEX_OP_XOR => Operation::Xor,
            _ => return Err(KError::Unsupported),
        };
        let comparison = match comparison_raw {
            value if value == FUTEX_OP_CMP_EQ => Comparison::Equal,
            value if value == FUTEX_OP_CMP_NE => Comparison::NotEqual,
            value if value == FUTEX_OP_CMP_LT => Comparison::Less,
            value if value == FUTEX_OP_CMP_LE => Comparison::LessOrEqual,
            value if value == FUTEX_OP_CMP_GT => Comparison::Greater,
            value if value == FUTEX_OP_CMP_GE => Comparison::GreaterOrEqual,
            _ => return Err(KError::Unsupported),
        };

        Ok(Self {
            operation,
            operation_argument,
            comparison,
            comparison_argument,
        })
    }

    pub(crate) fn apply(self, old: u32) -> u32 {
        let old = old as i32;
        (match self.operation {
            Operation::Set => self.operation_argument,
            Operation::Add => old.wrapping_add(self.operation_argument),
            Operation::Or => old | self.operation_argument,
            Operation::AndNot => old & !self.operation_argument,
            Operation::Xor => old ^ self.operation_argument,
        }) as u32
    }

    pub(crate) fn compare(self, old: u32) -> bool {
        let old = old as i32;
        match self.comparison {
            Comparison::Equal => old == self.comparison_argument,
            Comparison::NotEqual => old != self.comparison_argument,
            Comparison::Less => old < self.comparison_argument,
            Comparison::LessOrEqual => old <= self.comparison_argument,
            Comparison::Greater => old > self.comparison_argument,
            Comparison::GreaterOrEqual => old >= self.comparison_argument,
        }
    }
}

fn sign_extend_12(value: u32) -> i32 {
    ((value << 20) as i32) >> 20
}

unittest::path_mod_test!(tests, "../ut-tests/wake_op.rs");
