// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 数据块缓存模块
//!
//! 提供文件和目录数据块的缓存管理，支持延迟写回和LRU淘汰

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use log::debug;

use crate::{blockdev::*, config::*, error::*};

const MAX_WRITEBACK_BLOCKS_PER_BATCH: usize = 100;

/// 数据块缓存键（全局块号）
pub type BlockCacheKey = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedBlockKind {
    Data,
    Metadata,
}

impl CachedBlockKind {
    fn is_metadata(self) -> bool {
        matches!(self, Self::Metadata)
    }
}

/// 缓存的数据块
#[derive(Debug, Clone)]
pub struct CachedBlock {
    /// 数据块内容 最后变成[u8]
    pub data: Vec<u8>,
    /// 是否被修改（脏）
    pub dirty: bool,
    /// 块号
    pub block_num: u64,
    /// 最后访问时间戳（用于LRU）
    pub last_access: u64,
    /// 块语义，用于决定写回是否进入 journal
    pub kind: CachedBlockKind,
}

impl CachedBlock {
    pub fn new(data: Vec<u8>, block_num: u64) -> Self {
        Self::new_with_kind(data, block_num, CachedBlockKind::Data)
    }

    pub fn new_with_kind(data: Vec<u8>, block_num: u64, kind: CachedBlockKind) -> Self {
        Self {
            data,
            dirty: false,
            block_num,
            last_access: 0,
            kind,
        }
    }

    /// 标记为脏
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

/// 数据块缓存管理器
pub struct DataBlockCache {
    /// 缓存的数据块
    cache: BTreeMap<BlockCacheKey, CachedBlock>,
    /// 已知的 metadata 块号
    metadata_blocks: BTreeSet<BlockCacheKey>,
    /// 最大缓存条目数
    max_entries: usize,
    /// 访问计数器（用于LRU）
    access_counter: u64,
    /// 块大小
    block_size: usize,
}

impl DataBlockCache {
    /// 创建数据块缓存
    ///
    /// # 参数
    /// * `max_entries` - 最大缓存条目数，建议32-128个
    /// * `block_size` - 块大小（通常是4096字节）
    pub fn new(max_entries: usize, block_size: usize) -> Self {
        Self {
            cache: BTreeMap::new(),
            metadata_blocks: BTreeSet::new(),
            max_entries,
            access_counter: 0,
            block_size,
        }
    }

    /// 创建默认配置的缓存（最多64个块，4KB大小）
    pub fn create_default() -> Self {
        Self::new(64, BLOCK_SIZE)
    }

    /// 从磁盘加载数据块
    fn load_block<B: BlockDevice>(
        &self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
    ) -> BlockDevResult<Vec<u8>> {
        block_dev.read_block(block_num as u32)?;
        let buffer = block_dev.buffer();
        Ok(buffer.to_vec())
    }

    /// 获取数据块（如果不存在则从磁盘加载） - 只读视图
    ///
    /// # 参数
    /// * `block_dev` - 块设备
    /// * `block_num` - 块号
    pub fn get_or_load<B: BlockDevice>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
    ) -> BlockDevResult<&CachedBlock> {
        let kind = self.kind_for_block(block_num);
        self.get_or_load_with_kind(block_dev, block_num, kind)
    }

    fn get_or_load_with_kind<B: BlockDevice>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
        kind: CachedBlockKind,
    ) -> BlockDevResult<&CachedBlock> {
        // 如果缓存中不存在，则加载
        if !self.cache.contains_key(&block_num) {
            if self.cache.len() >= self.max_entries {
                self.evict_lru(block_dev)?;
            }

            let data = self.load_block(block_dev, block_num)?;
            let cached = CachedBlock::new_with_kind(data, block_num, kind);
            self.cache.insert(block_num, cached);
        }

        // 更新访问时间
        self.access_counter += 1;
        if let Some(cached) = self.cache.get_mut(&block_num) {
            cached.last_access = self.access_counter;
            if kind.is_metadata() {
                cached.kind = CachedBlockKind::Metadata;
            }
        }

        self.cache.get(&block_num).ok_or(BlockDevError::Corrupted)
    }

    /// 内部使用：获取可变引用（如果不存在则从磁盘加载）
    fn get_or_load_mut<B: BlockDevice>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
    ) -> BlockDevResult<&mut CachedBlock> {
        let kind = self.kind_for_block(block_num);
        self.get_or_load_mut_with_kind(block_dev, block_num, kind)
    }

    fn get_or_load_mut_with_kind<B: BlockDevice>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
        kind: CachedBlockKind,
    ) -> BlockDevResult<&mut CachedBlock> {
        if !self.cache.contains_key(&block_num) {
            if self.cache.len() >= self.max_entries {
                self.evict_lru(block_dev)?;
            }

            let data = self.load_block(block_dev, block_num)?;
            let cached = CachedBlock::new_with_kind(data, block_num, kind);
            self.cache.insert(block_num, cached);
        }

        self.access_counter += 1;
        if let Some(cached) = self.cache.get_mut(&block_num) {
            cached.last_access = self.access_counter;
            if kind.is_metadata() {
                cached.kind = CachedBlockKind::Metadata;
            }
            Ok(cached)
        } else {
            Err(BlockDevError::Corrupted)
        }
    }

    /// 获取已缓存的数据块（不加载）
    pub fn get(&self, block_num: u64) -> Option<&CachedBlock> {
        self.cache.get(&block_num)
    }

    /// 获取可变引用
    pub fn get_mut(&mut self, block_num: u64) -> Option<&mut CachedBlock> {
        if let Some(cached) = self.cache.get_mut(&block_num) {
            self.access_counter += 1;
            cached.last_access = self.access_counter;
            Some(cached)
        } else {
            None
        }
    }

    /// 创建新的数据块缓存（不立即写入磁盘），并返回可变引用 自动标记为脏
    pub fn create_new(&mut self, block_num: u64) -> &mut CachedBlock {
        let kind = self.kind_for_block(block_num);
        self.create_new_with_kind(block_num, kind)
    }

    fn create_new_with_kind(&mut self, block_num: u64, kind: CachedBlockKind) -> &mut CachedBlock {
        if self.cache.len() >= self.max_entries {
            // 这里无法调用需要 block_dev 的 evict_lru，交由调用方控制
        }

        let data = alloc::vec![0u8; self.block_size];
        let mut cached = CachedBlock::new_with_kind(data, block_num, kind);
        cached.dirty = true;

        self.access_counter += 1;
        cached.last_access = self.access_counter;

        self.cache.insert(block_num, cached);
        self.cache.get_mut(&block_num).unwrap()
    }

    /// 标记数据块为脏
    pub fn mark_dirty(&mut self, block_num: u64) {
        if let Some(cached) = self.cache.get_mut(&block_num) {
            cached.mark_dirty();
        }
    }

    pub fn remember_metadata_block(&mut self, block_num: u64) {
        self.metadata_blocks.insert(block_num);
        if let Some(cached) = self.cache.get_mut(&block_num) {
            cached.kind = CachedBlockKind::Metadata;
        }
    }

    fn kind_for_block(&self, block_num: u64) -> CachedBlockKind {
        if self.metadata_blocks.contains(&block_num) {
            CachedBlockKind::Metadata
        } else {
            CachedBlockKind::Data
        }
    }

    /// 使用闭包修改指定数据块，并自动标记为脏
    pub fn modify<B, F>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
        f: F,
    ) -> BlockDevResult<()>
    where
        B: BlockDevice,
        F: FnOnce(&mut [u8]),
    {
        let cached = self.get_or_load_mut(block_dev, block_num)?;
        f(&mut cached.data);
        cached.mark_dirty();
        Ok(())
    }

    /// 为新分配的数据块提供基于闭包的初始化接口
    pub fn modify_new<F>(&mut self, block_num: u64, f: F)
    where
        F: FnOnce(&mut [u8]),
    {
        let cached = self.create_new(block_num);
        f(&mut cached.data);
        cached.mark_dirty();
    }

    /// LRU淘汰：找到最久未访问的并写回（如果脏）
    fn evict_lru<B: BlockDevice>(&mut self, block_dev: &mut Jbd2Dev<B>) -> BlockDevResult<()> {
        // 找到最小的last_access
        let lru_key = self
            .cache
            .iter()
            .min_by_key(|(_, cached)| cached.last_access)
            .map(|(key, _)| *key);

        if let Some(key) = lru_key {
            self.evict(block_dev, key)?;
        }

        Ok(())
    }

    /// 淘汰指定的数据块
    pub fn evict<B: BlockDevice>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
    ) -> BlockDevResult<()> {
        if let Some(cached) = self.cache.remove(&block_num)
            && cached.dirty
        {
            // 写回磁盘
            Self::write_block_static(block_dev, cached.block_num, &cached.data, cached.kind)?;
        }
        Ok(())
    }

    /// 刷新所有脏数据块到磁盘
    pub fn flush_all<B: BlockDevice>(&mut self, block_dev: &mut Jbd2Dev<B>) -> BlockDevResult<()> {
        // 收集需要写回的数据块信息（block_num, data），并按块号排序
        let mut dirty_blocks: Vec<(u64, Vec<u8>, CachedBlockKind)> = self
            .cache
            .values()
            .filter(|cached| cached.dirty)
            .map(|cached| (cached.block_num, cached.data.clone(), cached.kind))
            .collect();

        if dirty_blocks.is_empty() {
            return Ok(());
        }

        dirty_blocks.sort_by_key(|(block_num, ..)| *block_num);

        // 将连续块聚合后，使用 write_blocks 一次性写回。
        // 这里的上限按“块数”计，而不是字节数，避免一次聚合出过大的
        // 写回缓冲区并耗尽 virtio/kdma 的 bounce DMA 池。
        let block_size = self.block_size;
        let mut idx = 0usize;
        while idx < dirty_blocks.len() {
            let (start_block, _, kind) = dirty_blocks[idx];
            let mut run_len = 1usize;

            // 统计从 start_block 开始的连续块数量
            while idx + run_len < dirty_blocks.len() && run_len < MAX_WRITEBACK_BLOCKS_PER_BATCH {
                let expected = start_block + run_len as u64;
                if dirty_blocks[idx + run_len].0 == expected
                    && dirty_blocks[idx + run_len].2 == kind
                {
                    run_len += 1;
                } else {
                    break;
                }
            }

            // 聚合这一段连续块的数据到一个大的 buffer 中
            let mut buf: Vec<u8> = Vec::with_capacity(block_size * run_len);
            for off in 0..run_len {
                buf.extend_from_slice(&dirty_blocks[idx + off].1);
            }

            debug!(
                "DataBlockCache::flush_all: start_block={} run_len={} bytes={}",
                start_block,
                run_len,
                buf.len()
            );

            // 通过底层的 write_blocks 一次性写入连续块
            block_dev.write_blocks(&buf, start_block as u32, run_len as u32, kind.is_metadata())?;

            idx += run_len;
        }

        // 清除脏标记
        for cached in self.cache.values_mut() {
            cached.dirty = false;
        }

        Ok(())
    }

    /// 刷新指定数据块到磁盘
    pub fn flush<B: BlockDevice>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
    ) -> BlockDevResult<()> {
        if let Some(cached) = self.cache.get(&block_num)
            && cached.dirty
        {
            let data = cached.data.clone();
            let kind = cached.kind;
            Self::write_block_static(block_dev, block_num, &data, kind)?;

            if let Some(cached) = self.cache.get_mut(&block_num) {
                cached.dirty = false;
            }
        }
        Ok(())
    }

    /// 静态方法：写数据块到磁盘
    fn write_block_static<B: BlockDevice>(
        block_dev: &mut Jbd2Dev<B>,
        block_num: u64,
        data: &[u8],
        kind: CachedBlockKind,
    ) -> BlockDevResult<()> {
        block_dev.read_block(block_num as u32)?;
        let buffer = block_dev.buffer_mut();
        buffer[..data.len()].copy_from_slice(data);
        block_dev.write_block(block_num as u32, kind.is_metadata())?;
        Ok(())
    }

    /// 使缓存的数据块失效（不写回）
    ///
    /// 用于删除文件或目录时，避免写回已删除的数据
    pub fn invalidate(&mut self, block_num: u64) {
        self.cache.remove(&block_num);
        self.metadata_blocks.remove(&block_num);
    }

    /// 清空缓存（不写回）
    pub fn clear(&mut self) {
        self.cache.clear();
        self.metadata_blocks.clear();
    }

    /// 获取缓存统计
    pub fn stats(&self) -> DataBlockCacheStats {
        let dirty_count = self.cache.values().filter(|c| c.dirty).count();

        let total_size = self.cache.len() * self.block_size;

        DataBlockCacheStats {
            total_entries: self.cache.len(),
            dirty_entries: dirty_count,
            max_entries: self.max_entries,
            total_size_bytes: total_size,
        }
    }
}

/// 数据块缓存统计信息
#[derive(Debug, Clone, Copy)]
pub struct DataBlockCacheStats {
    pub total_entries: usize,
    pub dirty_entries: usize,
    pub max_entries: usize,
    pub total_size_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datablock_cache_basic() {
        let cache = DataBlockCache::new(8, BLOCK_SIZE);
        let stats = cache.stats();

        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.max_entries, 8);
        assert_eq!(stats.total_size_bytes, 0);
    }

    #[test]
    fn test_create_new_block() {
        let mut cache = DataBlockCache::new(8, BLOCK_SIZE);

        let block = cache.create_new(100);
        assert_eq!(block.block_num, 100);
        assert_eq!(block.data.len(), BLOCK_SIZE);
        assert!(block.dirty); // 新块应该标记为脏
        assert_eq!(block.kind, CachedBlockKind::Data);

        cache.remember_metadata_block(101);
        let meta = cache.create_new(101);
        assert_eq!(meta.kind, CachedBlockKind::Metadata);

        let stats = cache.stats();
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.dirty_entries, 2);
    }

    #[test]
    fn test_invalidate() {
        let mut cache = DataBlockCache::new(8, BLOCK_SIZE);

        cache.create_new(100);
        assert_eq!(cache.cache.len(), 1);

        cache.invalidate(100);
        assert_eq!(cache.cache.len(), 0);
    }
}
