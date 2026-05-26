// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};
use core::bstr::ByteStr;

use kerrno::{KErrorKind, LinuxError, LinuxResult};
use knet::{
    RecvOptions, SocketAddrEx, SocketOps,
    unix::{DgramTransport, UnixAddr, UnixDomainSocket},
};
use kvfs::NodeType;
use kvfs_simple::{DirMapping, SimpleFs};

/// Bind /dev/log as a Unix domain socket for syslog messages
pub fn bind_dev_log() -> LinuxResult<()> {
    let server = UnixDomainSocket::new(DgramTransport::new(1));
    if let Err(err) = server.bind(SocketAddrEx::Unix(UnixAddr::Path("/dev/log".into()))) {
        let kind = KErrorKind::try_from(err);
        if matches!(
            kind,
            Ok(KErrorKind::Unsupported | KErrorKind::OperationNotSupported)
        ) {
            warn!("/dev/log not supported: {err}");
            return Ok(());
        }
        return Err(LinuxError::from(err));
    }
    ktask::spawn_with_name(
        move || {
            let mut buf = vec![0u8; 65536];
            loop {
                match server.recv(&mut buf[..], RecvOptions::default()) {
                    Ok(read) => {
                        let msg = ByteStr::new(buf[..read].trim_ascii_end());
                        info!("{msg}");
                    }
                    Err(err) => {
                        warn!("Failed to receive logs from client: {err:?}");
                        break;
                    }
                }
            }
        },
        "dev-log-server".into(),
    );
    Ok(())
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "log",
        kvfs_simple::SimpleFile::new(fs.clone(), NodeType::Socket, || Ok(b"")),
    );
}
