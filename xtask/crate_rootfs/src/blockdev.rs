use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};

use rsext4::{
    BlockDevice,
    error::{BlockDevError, BlockDevResult},
};

/// File-backed block device for building a raw ext4 image.
pub struct FileBlockDev {
    file: File,
    total_blocks: u64,
    block_size: u32,
    opened: bool,
}

impl FileBlockDev {
    /// Create a new file-backed block device.
    pub fn new(file: File, total_blocks: u64, block_size: u32) -> Self {
        Self {
            file,
            total_blocks,
            block_size,
            opened: true,
        }
    }

    fn check_range(&self, block_id: u32, count: u32) -> BlockDevResult<()> {
        let end = block_id as u64 + count as u64;
        if end > self.total_blocks {
            return Err(BlockDevError::BlockOutOfRange {
                block_id,
                max_blocks: self.total_blocks,
            });
        }
        Ok(())
    }
}

impl BlockDevice for FileBlockDev {
    fn write(&mut self, buffer: &[u8], block_id: u32, count: u32) -> BlockDevResult<()> {
        if !self.opened {
            return Err(BlockDevError::DeviceClosed);
        }
        self.check_range(block_id, count)?;

        let block_size = self.block_size as usize;
        let required = block_size * count as usize;
        if buffer.len() < required {
            return Err(BlockDevError::BufferTooSmall {
                provided: buffer.len(),
                required,
            });
        }

        let offset = block_id as u64 * self.block_size as u64;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| BlockDevError::IoError)?;
        self.file
            .write_all(&buffer[..required])
            .map_err(|_| BlockDevError::WriteError)?;
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], block_id: u32, count: u32) -> BlockDevResult<()> {
        if !self.opened {
            return Err(BlockDevError::DeviceClosed);
        }
        self.check_range(block_id, count)?;

        let block_size = self.block_size as usize;
        let required = block_size * count as usize;
        if buffer.len() < required {
            return Err(BlockDevError::BufferTooSmall {
                provided: buffer.len(),
                required,
            });
        }

        let offset = block_id as u64 * self.block_size as u64;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| BlockDevError::IoError)?;
        self.file
            .read_exact(&mut buffer[..required])
            .map_err(|_| BlockDevError::ReadError)?;
        Ok(())
    }

    fn open(&mut self) -> BlockDevResult<()> {
        self.opened = true;
        Ok(())
    }

    fn close(&mut self) -> BlockDevResult<()> {
        self.opened = false;
        Ok(())
    }

    fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn flush(&mut self) -> BlockDevResult<()> {
        self.file.sync_all().map_err(|_| BlockDevError::IoError)
    }

    fn is_open(&self) -> bool {
        self.opened
    }
}
