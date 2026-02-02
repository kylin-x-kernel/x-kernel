extern crate mbedtls;

use std::{
    io::{self, Write, stdin, stdout},
    net::TcpStream,
    sync::Arc,
};

use mbedtls::{
    Result as TlsResult,
    pk::{EcGroupId, Pk},
    rng::CtrDrbg,
    ssl::{
        CipherSuite, Config, Context,
        config::{Endpoint, Preset, Transport},
    },
    x509::Certificate,
};

#[path = "../tests/support/mod.rs"]
mod support;
use support::entropy::entropy_new;

fn result_main(addr: &str) -> TlsResult<()> {
    const PEM_KEY: &'static str =
        concat!(include_str!("../tests/support/keys/client_key.pem"), "\0");
    const PEM_CERT: &'static str =
        concat!(include_str!("../tests/support/keys/client_cert.pem"), "\0");
    const ROOT_CA_CERT: &'static str =
        concat!(include_str!("../tests/support/keys/ca_cert.pem"), "\0");

    let entropy = Arc::new(entropy_new());
    let rng = Arc::new(CtrDrbg::new(entropy, None)?);
    let ca_cert = Arc::new(Certificate::from_pem_multiple(ROOT_CA_CERT.as_bytes())?);
    let cert = Arc::new(Certificate::from_pem_multiple(PEM_CERT.as_bytes())?);
    let key = Arc::new(Pk::from_private_key(PEM_KEY.as_bytes(), None)?);
    let mut config = Config::new(Endpoint::Client, Transport::Stream, Preset::Default);
    let cipher_suite: Vec<i32> = vec![CipherSuite::Sm2WithSm4128GcmSm3.into(), 0];
    config.set_ciphersuites(Arc::new(cipher_suite));
    let curves: Vec<u32> = vec![EcGroupId::SM2P256R1.into(), 0];
    config.set_curves(Arc::new(curves));
    config.set_rng(rng);
    config.set_user_id(12345678);
    config.set_ca_list(ca_cert, None);
    config.push_cert(cert, key)?;
    let mut ctx = Context::new(Arc::new(config));
    let conn = TcpStream::connect(addr).unwrap();
    ctx.establish(conn, None)?;

    let mut line = String::new();
    stdin().read_line(&mut line).unwrap();
    ctx.write_all(line.as_bytes()).unwrap();
    io::copy(&mut ctx, &mut stdout()).unwrap();
    Ok(())
}

fn main() {
    let mut args = std::env::args();
    args.next();
    result_main(
        &args
            .next()
            .expect("supply destination in command-line argument"),
    )
    .unwrap();
}
