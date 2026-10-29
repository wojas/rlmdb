//! `mdb_dump`: see [`lmdb_tools::dump`].

fn main() {
    std::process::exit(lmdb_tools::common::run_main(lmdb_tools::dump::run));
}
