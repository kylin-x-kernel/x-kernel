mod args;
mod blockdev;
mod rootfs;
mod util;

use args::parse_args;
use rootfs::build_rootfs;

fn main() {
    let args = parse_args();

    if let Err(err) = build_rootfs(args) {
        eprintln!("crate_rootfs failed: {err}");
        std::process::exit(1);
    }
}
