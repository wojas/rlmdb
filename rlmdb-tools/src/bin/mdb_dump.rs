//! `mdb_dump`: see [`rlmdb_tools::dump`].

fn main() {
    std::process::exit(rlmdb_tools::common::run_main(rlmdb_tools::dump::run));
}
