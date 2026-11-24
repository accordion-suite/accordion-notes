//! The sandbox launcher on its own, for the tests that import Keep's notes
//! from Google Takeout. On a server, the server's own program is the
//! launcher, and it reads Takeout's archives with every module's products.

use accordion_core::takeout;
use accordion_notes::TAKEOUT_PRODUCTS;

fn main() {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(command) if command == takeout::COMMAND => {
            std::process::exit(takeout::reader::main(args, &TAKEOUT_PRODUCTS));
        }
        _ => accordion_sandbox::main(),
    }
}
