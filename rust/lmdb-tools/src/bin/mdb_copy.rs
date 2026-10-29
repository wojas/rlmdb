//! `mdb_copy`: see [`lmdb_tools::copy`].

fn main() {
    std::process::exit(lmdb_tools::common::run_main(lmdb_tools::copy::run));
}
