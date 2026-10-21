// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};
use core::{fmt, fmt::Debug};

use bytemuck::{Pod, Zeroable, bytes_of, bytes_of_mut};
use memoffset::offset_of;
use subtle::ConstantTimeEq;
use tee_crypto::{
    aead::{Aead, Aes128GcmAead, Sm4GcmAead},
    hash::{Digest, Sha256, Sm3},
};
#[cfg(not(feature = "tee_ss_smx"))]
use tee_raw_sys::{TEE_ALG_AES_ECB_NOPAD, TEE_ALG_HMAC_SHA256};
use tee_raw_sys::{
    TEE_ALG_AES_GCM, TEE_ALG_SHA256, TEE_ALG_SM3, TEE_ALG_SM4_GCM, TEE_ERROR_BAD_PARAMETERS,
    TEE_ERROR_CORRUPT_OBJECT, TEE_ERROR_GENERIC, TEE_ERROR_MAC_INVALID, TEE_ERROR_NOT_IMPLEMENTED,
    TEE_ERROR_NOT_SUPPORTED, TEE_ERROR_SECURITY, TEE_ERROR_SHORT_BUFFER, TEE_OperationMode,
    TEE_UUID,
};

use super::utee_defines::TEE_SM3_HASH_SIZE;
#[cfg(not(feature = "tee_ss_smx"))]
use super::utee_defines::{TEE_AES_BLOCK_SIZE, TEE_SHA256_HASH_SIZE};
use crate::tee::{
    TeeResult,
    rng_software::crypto_rng_read,
    tee_fs_key_manager::{TEE_FS_KM_FEK_SIZE, tee_fs_fek_crypt},
    tee_ree_fs::{TeeFsFdAux, TeeFsHtreeStorageOps},
    utee_defines::TeeAlg,
    utils::slice_fmt,
};

pub(super) const TEE_FS_HTREE_IV_SIZE: usize = 16;
pub(super) const TEE_FS_HTREE_FEK_SIZE: usize = 16;
pub(super) const TEE_FS_HTREE_TAG_SIZE: usize = 16;

cfg_if::cfg_if! {
    if #[cfg(feature = "tee_ss_smx")] {
        pub(super) const TEE_FS_HTREE_HASH_ALG: TeeAlg = TEE_ALG_SM3;
        pub(super) const TEE_FS_HTREE_HASH_SIZE: usize = TEE_SM3_HASH_SIZE;
        pub(super) const TEE_FS_HTREE_AUTH_ENC_ALG: TeeAlg = TEE_ALG_SM4_GCM;
    } else {
        pub(super) const TEE_FS_HTREE_HASH_ALG: TeeAlg = TEE_ALG_SHA256;
        pub(super) const TEE_FS_HTREE_HASH_SIZE: usize = TEE_SHA256_HASH_SIZE;
        pub(super) const TEE_FS_HTREE_AUTH_ENC_ALG: TeeAlg = TEE_ALG_AES_GCM;
    }
}

pub(super) const HTREE_NODE_COMMITTED_BLOCK: u32 = 1 << 0; // 即 0x1

#[inline]
fn block_num_to_node_id(num: usize) -> usize {
    num + 1
}

#[allow(dead_code)]
#[inline]
fn node_id_to_block_num(id: usize) -> usize {
    id - 1
}

#[inline]
pub(super) const fn htree_node_committed_child(n: usize) -> u32 {
    1 << (1 + n)
}

#[repr(C)]
#[derive(Copy, Debug, Clone, Default, Pod, Zeroable)]
pub(super) struct TeeFsHtreeMeta {
    pub length: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default, Debug, Pod, Zeroable)]
pub(super) struct TeeFsHtreeImeta {
    pub meta: TeeFsHtreeMeta,
    pub max_node_id: u32,
    pub _padding: [u8; 4],
}

pub(super) const TEE_FS_HTREE_IMETA_SIZE: usize = core::mem::size_of::<TeeFsHtreeImeta>();
#[repr(C)]
#[derive(Copy, Clone, Default, Pod, Zeroable)]
pub(super) struct TeeFsHtreeImage {
    pub iv: [u8; TEE_FS_HTREE_IV_SIZE],
    pub tag: [u8; TEE_FS_HTREE_TAG_SIZE],
    pub enc_fek: [u8; TEE_FS_HTREE_FEK_SIZE],
    pub imeta: [u8; TEE_FS_HTREE_IMETA_SIZE],
    pub counter: u32,
}

impl Debug for TeeFsHtreeImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TeeFsHtreeImage {{ iv: {:?}, tag: {:?}, enc_fek: {:?}, imeta: {:?}, counter: {:?} }}",
            hex::encode(self.iv),
            hex::encode(self.tag),
            hex::encode(self.enc_fek),
            hex::encode(self.imeta),
            self.counter
        )
    }
}

#[repr(C)]
#[derive(Copy, Clone, Default, Pod, Zeroable)]
pub(super) struct TeeFsHtreeNodeImage {
    pub hash: [u8; TEE_FS_HTREE_HASH_SIZE],
    pub iv: [u8; TEE_FS_HTREE_IV_SIZE],
    pub tag: [u8; TEE_FS_HTREE_TAG_SIZE],
    pub flags: u16,
}

static_assertions::const_assert!(core::mem::size_of::<TeeFsHtreeNodeImage>() == 66);

impl TeeFsHtreeNodeImage {
    pub(super) fn as_bytes(&self) -> &[u8] {
        bytes_of(self)
    }

    pub(super) fn as_bytes_mut(&mut self) -> &mut [u8] {
        bytes_of_mut(self)
    }
}

impl Debug for TeeFsHtreeNodeImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TeeFsHtreeNodeImage {{ hash: {:?}, iv: {:?}, tag: {:?}, flags: {:X?} }}",
            hex::encode(self.hash),
            hex::encode(self.iv),
            hex::encode(self.tag),
            self.flags,
        )
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum TeeFsHtreeType {
    Head,
    Node,
    Block,
    #[allow(dead_code)]
    UnsupportedType,
}

#[repr(C)]
#[derive(Copy, Clone, Default, Debug)]
pub(super) struct TeeFsHtreeData {
    pub head: TeeFsHtreeImage,
    pub fek: [u8; TEE_FS_HTREE_FEK_SIZE],
    pub imeta: TeeFsHtreeImeta,
    pub uuid: TEE_UUID,
    pub dirty: bool,
    // const struct tee_fs_htree_storage *stor;
    // void *stor_aux;
}

#[derive(Debug, Default)]
pub(super) struct HtreeNode {
    pub id: usize,
    pub dirty: bool,
    pub block_updated: bool,
    pub node: TeeFsHtreeNodeImage,
    pub parent_id: Option<usize>,
    // left/right 使用 Box，因为：
    // 1. 子节点由父节点拥有
    // 2. 释放时通过 Box 自动管理
    // 3. 符合 Rust 的所有权模型
    pub left: Subtree,
    pub right: Subtree,
}

impl HtreeNode {
    pub(super) fn new(id: usize, node_image: TeeFsHtreeNodeImage) -> Self {
        HtreeNode {
            id,
            node: node_image,
            ..Default::default()
        }
    }

    pub(super) fn set_left(current_node: &mut HtreeNode, mut child: HtreeNode) {
        child.parent_id = Some(current_node.id);
        current_node.left = Some(Box::new(child));
    }

    pub(super) fn set_right(current_node: &mut HtreeNode, mut child: HtreeNode) {
        child.parent_id = Some(current_node.id);
        current_node.right = Some(Box::new(child));
    }

    /// 根据索引获取左右子树的引用。
    ///
    /// `index` 为 0 时返回左子树，为 1 时返回右子树。
    /// 如果对应子树不存在，则返回 `None`。
    pub(super) fn get_child_by_index(&mut self, index: usize) -> Option<&HtreeNode> {
        if index == 0 {
            self.left.as_ref().map(|b| b.as_ref())
        } else {
            self.right.as_ref().map(|b| b.as_ref())
        }
    }

    /// 根据索引获取左右子树的引用。
    ///
    /// `index` 为 0 时返回左子树，为 1 时返回右子树。
    /// 如果对应子树不存在，则返回 `None`。
    pub(super) fn get_child_by_index_mut(&mut self, index: usize) -> Option<&mut HtreeNode> {
        if index == 0 {
            self.left.as_mut().map(|b| b.as_mut())
        } else {
            self.right.as_mut().map(|b| b.as_mut())
        }
    }
}

pub(super) type Subtree = Option<Box<HtreeNode>>;

pub(super) trait SubtreeExt {
    fn get_mut(&mut self) -> Option<&mut HtreeNode>;

    fn get_ref(&self) -> Option<&HtreeNode>;
}

impl SubtreeExt for Subtree {
    fn get_mut(&mut self) -> Option<&mut HtreeNode> {
        self.as_deref_mut()
    }

    fn get_ref(&self) -> Option<&HtreeNode> {
        self.as_deref()
    }
}

// #[derive(Debug)]
pub(super) struct TeeFsHtree {
    pub root: HtreeNode,
    pub data: TeeFsHtreeData,
    pub storage: Box<dyn TeeFsHtreeStorageOps>,
}

impl Default for TeeFsHtree {
    fn default() -> Self {
        TeeFsHtree {
            root: HtreeNode::new(0, TeeFsHtreeNodeImage::default()),
            data: TeeFsHtreeData::default(),
            storage: Box::new(TeeFsFdAux::new()),
        }
    }
}

impl Debug for TeeFsHtree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TeeFsHtree {{ root: {:?}, data: {:?} }}",
            self.root, self.data
        )
    }
}

/// Hash context wrapping tee_crypto hash types.
pub(super) enum HashContext {
    Sm3(Sm3),
    Sha256(Sha256),
}

fn tee_alg_to_hash_ctx(alg: TeeAlg) -> TeeResult<HashContext> {
    match alg {
        TEE_ALG_SHA256 => Ok(HashContext::Sha256(Sha256::new())),
        TEE_ALG_SM3 => Ok(HashContext::Sm3(Sm3::new())),
        _ => Err(TEE_ERROR_NOT_IMPLEMENTED),
    }
}

fn crypto_hash_alloc_ctx(alg: TeeAlg) -> TeeResult<HashContext> {
    tee_alg_to_hash_ctx(alg)
}

fn crypto_hash_init(_md: &mut HashContext) -> TeeResult {
    // Hash context is initialized at creation time in tee_crypto
    Ok(())
}

fn crypto_hash_update(md: &mut HashContext, data: &[u8]) -> TeeResult {
    tee_debug!(
        "crypto_hash_update: data length: {:?}, data: {:X?}",
        data.len(),
        hex::encode(data)
    );
    match md {
        HashContext::Sm3(h) => h.update(data),
        HashContext::Sha256(h) => h.update(data),
    }
    Ok(())
}

fn crypto_hash_final(md: HashContext, digest: &mut [u8]) -> TeeResult {
    tee_debug!(
        "crypto_hash_final: digest length: {:?}, digest: {:X?}",
        digest.len(),
        hex::encode(&digest)
    );
    let result = match md {
        HashContext::Sm3(h) => h.finalize(),
        HashContext::Sha256(h) => h.finalize(),
    };
    digest.copy_from_slice(&result.as_bytes()[..digest.len()]);
    Ok(())
}

/// read the data from the storage
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `typ` - the type of the data
/// * `idx` - the index of the data
/// * `vers` - the version of the data
/// * `data` - the data to read
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn rpc_read(
    // fd: &mut FileVariant,
    storage: &dyn TeeFsHtreeStorageOps,
    typ: TeeFsHtreeType,
    idx: usize,
    vers: u8,
    data: &mut [u8],
) -> TeeResult {
    let dlen = data.len();
    if dlen == 0 {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    // rpc_read_init()?;
    storage.rpc_read_init()?;

    let result = storage.rpc_read_final(typ, idx, vers, data)?;
    if result != dlen {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    Ok(())
}

/// read the head from the storage
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `vers` - the version of the head
/// * `head` - the head of the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn rpc_read_head(
    storage: &dyn TeeFsHtreeStorageOps,
    // ht: &mut TeeFsHtree,
    vers: u8,
    head: &mut TeeFsHtreeImage,
) -> TeeResult {
    rpc_read(storage, TeeFsHtreeType::Head, 0, vers, bytes_of_mut(head))?;
    Ok(())
}

/// read the node from the storage
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `node_id` - the id of the node
/// * `vers` - the version of the node
/// * `head` - the head of the node
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn rpc_read_node(
    // fd: &mut FileVariant,
    // ht: &mut TeeFsHtree,
    // storage: &dyn TeeFsHtreeStorageOps,
    storage: &dyn TeeFsHtreeStorageOps,
    node_id: usize,
    vers: u8,
    head: &mut TeeFsHtreeNodeImage,
) -> TeeResult {
    tee_debug!("rpc_read_node: node_id: {:X?}, vers: {:X?}", node_id, vers);
    let data_ptr: &mut [u8] = head.as_bytes_mut();
    rpc_read(storage, TeeFsHtreeType::Node, node_id - 1, vers, data_ptr)?;
    Ok(())
}

/// write the data to the storage
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `typ` - the type of the data
/// * `idx` - the index of the data
/// * `vers` - the version of the data
/// * `data` - the data to write
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn rpc_write(
    // fd: &FileVariant,
    storage: &dyn TeeFsHtreeStorageOps,
    typ: TeeFsHtreeType,
    idx: usize,
    vers: u8,
    data: &[u8],
) -> TeeResult {
    let dlen = data.len();
    if dlen == 0 {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    storage.rpc_write_init()?;

    let _ = storage
        .rpc_write_final(typ, idx, vers, data)
        .inspect_err(|e| {
            error!("rpc_write_final: error: {:X?}", e);
        })?;

    Ok(())
}

/// write the head to the storage
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `vers` - the version of the head
/// * `head` - the head of the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn rpc_write_head(
    // fd: &FileVariant,
    // ht: &mut TeeFsHtree,
    storage: &dyn TeeFsHtreeStorageOps,
    vers: u8,
    head: &TeeFsHtreeImage,
) -> TeeResult {
    tee_debug!("rpc_write_head: vers: {}, counter: {}", vers, head.counter);
    rpc_write(storage, TeeFsHtreeType::Head, 0, vers, bytes_of(head))?;
    Ok(())
}

/// write the node to the storage
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `node_id` - the id of the node
/// * `vers` - the version of the node
/// * `head` - the head of the node
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn rpc_write_node(
    // fd: &FileVariant,
    // ht: &mut TeeFsHtree,
    storage: &dyn TeeFsHtreeStorageOps,
    node_id: usize,
    vers: u8,
    head: &TeeFsHtreeNodeImage,
) -> TeeResult {
    tee_debug!(
        "rpc_write_node: node_id: {:?}, vers: {:?}, head: {:?}",
        node_id,
        vers,
        head,
    );
    let data_ptr: &[u8] = head.as_bytes();
    rpc_write(storage, TeeFsHtreeType::Node, node_id - 1, vers, data_ptr)
        .inspect_err(|e| error!("rpc_write_node error! {:X?}", e))?;
    Ok(())
}

/// calc the hash of the node
///
/// # Arguments
/// * `node` - the node
/// * `ht_data` - the data of the tree
/// * `fd` - the file descriptor
/// # Returns
/// * `TeeResult` - the result of the operation
#[cfg(unittest)]
pub(super) fn calc_node(
    node: &mut HtreeNode,
    ht_data: &TeeFsHtreeData,
    _storage: &dyn TeeFsHtreeStorageOps,
    // _fd: Option<&mut FileVariant>,
) -> TeeResult {
    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];

    if node.parent_id.is_some() {
        calc_node_hash_with_type(TEE_FS_HTREE_HASH_ALG, node, None, &mut digest)?;
    } else {
        calc_node_hash_with_type(
            TEE_FS_HTREE_HASH_ALG,
            node,
            Some(&ht_data.imeta.meta),
            &mut digest,
        )?;
    }

    node.node.hash.copy_from_slice(&digest);

    Ok(())
}

/// calc the hash of the node with context
///
/// # Arguments
/// * `md` - the hash context
/// * `node` - the node
/// * `meta` - the meta of the tree
/// * `digest` - the digest of the hash
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn calc_node_hash_with_ctx(
    mut md: HashContext,
    node: &HtreeNode,
    meta: Option<&TeeFsHtreeMeta>,
    digest: &mut [u8; TEE_FS_HTREE_HASH_SIZE],
) -> TeeResult {
    // let all_bytes = bytemuck::bytes_of(&node.node);
    let all_bytes: &[u8] = node.node.as_bytes();
    debug_assert!(66 == all_bytes.len());
    let iv_offset = offset_of!(TeeFsHtreeNodeImage, iv);
    let flags_offset = offset_of!(TeeFsHtreeNodeImage, flags);
    let flags_size = core::mem::size_of::<u16>();

    tee_debug!(
        "all_bytes.len(): {:X?}, iv_offset: {:X?}, flags_offset: {:X?}, flags_size: {:X?}",
        all_bytes.len(),
        iv_offset,
        flags_offset,
        flags_size
    );

    tee_debug!(
        "calc_node_hash_with_ctx: node.node: {:?}, meta: {:?}",
        node.node,
        meta
    );

    crypto_hash_init(&mut md)?;
    crypto_hash_update(&mut md, &all_bytes[iv_offset..flags_offset + flags_size])?;

    if let Some(meta) = meta {
        crypto_hash_update(&mut md, bytemuck::bytes_of(meta))?;
    }

    if let Some(left) = node.left.get_ref() {
        crypto_hash_update(&mut md, &left.node.hash)?;
    }

    if let Some(right) = node.right.get_ref() {
        crypto_hash_update(&mut md, &right.node.hash)?;
    }
    crypto_hash_final(md, digest)?;

    Ok(())
}

/// calc the hash of the node with type
///
/// # Arguments
/// * `t` - the type of the hash
/// * `node` - the node
/// * `meta` - the meta of the tree
/// * `digest` - the digest of the hash
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn calc_node_hash_with_type(
    t: TeeAlg,
    node: &HtreeNode,
    meta: Option<&TeeFsHtreeMeta>,
    digest: &mut [u8; TEE_FS_HTREE_HASH_SIZE],
) -> TeeResult {
    let md = crypto_hash_alloc_ctx(t)?;
    calc_node_hash_with_ctx(md, node, meta, digest)?;

    tee_debug!("hash with {} {}", node.id, hex::encode(digest));
    Ok(())
}

/// calc the hash of the node
///
/// # Arguments
/// * `node` - the node
/// * `meta` - the meta of the tree
/// * `digest` - the digest of the hash
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn calc_node_hash(
    node: &HtreeNode,
    meta: &TeeFsHtreeMeta,
    digest: &mut [u8; TEE_FS_HTREE_HASH_SIZE],
) -> TeeResult {
    calc_node_hash_with_type(TEE_ALG_SHA256, node, Some(meta), digest)
}

/// traverse the tree post order
///
/// # Arguments
/// * `node` - the node
/// * `ht_data` - the data of the tree
/// * `visitor` - the visitor function
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn post_order_traverse<F>(
    node: &HtreeNode,
    ht_data: &TeeFsHtreeData,
    visitor: &mut F,
) -> TeeResult
where
    F: FnMut(&HtreeNode, &TeeFsHtreeData) -> TeeResult,
{
    // 对 fd 做借用变换

    // Traverse left subtree
    if let Some(left_child_arc) = node.left.get_ref() {
        post_order_traverse(left_child_arc, ht_data, visitor)?;
    }

    // Traverse right subtree
    if let Some(right_child_arc) = node.right.get_ref() {
        post_order_traverse(right_child_arc, ht_data, visitor)?;
    }

    // Visit the current node
    visitor(node, ht_data)?;

    Ok(())
}

/// traverse the tree post order mut
///
/// # Arguments
/// * `node` - the node
/// * `ht_data` - the data of the tree
/// * `visitor` - the visitor function
/// * `fd` - the file descriptor
/// # Returns
/// * `TeeResult` - the result of the operation
#[cfg(unittest)]
pub(super) fn post_order_traverse_mut<F>(
    node: &mut HtreeNode,
    ht_data: &TeeFsHtreeData,
    storage: &dyn TeeFsHtreeStorageOps,
    visitor: &mut F,
) -> TeeResult
where
    F: FnMut(
        &mut HtreeNode,
        &TeeFsHtreeData,
        &dyn TeeFsHtreeStorageOps,
        // Option<&mut FileVariant>,
    ) -> TeeResult,
{
    // Traverse the left subtree.
    if let Some(left_child_arc) = node.left.get_mut() {
        post_order_traverse_mut(left_child_arc, ht_data, storage, visitor)?;
    }

    // Traverse the right subtree.
    if let Some(right_child_arc) = node.right.get_mut() {
        post_order_traverse_mut(right_child_arc, ht_data, storage, visitor)?;
    }

    // Visit the current node; errors propagate to the caller.
    visitor(node, ht_data, storage)?;

    Ok(())
}

/// verify the node
///
/// # Arguments
/// * `node` - the node
/// * `ht_data` - the data of the tree
/// * `fd` - the file descriptor
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn verify_node(
    node: &HtreeNode,
    ht_data: &TeeFsHtreeData,
    // _fd: Option<&mut FileVariant>,
) -> TeeResult {
    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];

    if node.parent_id.is_some() {
        calc_node_hash_with_type(TEE_FS_HTREE_HASH_ALG, node, None, &mut digest)?;
    } else {
        calc_node_hash_with_type(
            TEE_FS_HTREE_HASH_ALG,
            node,
            Some(&ht_data.imeta.meta),
            &mut digest,
        )?;
    }

    tee_debug!(
        "check hash {} with {}",
        hex::encode(node.node.hash),
        hex::encode(digest)
    );

    if node.node.hash.ct_eq(&digest).unwrap_u8() == 0 {
        tee_debug!("verify_node: hash not equal");
        return Err(TEE_ERROR_CORRUPT_OBJECT);
    }

    Ok(())
}

/// print the hash of the node
///
/// # Arguments
/// * `node` - the node
/// * `ht_data` - the data of the tree
/// * `fd` - the file descriptor
/// # Returns
/// * `TeeResult` - the result of the operation
#[cfg(unittest)]
pub(super) fn print_node_hash(
    node: &HtreeNode,
    ht_data: &TeeFsHtreeData,
    // _fd: Option<&mut FileVariant>,
) -> TeeResult {
    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];

    if node.parent_id.is_some() {
        calc_node_hash_with_type(TEE_FS_HTREE_HASH_ALG, node, None, &mut digest)?;
    } else {
        calc_node_hash_with_type(
            TEE_FS_HTREE_HASH_ALG,
            node,
            Some(&ht_data.imeta.meta),
            &mut digest,
        )?;
    }

    tee_debug!("hash with {} {}", node.id, hex::encode(digest));
    Ok(())
}

/// sync the node to the storage
///
/// # Arguments
/// * `node` - the node
/// * `ht_data` - the data of the tree
/// * `fd` - the file descriptor
/// # Returns
/// * `TeeResult` - the result of the operation
fn write_current_node_to_storage(
    node: &mut HtreeNode,
    storage: &dyn TeeFsHtreeStorageOps,
    vers: u8,
    meta: Option<&TeeFsHtreeMeta>,
) -> TeeResult {
    tee_debug!(
        "htree_sync_node_to_storage: node.id: {:?}, node.dirty: {:?}, node.block_updated: {:?}",
        node.id,
        node.dirty,
        node.block_updated
    );

    // The node can be dirty while the block isn't updated due to
    // updated children, but if block is updated the node has to be
    // dirty.
    assert!(node.dirty >= node.block_updated);

    if !node.dirty {
        return Ok(());
    }
    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];

    calc_node_hash_with_type(TEE_FS_HTREE_HASH_ALG, node, meta, &mut digest)?;

    node.node.hash.copy_from_slice(&digest);

    node.dirty = false;
    node.block_updated = false;

    rpc_write_node(storage, node.id, vers, &node.node)?;
    Ok(())
}

fn sync_descendants_to_storage(
    node: &mut HtreeNode,
    storage: &dyn TeeFsHtreeStorageOps,
) -> TeeResult {
    if let Some(left) = node.left.get_mut() {
        sync_descendants_to_storage(left, storage)?;
        if left.dirty {
            let f = htree_node_committed_child(left.id & 1);
            node.dirty = true;
            node.node.flags ^= f as u16;
            let vers = ((node.node.flags & f as u16) != 0) as u8;
            write_current_node_to_storage(left, storage, vers, None)?;
        }
    }

    if let Some(right) = node.right.get_mut() {
        sync_descendants_to_storage(right, storage)?;
        if right.dirty {
            let f = htree_node_committed_child(right.id & 1);
            node.dirty = true;
            node.node.flags ^= f as u16;
            let vers = ((node.node.flags & f as u16) != 0) as u8;
            write_current_node_to_storage(right, storage, vers, None)?;
        }
    }

    Ok(())
}

fn sync_tree_to_storage(ht: &mut TeeFsHtree) -> TeeResult {
    let storage = ht.storage.as_ref();
    sync_descendants_to_storage(&mut ht.root, storage)?;

    let vers = ((ht.data.head.counter & 1) == 0) as u8;
    tee_debug!(
        "sync_tree_to_storage: counter: {}, root_node_vers: {}, root_flags: 0x{:04X}",
        ht.data.head.counter,
        vers,
        ht.root.node.flags
    );
    write_current_node_to_storage(&mut ht.root, storage, vers, Some(&ht.data.imeta.meta))
}

/// AEAD cipher context replacing mbedtls Cipher type-state.
/// Stores parameters for one-shot encrypt/decrypt at finalize time.
pub(super) struct AeadContext {
    alg: TeeAlg,
    key: [u8; TEE_FS_HTREE_FEK_SIZE],
    nonce: [u8; TEE_FS_HTREE_IV_SIZE],
    aad: Vec<u8>,
}

/// Marker for encryption direction (kept for API compatibility, no longer used as generic bound).
#[allow(dead_code)]
pub(super) struct Encryption;

/// Marker for decryption direction (kept for API compatibility, no longer used as generic bound).
#[allow(dead_code)]
pub(super) struct Decryption;

/// create cipher for encrypt or decrypt
///
/// # Arguments
/// * `alg` - the algorithm of the cipher
/// * `key_bytes` - the length of the key
/// # Returns
/// * `TeeResult<AeadContext>` - the cipher for encrypt or decrypt
fn create_cipher(alg: TeeAlg, key_bytes: usize) -> TeeResult<AeadContext> {
    match alg {
        TEE_ALG_AES_GCM | TEE_ALG_SM4_GCM => {
            if key_bytes != TEE_FS_HTREE_FEK_SIZE {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
            Ok(AeadContext {
                alg,
                key: [0u8; TEE_FS_HTREE_FEK_SIZE],
                nonce: [0u8; TEE_FS_HTREE_IV_SIZE],
                aad: Vec::new(),
            })
        }
        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

impl AeadContext {
    fn set_key_iv(mut self, key: &[u8], iv: &[u8]) -> TeeResult<Self> {
        self.key.copy_from_slice(&key[..TEE_FS_HTREE_FEK_SIZE]);
        self.nonce.copy_from_slice(&iv[..TEE_FS_HTREE_IV_SIZE]);
        Ok(self)
    }

    fn set_ad(mut self, aad: &[u8]) -> TeeResult<Self> {
        self.aad = aad.to_vec();
        Ok(self)
    }
}

/// init cipher for encrypt or decrypt, internal function,
/// using separated parameters to avoid borrow conflicts
///
/// # Arguments
/// * `fek` - the key for encrypt or decrypt
/// * `head` - the head of the tree
/// * `iv` - the iv for encrypt or decrypt
/// * `ni_is_some` - if the node is some
/// * `root_hash` - the hash of the root
/// # Returns
/// * `AeadContext` - the cipher for encrypt or decrypt
fn authenc_init_core(
    fek: &[u8; TEE_FS_HTREE_FEK_SIZE],
    head: &TeeFsHtreeImage,
    iv: &[u8; TEE_FS_HTREE_IV_SIZE],
    ni_is_some: bool,
    root_hash: Option<&[u8; TEE_FS_HTREE_HASH_SIZE]>,
) -> TeeResult<AeadContext> {
    const ALG: TeeAlg = TEE_FS_HTREE_AUTH_ENC_ALG;
    let mut aad_len = TEE_FS_HTREE_FEK_SIZE + TEE_FS_HTREE_IV_SIZE;

    if !ni_is_some {
        aad_len += TEE_FS_HTREE_HASH_SIZE + core::mem::size_of_val(&head.counter);
    }

    let cipher = create_cipher(ALG, TEE_FS_HTREE_FEK_SIZE)?;
    let cipher_k = cipher.set_key_iv(fek, iv)?;

    let mut ad: Vec<u8> = Vec::with_capacity(aad_len);
    if !ni_is_some {
        // When ni is None (not using node IV), AAD includes root.hash and head.counter
        if let Some(hash) = root_hash {
            ad.extend_from_slice(hash);
        }
        ad.extend_from_slice(bytemuck::bytes_of(&head.counter));
    }

    ad.extend_from_slice(bytemuck::bytes_of(&head.enc_fek));
    ad.extend_from_slice(iv);

    cipher_k.set_ad(ad.as_slice())
}

/// init cipher for encrypt or decrypt
///
/// # Arguments
/// * `mode` - the mode of the operation
/// * `ht` - the tree
/// * `ni` - the node
/// * `_payload_len` - the length of the payload
/// * `root_hash` - the hash of the root
/// # Returns
/// * `AeadContext` - the cipher for encrypt or decrypt
pub(super) fn authenc_init(
    mode: TEE_OperationMode,
    ht: &mut TeeFsHtree,
    ni: Option<&mut TeeFsHtreeNodeImage>,
    _payload_len: usize,
    root_hash: Option<&[u8; TEE_FS_HTREE_HASH_SIZE]>,
) -> TeeResult<AeadContext> {
    // When ni is Some (block encryption/decryption), root_hash should not be used
    // When ni is None, root_hash should be provided or use ht.root.node.hash
    let hash = if ni.is_some() {
        // When using node IV, don't use root_hash (will be ignored in authenc_init_core anyway)
        None
    } else {
        // When not using node IV, use provided root_hash or ht.root.node.hash
        root_hash.or(Some(&ht.root.node.hash))
    };

    let (iv, ni_is_some) = if let Some(ni) = ni {
        if mode == TEE_OperationMode::TEE_MODE_ENCRYPT {
            crypto_rng_read(&mut ni.iv)?;
        }
        (&ni.iv, true)
    } else {
        if mode == TEE_OperationMode::TEE_MODE_ENCRYPT {
            crypto_rng_read(&mut ht.data.head.iv)?;
        }
        (&ht.data.head.iv, false)
    };

    authenc_init_core(&ht.data.fek, &ht.data.head, iv, ni_is_some, hash)
}

/// special version for decrypt, using separated parameters to avoid borrow conflicts
///
/// # Arguments
/// * `fek` - the key for decrypt
/// * `head` - the head of the tree
/// * `ni_iv` - the iv from the node, if None use head.iv
/// * `root_hash` - the hash of the root
/// # Returns
/// * `AeadContext` - the cipher for decrypt
fn authenc_init_decrypt(
    fek: &[u8; TEE_FS_HTREE_FEK_SIZE],
    head: &TeeFsHtreeImage,
    ni_iv: Option<&[u8; TEE_FS_HTREE_IV_SIZE]>,
    root_hash: Option<&[u8; TEE_FS_HTREE_HASH_SIZE]>,
) -> TeeResult<AeadContext> {
    let (iv, ni_is_some) = if let Some(ni_iv) = ni_iv {
        (ni_iv, true)
    } else {
        (&head.iv, false)
    };

    tee_debug!(
        "authenc_init_decrypt: fek: {:?}, head: {:?}, iv: {:?}, ni_is_some: {:?}, root_hash: {:?}",
        hex::encode(fek),
        head,
        hex::encode(iv),
        ni_is_some,
        root_hash.map(hex::encode),
    );
    authenc_init_core(fek, head, iv, ni_is_some, root_hash)
}

/// special version for encrypt, using separated parameters to avoid borrow conflicts
///
/// # Arguments
/// * `fek` - the key for encrypt
/// * `head` - the head of the tree (only needs to be mutable if ni_iv is None)
/// * `ni_iv` - the iv from the node (will be filled with random data), if None use head.iv
/// * `root_hash` - the hash of the root
/// # Returns
/// * `AeadContext` - the cipher for encrypt
fn authenc_init_encrypt(
    fek: &[u8; TEE_FS_HTREE_FEK_SIZE],
    head: &TeeFsHtreeImage,
    ni_iv: Option<&mut [u8; TEE_FS_HTREE_IV_SIZE]>,
    root_hash: Option<&[u8; TEE_FS_HTREE_HASH_SIZE]>,
) -> TeeResult<AeadContext> {
    let (iv, ni_is_some) = if let Some(ni_iv) = ni_iv {
        crypto_rng_read(ni_iv)?;
        (ni_iv as &[u8; TEE_FS_HTREE_IV_SIZE], true)
    } else {
        // This case should not happen in tee_fs_htree_write_block
        // as we always pass Some(&mut node.node.iv)
        // But we keep it for completeness
        return Err(TEE_ERROR_GENERIC);
    };

    tee_debug!(
        "authenc_init_encrypt: fek: {:?}, head: {:?}, iv: {:?}, ni_is_some: {:?}, root_hash: {:?}",
        hex::encode(fek),
        head,
        hex::encode(iv),
        ni_is_some,
        root_hash.map(hex::encode),
    );
    authenc_init_core(fek, head, iv, ni_is_some, root_hash)
}

/// final for decrypt, using separated parameters to avoid borrow conflicts
///
/// # Arguments
/// * `ctx` - the cipher context for decrypt
/// * `tag` - the tag for decrypt
/// * `crypt` - the crypt for decrypt
/// * `plain` - the plain for decrypt
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn authenc_decrypt_final(
    ctx: AeadContext,
    tag: &[u8],
    crypt: &[u8],
    plain: &mut [u8],
) -> TeeResult {
    tee_debug!(
        "authenc_decrypt_final: tag: {:?}, crypt: {:?}, plain: {:?}",
        slice_fmt(tag),
        slice_fmt(crypt),
        slice_fmt(plain),
    );

    // tee_crypto decrypt expects ciphertext || tag concatenated
    let mut ct_with_tag = crypt.to_vec();
    ct_with_tag.extend_from_slice(tag);

    let result = match ctx.alg {
        TEE_ALG_AES_GCM => Aes128GcmAead::decrypt(&ctx.key, &ctx.nonce, &ctx.aad, &ct_with_tag),
        TEE_ALG_SM4_GCM => Sm4GcmAead::decrypt(&ctx.key, &ctx.nonce, &ctx.aad, &ct_with_tag),
        _ => return Err(TEE_ERROR_NOT_SUPPORTED),
    }
    .map_err(|_| TEE_ERROR_MAC_INVALID)?;

    plain.copy_from_slice(&result);
    Ok(())
}

/// final for encrypt, using separated parameters to avoid borrow conflicts
///
/// # Arguments
/// * `ctx` - the cipher context for encrypt
/// * `tag` - the tag for encrypt
/// * `plain` - the plain for encrypt
/// * `crypt` - the crypt for encrypt
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn authenc_encrypt_final(
    ctx: AeadContext,
    tag: &mut [u8],
    plain: &[u8],
    crypt: &mut [u8],
) -> TeeResult {
    // tee_crypto encrypt returns ciphertext || tag
    let result = match ctx.alg {
        TEE_ALG_AES_GCM => Aes128GcmAead::encrypt(&ctx.key, &ctx.nonce, &ctx.aad, plain),
        TEE_ALG_SM4_GCM => Sm4GcmAead::encrypt(&ctx.key, &ctx.nonce, &ctx.aad, plain),
        _ => return Err(TEE_ERROR_NOT_SUPPORTED),
    }
    .map_err(|_| TEE_ERROR_GENERIC)?;

    // result = ciphertext || tag (tag is 16 bytes)
    let ct_len = plain.len();
    if result.len() != ct_len + 16 {
        return Err(TEE_ERROR_GENERIC);
    }
    crypt.copy_from_slice(&result[..ct_len]);
    tag.copy_from_slice(&result[ct_len..]);

    tee_debug!(
        "authenc_encrypt_final: tag: {:?}, crypt: {:?}, plain: {:?}",
        slice_fmt(tag),
        slice_fmt(crypt),
        slice_fmt(plain),
    );
    Ok(())
}

/// update the root of the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn update_root(ht: &mut TeeFsHtree) -> TeeResult {
    ht.data.head.counter += 1;

    let cipher = authenc_init(
        TEE_OperationMode::TEE_MODE_ENCRYPT,
        ht,
        None,
        size_of_val(&ht.data.imeta),
        None,
    )?;

    authenc_encrypt_final(
        cipher,
        &mut ht.data.head.tag,
        bytes_of_mut(&mut ht.data.imeta),
        &mut ht.data.head.imeta,
    )?;

    Ok(())
}

/// traverse the tree post order
///
/// # Arguments
/// * `ht` - the tree
/// * `visitor` - the visitor function
/// * `fd` - the file descriptor
/// # Returns
/// * `TeeResult` - the result of the operation
fn htree_traverse_post_order<F>(ht: &TeeFsHtree, visitor: &mut F) -> TeeResult
where
    F: FnMut(&HtreeNode, &TeeFsHtreeData) -> TeeResult,
{
    post_order_traverse(&ht.root, &ht.data, visitor)?;

    Ok(())
}

/// traverse the tree post order mut
///
/// # Arguments
/// * `ht` - the tree
/// * `visitor` - the visitor function
/// # Returns
/// * `TeeResult` - the result of the operation
#[cfg(unittest)]
fn htree_traverse_post_order_mut<F>(ht: &mut TeeFsHtree, visitor: &mut F) -> TeeResult
where
    F: FnMut(&mut HtreeNode, &TeeFsHtreeData, &dyn TeeFsHtreeStorageOps) -> TeeResult,
{
    let storage = ht.storage.as_ref();
    post_order_traverse_mut(&mut ht.root, &ht.data, storage, visitor)?;

    Ok(())
}

/// verify the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn verify_tree(ht: &TeeFsHtree) -> TeeResult {
    htree_traverse_post_order(ht, &mut verify_node)?;
    Ok(())
}

/// calc the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
#[cfg(unittest)]
pub(super) fn calc_tree(ht: &mut TeeFsHtree) -> TeeResult {
    htree_traverse_post_order_mut(ht, &mut calc_node)?;
    Ok(())
}

/// print the hash of the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
#[cfg(unittest)]
pub(super) fn print_tree_hash(ht: &TeeFsHtree) -> TeeResult {
    htree_traverse_post_order(ht, &mut print_node_hash)?;

    Ok(())
}

/// init the root node of the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn init_root_node(ht: &mut TeeFsHtree) -> TeeResult {
    tee_debug!("init_root_node");
    let _hash = crypto_hash_alloc_ctx(TEE_ALG_SHA256)?;

    ht.root.id = 1;
    ht.root.dirty = true;

    // TODO: 需要优化，以去掉搬运过程
    let mut digest = [0u8; TEE_FS_HTREE_HASH_SIZE];
    calc_node_hash(&ht.root, &ht.data.imeta.meta, &mut digest)?;
    ht.root.node.hash.copy_from_slice(&digest);

    Ok(())
}

/// convert the node id to the level
///
/// # Arguments
/// * `node_id` - the node id
/// # Returns
/// * `usize` - the level of the node
pub(super) fn node_id_to_level(node_id: usize) -> usize {
    assert!(node_id > 0 && node_id < usize::MAX);
    (usize::BITS - node_id.leading_zeros()) as usize
}

/// find the closest node of the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `node_id` - the node id
/// # Returns
/// * `&mut HtreeNode` - the closest node
pub(super) fn find_closest_node(ht: &mut TeeFsHtree, node_id: usize) -> &mut HtreeNode {
    let target_level = node_id_to_level(node_id);

    // 记录访问路径（索引序列），避免在循环中的借用冲突
    let mut path = Vec::new();
    for n in 1..target_level {
        let bit_idx = target_level - n - 1;
        path.push((node_id >> bit_idx) & 1);
    }

    // 通过路径逐步访问节点，每次只借用一次
    let mut current = &mut ht.root;
    for &index in &path {
        // 检查子节点是否存在
        let child_exists = {
            let child_opt = current.get_child_by_index(index);
            child_opt.is_some()
        };

        if child_exists {
            // 重新获取子节点引用，因为之前的引用已经释放
            #[expect(
                clippy::unwrap_used,
                reason = "CG-021-no-panic-on-reachable-failure: child_exists checked this same \
                          index while the tree is exclusively borrowed and no mutation intervenes"
            )]
            let child = current.get_child_by_index_mut(index).unwrap();
            current = child;
        } else {
            // 子节点不存在，返回当前节点
            return current;
        }
    }

    current
}

/// find the node of the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `node_id` - the node id
/// # Returns
/// * `Option<&mut HtreeNode>` - the node
pub(super) fn find_node(ht: &mut TeeFsHtree, node_id: usize) -> Option<&mut HtreeNode> {
    let node = find_closest_node(ht, node_id);
    if node.id == node_id { Some(node) } else { None }
}

/// ensure the node exists, if not create it
/// internal function, not return reference, to avoid borrow conflicts
///
/// # Arguments
/// * `ht` - the tree
/// * `create` - if create the node
/// * `node_id` - the node id
/// # Returns
/// * `TeeResult` - the result of the operation
fn ensure_node_exists(ht: &mut TeeFsHtree, create: bool, node_id: usize) -> TeeResult {
    let current_node = find_closest_node(ht, node_id);
    let current_id = current_node.id;

    if current_id == node_id {
        return Ok(()); // node exists
    }

    if !create {
        return Err(TEE_ERROR_GENERIC);
    }

    // Add missing nodes, some nodes may already be there.
    for n in current_id..=node_id {
        let node = find_closest_node(ht, n);
        if node.id == n {
            continue;
        }
        // Node id n should be a child of node
        debug_assert_eq!((n >> 1), node.id);
        debug_assert!(node.get_child_by_index(n & 1).is_none());

        let new_node = HtreeNode::new(n, TeeFsHtreeNodeImage::default());

        if (n & 1) == 0 {
            HtreeNode::set_left(node, new_node);
        } else {
            HtreeNode::set_right(node, new_node);
        }
    }

    // update max_node_id
    if node_id > ht.data.imeta.max_node_id as usize {
        ht.data.imeta.max_node_id = node_id as u32;
    }

    Ok(())
}

/// get the node of the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `create` - if create the node
/// * `node_id` - the node id
/// # Returns
/// * `TeeResult<&mut HtreeNode>` - the node
pub(super) fn get_node(
    ht: &mut TeeFsHtree,
    create: bool,
    node_id: usize,
) -> TeeResult<&mut HtreeNode> {
    // first ensure the node exists (create the required nodes)
    ensure_node_exists(ht, create, node_id)?;

    // then find and return the node
    Ok(find_closest_node(ht, node_id))
}

/// get the index from the counter
///
/// # Arguments
/// * `counter0` - the counter0
/// * `counter1` - the counter1
/// # Returns
/// * `Result<u8, ()>` - the index
fn get_idx_from_counter(counter0: u32, counter1: u32) -> Result<u8, ()> {
    if (counter0 & 1) == 0 {
        // Equivalent to !(counter0 & 1)
        if (counter1 & 1) == 0 {
            // Equivalent to !(counter1 & 1)
            return Ok(0);
        }
        if counter0 > counter1 {
            return Ok(0);
        } else {
            return Ok(1);
        }
    }

    if (counter1 & 1) != 0 {
        // Equivalent to (counter1 & 1)
        Ok(1)
    } else {
        Err(())
    }
}

/// init the head from the data
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `ht` - the tree
/// * `hash` - the hash of the target node
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn init_head_from_data(
    // fd: &mut FileVariant,
    ht: &mut TeeFsHtree,
    hash: Option<&[u8]>,
) -> TeeResult {
    let storage = ht.storage.as_mut();
    // let mut fd = storage.get_hd();
    if let Some(target_hash) = hash {
        for idx in 0.. {
            let node_ref = &mut ht.root.node; // mutable access in scope
            rpc_read_node(storage, 1, idx, node_ref)?;
            tee_debug!(
                "rpc_read_node: root node hash: {:X?}, target_hash: {:X?}",
                hex::encode(node_ref.hash),
                hex::encode(target_hash)
            );
            if node_ref.hash == target_hash {
                rpc_read_head(storage, idx, &mut ht.data.head).inspect_err(|e| {
                    error!("rpc_read_head error! {:X?}", e);
                })?;
                break;
            }

            if idx != 0 {
                return Err(TEE_ERROR_SECURITY);
            }
        }
    } else {
        let mut heads = [TeeFsHtreeImage::default(); 2];
        for (idx, head) in heads.iter_mut().enumerate() {
            // Read version idx (0 or 1) of the head, consistent with C implementation
            rpc_read_head(storage, idx as u8, head)?;
            tee_debug!(
                "init_head_from_data: read head[{}]: counter={}",
                idx,
                head.counter
            );
        }

        let idx = get_idx_from_counter(heads[0].counter, heads[1].counter)
            .map_err(|_| TEE_ERROR_SECURITY)?;
        tee_debug!(
            "init_head_from_data: get_idx_from_counter result: idx={}, heads[0].counter={}, \
             heads[1].counter={}",
            idx,
            heads[0].counter,
            heads[1].counter
        );

        let node_ref = &mut ht.root.node;
        tee_debug!(
            "init_head_from_data: reading root node with vers: {}, heads[0].counter: {}, \
             heads[1].counter: {}",
            idx,
            heads[0].counter,
            heads[1].counter
        );
        rpc_read_node(storage, 1, idx, node_ref)?;
        tee_debug!(
            "init_head_from_data: root node loaded, flags: 0x{:04X}",
            node_ref.flags
        );

        ht.data.head = heads[idx as usize];
    }

    ht.root.id = 1;
    Ok(())
}

/// init the tree from the data
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn init_tree_from_data(ht: &mut TeeFsHtree) -> TeeResult {
    let mut node_image = TeeFsHtreeNodeImage::default();
    let mut node_id = 2;

    while node_id <= ht.data.imeta.max_node_id {
        // find the parent node (node_id >> 1)
        let parent_id = node_id >> 1;
        let parent_node = find_node(ht, parent_id as usize).ok_or(TEE_ERROR_GENERIC)?; // htree not find parent node, return error

        let committed_version = (parent_node.node.flags
            & htree_node_committed_child((node_id & 1) as usize) as u16
            != 0) as u8;

        // read the node from the storage
        let storage = ht.storage.as_ref();
        rpc_read_node(
            storage,
            node_id as usize,
            committed_version,
            &mut node_image,
        )?;

        // create node or get the existing node reference
        let nc = get_node(ht, true, node_id as usize)?;

        // set the content
        nc.node = node_image;

        node_id += 1;
    }

    Ok(())
}

/// verify the root of the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn verify_root(ht: &mut TeeFsHtree) -> TeeResult {
    let mut fek = [0u8; TEE_FS_HTREE_FEK_SIZE];
    tee_fs_fek_crypt(
        Some(&ht.data.uuid),
        TEE_OperationMode::TEE_MODE_DECRYPT,
        Some(&ht.data.head.enc_fek),
        TEE_FS_KM_FEK_SIZE,
        Some(&mut fek),
    )?;
    ht.data.fek.copy_from_slice(&fek);

    let cipher = authenc_init(
        TEE_OperationMode::TEE_MODE_DECRYPT,
        ht,
        None,
        size_of_val(&ht.data.imeta),
        None,
    )?;

    authenc_decrypt_final(
        cipher,
        &ht.data.head.tag,
        &ht.data.head.imeta,
        bytes_of_mut(&mut ht.data.imeta),
    )?;

    Ok(())
}

/// sync the tree to the storage
///
/// # Arguments
/// * `ht` - the tree
/// * `fd` - the file descriptor
/// * `hash` - the hash of the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_fs_htree_sync_to_storage(
    ht: &mut TeeFsHtree,
    // fd: &mut FileVariant,
    hash: Option<&mut [u8; TEE_FS_HTREE_HASH_SIZE]>,
) -> TeeResult {
    // if ht.is_none() {
    //     return Err(TeeResultCode::ErrorCorruptObject);
    // }
    tee_debug!(
        "tee_fs_htree_sync_to_storage: ht.data.dirty: {:?}",
        ht.data.dirty
    );

    if !ht.data.dirty {
        return Ok(());
    }

    // TODO: fd through out parameters?
    // let mut fd = FileVariant::open("filenamne", FS_OFLAG_DEFAULT, FS_MODE_644)
    //     .map_err(|_| TeeResultCode::ErrorGeneric)?;

    sync_tree_to_storage(ht).inspect_err(|e| error!("sync_tree_to_storage error! {:X?}", e))?;

    let counter_before = ht.data.head.counter;
    update_root(ht)?;
    let counter_after = ht.data.head.counter;
    let head_vers = (ht.data.head.counter & 1) as u8;
    tee_debug!(
        "tee_fs_htree_sync_to_storage: counter_before: {}, counter_after: {}, head_vers: {}, \
         root_flags: 0x{:04X}",
        counter_before,
        counter_after,
        head_vers,
        ht.root.node.flags
    );

    let storage = ht.storage.as_ref();
    rpc_write_head(storage, head_vers, &ht.data.head)?;

    ht.data.dirty = false;

    if let Some(slice) = hash {
        slice.copy_from_slice(&ht.root.node.hash);
    }

    // TODO:
    // tee_fs_htree_close(ht_arg);
    Ok(())
}

/// open the tree
///
/// # Arguments
/// * `fd` - the file descriptor
/// * `create` - if create the tree
/// * `hash` - the hash of the tree
/// * `uuid` - the uuid of the tree
/// # Returns
/// * `TeeResult<Box<TeeFsHtree>>` - the tree
pub(super) fn tee_fs_htree_open(
    // fd: &mut FileVariant,
    storage: Box<dyn TeeFsHtreeStorageOps>,
    create: bool,
    hash: Option<&mut [u8; TEE_FS_HTREE_HASH_SIZE]>,
    uuid: Option<&TEE_UUID>,
) -> TeeResult<Box<TeeFsHtree>> {
    tee_debug!(
        "tee_fs_htree_open: create: {:?}, hash: {:?}, uuid: {:?}",
        create,
        hash,
        uuid
    );
    let mut ht = Box::new(TeeFsHtree::default());
    if let Some(uuid_val) = uuid {
        ht.data.uuid = *uuid_val;
    }

    ht.storage = storage;

    let init_result = (|| {
        if create {
            let dummy_head = TeeFsHtreeImage::default();
            tee_debug!("tee_fs_htree_open: create: true");
            crypto_rng_read(&mut ht.data.fek)?;
            tee_fs_fek_crypt(
                Some(&ht.data.uuid),
                TEE_OperationMode::TEE_MODE_ENCRYPT,
                Some(&ht.data.fek),
                size_of_val(&ht.data.fek),
                Some(&mut ht.data.head.enc_fek),
            )?;
            init_root_node(&mut ht)?;
            tee_debug!("init_root_node to get: ht: {:?}", &ht);
            ht.data.dirty = true;
            tee_fs_htree_sync_to_storage(&mut ht, hash)?;
            let storage = ht.storage.as_ref();
            rpc_write_head(storage, 0, &dummy_head)?;
        } else {
            init_head_from_data(&mut ht, hash.as_ref().map(|s| &s[..])).inspect_err(|e| {
                error!("init_head_from_data error! {:X?}", e);
            })?;
            verify_root(&mut ht).inspect_err(|e| {
                error!("verify_root error! {:X?}", e);
            })?;
            init_tree_from_data(&mut ht).inspect_err(|e| {
                error!("init_tree_from_data error! {:X?}", e);
            })?;
            tee_debug!("verify_tree");
            verify_tree(&ht).inspect_err(|e| {
                error!("verify_tree error! {:X?}", e);
            })?;
        }

        Ok(())
    })();
    match init_result {
        Ok(_) => {
            // if init success, return ht ownership
            Ok(ht)
        }
        Err(e) => {
            // if init failed, call tee_fs_htree_close to clean ht
            if let Err(close_err) = tee_fs_htree_close(ht) {
                error!("tee_fs_htree_close error! {:?}", close_err);
            }
            Err(e)
        }
    }
}

/// close the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_fs_htree_close(_ht: Box<TeeFsHtree>) -> TeeResult {
    // TODO: check if no need to free nodes manually??? rust will free them automatically???
    // htree_traverse_post_order(&ht, &mut free_node, None)?;

    Ok(())
    // ht free after leave scope
}

/// get the meta of the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `&mut TeeFsHtreeMeta` - the meta of the tree
pub(super) fn tee_fs_htree_get_meta(ht: &mut TeeFsHtree) -> &mut TeeFsHtreeMeta {
    &mut ht.data.imeta.meta
}

/// set the dirty of the tree
///
/// # Arguments
/// * `ht` - the tree
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_fs_htree_meta_set_dirty(ht: &mut TeeFsHtree) {
    ht.data.dirty = true;
    ht.root.dirty = true;
}

/// get the block node of the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `create` - if create the block node
/// * `block_num` - the block number
/// # Returns
/// * `TeeResult<&mut HtreeNode>` - the block node
fn get_block_node(
    ht: &mut TeeFsHtree,
    create: bool,
    block_num: usize,
) -> TeeResult<&mut HtreeNode> {
    let node_id = block_num_to_node_id(block_num);
    get_node(ht, create, node_id)
}

/// read the block of the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `storage` - the storage
/// * `fd` - the file descriptor
/// * `block_num` - the block number
/// * `block` - the block
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_fs_htree_read_block(
    ht: &mut TeeFsHtree,
    // fd: &mut FileVariant,
    block_num: usize,
    block: &mut [u8],
) -> TeeResult {
    let block_size = {
        let storage = ht.storage.as_ref();
        storage.block_size()
    };
    tee_debug!(
        "tee_fs_htree_read_block: ht: {:?},  block_num: {:?}, block_len: 0x{:X?}, block_size: \
         0x{:X?}",
        ht,
        // fd,
        block_num,
        block.len(),
        block_size
    );
    // first get the node and extract the necessary information, then release the node borrow
    let (block_vers, ni_iv, ni_tag) = {
        let node = get_block_node(ht, false, block_num).map_err(|_| TEE_ERROR_CORRUPT_OBJECT)?;

        let vers = if (node.node.flags & HTREE_NODE_COMMITTED_BLOCK as u16) != 0 {
            1
        } else {
            0
        };

        tee_debug!(
            "tee_fs_htree_read_block: node.node.flags: 0x{:04X}, HTREE_NODE_COMMITTED_BLOCK: \
             0x{:04X}, block_vers: {}",
            node.node.flags,
            HTREE_NODE_COMMITTED_BLOCK as u16,
            vers
        );

        // extract iv and tag (these are Copy types, can be used directly)
        (vers, node.node.iv, node.node.tag)
    };

    // before calling authenc_init, get the root hash first
    let root_hash = ht.root.node.hash;

    tee_debug!(
        "tee_fs_htree_read_block: block_vers: {:?}, ni_iv: {:X?}, ni_tag: {:X?}, root_hash: {:X?}",
        block_vers,
        hex::encode(ni_iv),
        hex::encode(ni_tag),
        hex::encode(root_hash)
    );

    // now the node borrow is released, can safely borrow the other parts of ht
    let result = (|| {
        // allocate buffer, length is one BLOCK
        let mut enc_block = vec![0u8; block_size];

        let storage = ht.storage.as_ref();
        storage.rpc_read_init()?;

        let len =
            storage.rpc_read_final(TeeFsHtreeType::Block, block_num, block_vers, &mut enc_block)?;

        if len != block_size {
            error!(
                "tee_fs_htree_read_block: len: {:X?} != block_size: {:X?}",
                len, block_size
            );
            return Err(TEE_ERROR_CORRUPT_OBJECT);
        }

        // use authenc_init_decrypt, directly pass ni_iv without constructing temporary struct
        let cipher =
            authenc_init_decrypt(&ht.data.fek, &ht.data.head, Some(&ni_iv), Some(&root_hash))
                .inspect_err(|e| {
                    error!("authenc_init_decrypt: error: {:X?}", e);
                })
                .map_err(|_| TEE_ERROR_CORRUPT_OBJECT)?;

        // same as C version: res = authenc_decrypt_final(ctx, node->node.tag, enc_block, ht->stor->block_size, block);
        authenc_decrypt_final(cipher, &ni_tag, &enc_block, block).inspect_err(|e| {
            error!("authenc_decrypt_final: error: {:X?}", e);
        })?;

        Ok(())
    })();

    if result.is_err() {
        error!("tee_fs_htree_read_block error! {:X?}", result);
        // tee_fs_htree_close(ht)?;
    }

    result
}

/// write the block of the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `storage` - the storage
/// * `fd` - the file descriptor
/// * `block_num` - the block number
/// * `block` - the block
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_fs_htree_write_block(
    ht: &mut TeeFsHtree,
    // storage: &dyn TeeFsHtreeStorageOps,
    // fd: &mut FileVariant,
    block_num: usize,
    block: &[u8],
) -> TeeResult {
    tee_debug!(
        "tee_fs_htree_write_block-> block_num: {:?}, block.len: {:?}",
        block_num,
        block.len()
    );
    // before calling authenc_init, get the root hash first
    let root_hash = ht.root.node.hash;

    // extract fek and head before getting node to avoid borrow conflicts
    let fek = ht.data.fek;
    let head = ht.data.head;

    // 在获取 node 之前先获取 block_size 和初始化写入操作，避免借用冲突
    let (block_size, write_init_result) = {
        let storage = ht.storage.as_ref();
        let block_size = storage.block_size();
        let write_init_result = storage.rpc_write_init();
        (block_size, write_init_result)
    };
    write_init_result?;

    let result = (|| {
        // get node once for all operations
        let node = get_block_node(ht, true, block_num).map_err(|_| TEE_ERROR_CORRUPT_OBJECT)?;

        // if block not updated, toggle committed flag
        let block_vers = {
            let flags_before = node.node.flags;
            if !node.block_updated {
                node.node.flags ^= HTREE_NODE_COMMITTED_BLOCK as u16;
            }

            let vers = if (node.node.flags & HTREE_NODE_COMMITTED_BLOCK as u16) != 0 {
                1
            } else {
                0
            };
            tee_debug!(
                "tee_fs_htree_write_block: block_num: {}, block_updated: {}, flags_before: \
                 0x{:04X}, flags_after: 0x{:04X}, block_vers: {}",
                block_num,
                node.block_updated,
                flags_before,
                node.node.flags,
                vers
            );
            vers
        };

        // allocate encryption buffer (使用之前获取的 block_size)
        let mut enc_block = vec![0u8; block_size];

        // use authenc_init_encrypt, directly use extracted fek and head (immutable) since ni_iv is Some
        // authenc_init_encrypt will generate random IV for node.node.iv
        let cipher = authenc_init_encrypt(&fek, &head, Some(&mut node.node.iv), Some(&root_hash))
            .map_err(|_| TEE_ERROR_CORRUPT_OBJECT)?;

        // encrypt data block
        authenc_encrypt_final(cipher, &mut node.node.tag, block, &mut enc_block)?;

        // mark node as updated and dirty
        node.block_updated = true;
        node.dirty = true;

        // node borrow will be released when going out of scope
        // write encrypted data
        {
            let storage = ht.storage.as_ref();
            storage
                .rpc_write_final(TeeFsHtreeType::Block, block_num, block_vers, &enc_block)
                .inspect_err(|e| {
                    error!("rpc_write_final: error: {:X?}", e);
                })?;
        }

        // mark tree as dirty
        ht.data.dirty = true;

        Ok(())
    })();

    if result.is_err() {
        error!("tee_fs_htree_write_block error! {:?}", result);
        // tee_fs_htree_close(ht)?;
    }

    result
}

/// truncate the tree
///
/// # Arguments
/// * `ht` - the tree
/// * `block_num` - the block number
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_fs_htree_truncate(ht: &mut TeeFsHtree, block_num: usize) -> TeeResult {
    let node_id = block_num_to_node_id(block_num);

    while node_id < ht.data.imeta.max_node_id as usize {
        let current_max_node_id = ht.data.imeta.max_node_id as usize;
        let node = find_closest_node(ht, current_max_node_id);
        assert!(node.id == current_max_node_id);
        assert!(node.get_child_by_index(0).is_none() && node.get_child_by_index(1).is_none());
        assert!(node.parent_id.is_some());

        let (parent_id, child_index) = if let Some(parent_id) = node.parent_id {
            (parent_id, node.id & 1)
        } else {
            unreachable!() // already ensured by assert that parent exists
        };

        let _ = node;

        let parent_node = find_node(ht, parent_id).ok_or(TEE_ERROR_GENERIC)?;

        if child_index == 0 {
            parent_node.left = None;
        } else {
            parent_node.right = None;
        }
        ht.data.imeta.max_node_id -= 1;
        ht.data.dirty = true;
    }
    Ok(())
}

unittest::path_mod_test!(
    tests_htree_basic,
    "../../ut-tests/tee/fs_htree/tests_htree_basic.rs"
);

unittest::path_mod_test!(
    tests_node_id_to_level,
    "../../ut-tests/tee/fs_htree/tests_node_id_to_level.rs"
);

unittest::path_mod_test!(
    tests_find_closest_node,
    "../../ut-tests/tee/fs_htree/tests_find_closest_node.rs"
);

unittest::path_mod_test!(
    tests_get_node,
    "../../ut-tests/tee/fs_htree/tests_get_node.rs"
);

unittest::path_mod_test!(
    tests_authenc_funcs,
    "../../ut-tests/tee/fs_htree/tests_authenc_funcs.rs"
);

unittest::path_mod_test!(pub(super) mod tests_fs_htree, "../../ut-tests/tee/fs_htree/tests_fs_htree.rs");
