use core::bstr::ByteStr;

use kerrno::LinuxResult;
use knet::{
    RecvOptions, SocketAddrEx, SocketOps,
    unix::{DgramTransport, UnixAddr, UnixDomainSocket},
};

/// Bind /dev/log as a Unix domain socket for syslog messages
pub fn bind_dev_log() -> LinuxResult<()> {
    let server = UnixDomainSocket::new(DgramTransport::new(1));
    server.bind(SocketAddrEx::Unix(UnixAddr::Path("/dev/log".into())))?;
    ktask::spawn_with_name(
        move || {
            let mut buf = [0u8; 65536];
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
