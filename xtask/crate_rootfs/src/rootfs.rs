use std::{
    fs::{File, OpenOptions},
    io::Read,
};

use rsext4::{
    config::BLOCK_SIZE,
    dir::get_inode_with_num,
    disknode::Ext4Inode,
    ext4::{mkfs, Ext4FileSystem},
    file::mkfile_with_ino,
    Jbd2Dev,
};

use crate::{
    args::Args,
    blockdev::FileBlockDev,
    util::{align_up, ensure_parent},
};

/// Build an ext4 image with a single hello binary.
pub fn build_rootfs(args: Args) -> Result<(), String> {
    ensure_parent(&args.image)?;

    let block_size = BLOCK_SIZE as u64;
    let size_bytes = align_up(args.size_bytes, block_size);
    if size_bytes < block_size * 16 {
        return Err("size is too small for ext4 image".to_string());
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&args.image)
        .map_err(|e| format!("failed to open image {}/{}: {e}", args.image.display(), size_bytes))?;
    file.set_len(size_bytes)
        .map_err(|e| format!("failed to set image size: {e}"))?;

    let total_blocks = size_bytes / block_size;
    let dev = FileBlockDev::new(file, total_blocks, BLOCK_SIZE as u32);
    let mut jbd = Jbd2Dev::initial_jbd2dev(0, dev, false);

    mkfs(&mut jbd).map_err(|e| format!("mkfs failed: {e}"))?;

    let mut fs = Ext4FileSystem::mount(&mut jbd).map_err(|e| format!("mount failed: {e}"))?;

    let mut hello_data = Vec::new();
    File::open(&args.src)
        .map_err(|e| format!("failed to open source binary {}: {e}", args.src.display()))?
        .read_to_end(&mut hello_data)
        .map_err(|e| format!("failed to read hello binary: {e}"))?;

    let dest = if args.dest.starts_with('/') {
        args.dest.clone()
    } else {
        format!("/{}", args.dest)
    };

    let (inode_num, _inode) = mkfile_with_ino(&mut jbd, &mut fs, &dest, Some(&hello_data), None)
        .ok_or_else(|| format!("failed to create file {dest} in ext4 image"))?;

    fs.modify_inode(&mut jbd, inode_num, |inode| {
        inode.i_mode = Ext4Inode::S_IFREG | 0o755;
    })
    .map_err(|e| format!("chmod failed: {e}"))?;

    let fallback_path = "/bin/hello";
    if dest != fallback_path {
        let (fallback_ino, _fallback_inode) = mkfile_with_ino(
            &mut jbd,
            &mut fs,
            fallback_path,
            Some(&hello_data),
            None,
        )
        .ok_or_else(|| format!("failed to create file {fallback_path} in ext4 image"))?;
        fs.modify_inode(&mut jbd, fallback_ino, |inode| {
            inode.i_mode = Ext4Inode::S_IFREG | 0o755;
        })
        .map_err(|e| format!("chmod failed: {e}"))?;
    }

    verify_present(&mut fs, &mut jbd, &dest)?;
    verify_present(&mut fs, &mut jbd, fallback_path)?;

    fs.umount(&mut jbd)
        .map_err(|e| format!("umount failed: {e}"))?;

    jbd.cantflush()
        .map_err(|e| format!("flush failed: {e}"))?;

    drop(jbd);

    verify_persisted(&args, total_blocks, &dest, fallback_path)?;

    Ok(())
}

fn verify_present(
    fs: &mut Ext4FileSystem,
    jbd: &mut Jbd2Dev<FileBlockDev>,
    path: &str,
) -> Result<(), String> {
    if get_inode_with_num(fs, jbd, path)
        .map_err(|e| format!("verify {path} failed: {e}"))?
        .is_none()
    {
        return Err(format!("verify {path} failed: not found"));
    }
    Ok(())
}

fn verify_persisted(
    args: &Args,
    total_blocks: u64,
    primary: &str,
    fallback: &str,
) -> Result<(), String> {
    let verify_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.image)
        .map_err(|e| format!("failed to reopen image for verify: {e}"))?;
    let verify_dev = FileBlockDev::new(verify_file, total_blocks, BLOCK_SIZE as u32);
    let mut verify_jbd = Jbd2Dev::initial_jbd2dev(0, verify_dev, false);
    let mut verify_fs =
        Ext4FileSystem::mount(&mut verify_jbd).map_err(|e| format!("verify mount failed: {e}"))?;

    if get_inode_with_num(&mut verify_fs, &mut verify_jbd, primary)
        .map_err(|e| format!("verify after umount {primary} failed: {e}"))?
        .is_none()
    {
        return Err(format!("verify after umount {primary} failed: not found"));
    }
    if get_inode_with_num(&mut verify_fs, &mut verify_jbd, fallback)
        .map_err(|e| format!("verify after umount {fallback} failed: {e}"))?
        .is_none()
    {
        return Err(format!("verify after umount {fallback} failed: not found"));
    }

    verify_fs
        .umount(&mut verify_jbd)
        .map_err(|e| format!("verify umount failed: {e}"))?;
    verify_jbd
        .cantflush()
        .map_err(|e| format!("verify flush failed: {e}"))?;
    Ok(())
}
