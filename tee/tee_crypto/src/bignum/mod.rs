// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! BigNum implementation for arbitrary-precision integer arithmetic.
//!
//! The public types in this module are value-level helpers. They do not define
//! the GP TEE `TEE_BigInt` memory layout; `rust-libutee` owns that ABI boundary
//! and converts it to these types before doing arithmetic.
//!
//! `TeeBigNum` is an unsigned wrapper used by kernel crypto object code.
//! `TeeBigInt` is a signed facade used by the GP TEE arithmetic API. Signed
//! division returns a quotient truncated toward zero and a remainder with the
//! dividend sign; `modulo` normalizes the result to a non-negative residue.
//!
//! Several operations use `crypto-bigint` variable-time APIs. Callers should not
//! treat this module as a constant-time primitive for secret-dependent control
//! flow without auditing the exact operation being used.

mod signed;
mod unsigned;

pub use signed::{TeeBigInt, TeeBigIntSign};
pub use unsigned::{BigNum, TeeBigNum};
