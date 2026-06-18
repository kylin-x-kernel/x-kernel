// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Algorithm metadata for streaming cipher dispatch.

/// Supported symmetric cipher algorithms + modes, including AEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingCipherAlgo {
    Aes128Ecb,
    Aes256Ecb,
    Aes128Cbc,
    Aes256Cbc,
    Aes128Ctr,
    Aes256Ctr,
    Sm4Ecb,
    Sm4Cbc,
    Sm4Ctr,
    Des3Ecb,
    Des3Cbc,
    DesEcb,
    DesCbc,
    Aes128Gcm,
    Aes192Gcm,
    Aes256Gcm,
    Sm4Gcm,
    Aes128Ccm,
    Aes256Ccm,
}

/// Padding mode for block ciphers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaddingMode {
    #[default]
    None,
    Pkcs7,
}

/// Cipher operation direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Encrypt,
    Decrypt,
}

impl Direction {
    pub const fn is_encrypting(self) -> bool {
        matches!(self, Self::Encrypt)
    }

    pub const fn is_decrypting(self) -> bool {
        matches!(self, Self::Decrypt)
    }
}

/// Algorithm family used to select backend primitives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlgorithmFamily {
    Aes128,
    Aes192,
    Aes256,
    Sm4,
    Des,
    Des3,
}

/// Cipher mode used by the streaming context state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlgorithmMode {
    Ecb,
    Cbc,
    Ctr,
    Gcm,
    Ccm,
}

/// Static metadata for a streaming cipher algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlgorithmSpec {
    pub family: AlgorithmFamily,
    pub mode: AlgorithmMode,
    pub block_size: usize,
    pub key_size: usize,
    pub iv_size: Option<usize>,
    pub tag_size: Option<usize>,
}

impl AlgorithmSpec {
    pub const fn is_ctr(self) -> bool {
        matches!(self.mode, AlgorithmMode::Ctr)
    }

    pub const fn is_ecb(self) -> bool {
        matches!(self.mode, AlgorithmMode::Ecb)
    }

    pub const fn is_aead(self) -> bool {
        matches!(self.mode, AlgorithmMode::Gcm | AlgorithmMode::Ccm)
    }

    pub const fn is_gcm(self) -> bool {
        matches!(self.mode, AlgorithmMode::Gcm)
    }

    pub const fn is_ccm(self) -> bool {
        matches!(self.mode, AlgorithmMode::Ccm)
    }
}

impl StreamingCipherAlgo {
    pub const fn spec(self) -> AlgorithmSpec {
        use AlgorithmFamily::*;
        use AlgorithmMode::*;

        match self {
            Self::Aes128Ecb => AlgorithmSpec {
                family: Aes128,
                mode: Ecb,
                block_size: 16,
                key_size: 16,
                iv_size: None,
                tag_size: None,
            },
            Self::Aes256Ecb => AlgorithmSpec {
                family: Aes256,
                mode: Ecb,
                block_size: 16,
                key_size: 32,
                iv_size: None,
                tag_size: None,
            },
            Self::Aes128Cbc => AlgorithmSpec {
                family: Aes128,
                mode: Cbc,
                block_size: 16,
                key_size: 16,
                iv_size: Some(16),
                tag_size: None,
            },
            Self::Aes256Cbc => AlgorithmSpec {
                family: Aes256,
                mode: Cbc,
                block_size: 16,
                key_size: 32,
                iv_size: Some(16),
                tag_size: None,
            },
            Self::Aes128Ctr => AlgorithmSpec {
                family: Aes128,
                mode: Ctr,
                block_size: 16,
                key_size: 16,
                iv_size: Some(16),
                tag_size: None,
            },
            Self::Aes256Ctr => AlgorithmSpec {
                family: Aes256,
                mode: Ctr,
                block_size: 16,
                key_size: 32,
                iv_size: Some(16),
                tag_size: None,
            },
            Self::Sm4Ecb => AlgorithmSpec {
                family: Sm4,
                mode: Ecb,
                block_size: 16,
                key_size: 16,
                iv_size: None,
                tag_size: None,
            },
            Self::Sm4Cbc => AlgorithmSpec {
                family: Sm4,
                mode: Cbc,
                block_size: 16,
                key_size: 16,
                iv_size: Some(16),
                tag_size: None,
            },
            Self::Sm4Ctr => AlgorithmSpec {
                family: Sm4,
                mode: Ctr,
                block_size: 16,
                key_size: 16,
                iv_size: Some(16),
                tag_size: None,
            },
            Self::Des3Ecb => AlgorithmSpec {
                family: Des3,
                mode: Ecb,
                block_size: 8,
                key_size: 24,
                iv_size: None,
                tag_size: None,
            },
            Self::Des3Cbc => AlgorithmSpec {
                family: Des3,
                mode: Cbc,
                block_size: 8,
                key_size: 24,
                iv_size: Some(8),
                tag_size: None,
            },
            Self::DesEcb => AlgorithmSpec {
                family: Des,
                mode: Ecb,
                block_size: 8,
                key_size: 8,
                iv_size: None,
                tag_size: None,
            },
            Self::DesCbc => AlgorithmSpec {
                family: Des,
                mode: Cbc,
                block_size: 8,
                key_size: 8,
                iv_size: Some(8),
                tag_size: None,
            },
            Self::Aes128Gcm => AlgorithmSpec {
                family: Aes128,
                mode: Gcm,
                block_size: 16,
                key_size: 16,
                iv_size: None,
                tag_size: Some(16),
            },
            Self::Aes192Gcm => AlgorithmSpec {
                family: Aes192,
                mode: Gcm,
                block_size: 16,
                key_size: 24,
                iv_size: None,
                tag_size: Some(16),
            },
            Self::Aes256Gcm => AlgorithmSpec {
                family: Aes256,
                mode: Gcm,
                block_size: 16,
                key_size: 32,
                iv_size: None,
                tag_size: Some(16),
            },
            Self::Sm4Gcm => AlgorithmSpec {
                family: Sm4,
                mode: Gcm,
                block_size: 16,
                key_size: 16,
                iv_size: None,
                tag_size: Some(16),
            },
            Self::Aes128Ccm => AlgorithmSpec {
                family: Aes128,
                mode: Ccm,
                block_size: 16,
                key_size: 16,
                iv_size: None,
                tag_size: Some(16),
            },
            Self::Aes256Ccm => AlgorithmSpec {
                family: Aes256,
                mode: Ccm,
                block_size: 16,
                key_size: 32,
                iv_size: None,
                tag_size: Some(16),
            },
        }
    }

    pub const fn block_size(self) -> usize {
        self.spec().block_size
    }

    pub const fn key_size(self) -> usize {
        self.spec().key_size
    }

    pub const fn is_ctr(self) -> bool {
        self.spec().is_ctr()
    }

    pub const fn is_ecb(self) -> bool {
        self.spec().is_ecb()
    }

    pub const fn is_aead(self) -> bool {
        self.spec().is_aead()
    }

    pub const fn is_gcm(self) -> bool {
        self.spec().is_gcm()
    }

    pub const fn is_ccm(self) -> bool {
        self.spec().is_ccm()
    }
}
