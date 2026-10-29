//! `mdb_drop`: see [`lmdb_tools::drop`].

fn main() {
    std::process::exit(lmdb_tools::common::run_main(lmdb_tools::drop::run));
}
