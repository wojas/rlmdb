//! `mdb_drop`: see [`rlmdb_tools::drop`].

fn main() {
    std::process::exit(rlmdb_tools::common::run_main(rlmdb_tools::drop::run));
}
