//! `mdb_copy`: see [`rlmdb_tools::copy`].

fn main() {
    std::process::exit(rlmdb_tools::common::run_main(rlmdb_tools::copy::run));
}
