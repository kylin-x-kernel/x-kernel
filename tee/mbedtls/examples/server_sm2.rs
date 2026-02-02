extern crate mbedtls;

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
};

use mbedtls::{
    Result as TlsResult,
    pk::{EcGroupId, Pk},
    rng::CtrDrbg,
    ssl::{
        CipherSuite, Config, Context,
        config::{AuthMode, Endpoint, Preset, Transport},
    },
    x509::Certificate,
};

#[path = "../tests/support/mod.rs"]
mod support;
use support::entropy::entropy_new;

fn listen<E, F: FnMut(TcpStream) -> Result<(), E>>(mut handle_client: F) -> Result<(), E> {
    let sock = TcpListener::bind("127.0.0.1:8080").unwrap();
    for conn in sock.incoming().map(Result::unwrap) {
        println!("Connection from {}", conn.peer_addr().unwrap());
        handle_client(conn)?;
    }

    Ok(())
}

fn result_main() -> TlsResult<()> {
    const PEM_KEY: &'static str =
        concat!(include_str!("../tests/support/keys/server_key.pem"), "\0");
    const PEM_CERT: &'static str =
        concat!(include_str!("../tests/support/keys/server_cert.pem"), "\0");
    const ROOT_CA_CERT: &'static str =
        concat!(include_str!("../tests/support/keys/ca_cert.pem"), "\0");

    let entropy = entropy_new();
    let rng = Arc::new(CtrDrbg::new(Arc::new(entropy), None)?);
    let ca_cert = Arc::new(Certificate::from_pem_multiple(ROOT_CA_CERT.as_bytes())?);
    let cert = Arc::new(Certificate::from_pem_multiple(PEM_CERT.as_bytes())?);
    let key = Arc::new(Pk::from_private_key(PEM_KEY.as_bytes(), None)?);
    let mut config = Config::new(Endpoint::Server, Transport::Stream, Preset::Default);
    let cipher_suite: Vec<i32> = vec![CipherSuite::Sm2WithSm4128GcmSm3.into(), 0];
    config.set_ciphersuites(Arc::new(cipher_suite));
    let curves: Vec<u32> = vec![EcGroupId::SM2P256R1.into(), 0];
    config.set_curves(Arc::new(curves));
    config.set_user_id(87654321);
    config.set_rng(rng);
    config.set_ca_list(ca_cert, None);
    config.set_authmode(AuthMode::Required);
    config.push_cert(cert, key)?;
    let rc_config = Arc::new(config);

    listen(move |conn| {
        let mut ctx = Context::new(rc_config.clone());
        ctx.establish(conn, None)?;
        let mut session = BufReader::new(ctx);
        let mut line = Vec::new();
        session.read_until(b'\n', &mut line).unwrap();
        println!("Received: {}", String::from_utf8_lossy(&line));
        session.get_mut().write_all(&line).unwrap();
        Ok(())
    })
}

fn main() {
    result_main().unwrap();
}
