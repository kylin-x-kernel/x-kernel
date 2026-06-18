// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Streaming symmetric cipher context.
//!
//! Wraps tee_crypto's one-shot cipher functions with a buffering layer that
//! supports incremental `update()` / `final()` calls, matching mbedtls's
//! streaming Cipher API. AEAD algorithms use the same context facade for
//! compatibility with the TEE operation layer.

mod algo;
mod context;
pub(super) mod mode;
pub(crate) mod padding;

pub use algo::{
    AlgorithmFamily, AlgorithmMode, AlgorithmSpec, Direction, PaddingMode, StreamingCipherAlgo,
};
pub use context::StreamingCipherCtx;
