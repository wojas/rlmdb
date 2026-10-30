//! `mdb_load`: see [`rlmdb_tools::load`].

fn main() {
    std::process::exit(rlmdb_tools::common::run_main(rlmdb_tools::load::run));
}
