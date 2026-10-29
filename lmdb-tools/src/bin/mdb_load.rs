//! `mdb_load`: see [`lmdb_tools::load`].

fn main() {
    std::process::exit(lmdb_tools::common::run_main(lmdb_tools::load::run));
}
