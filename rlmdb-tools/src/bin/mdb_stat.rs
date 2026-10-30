//! `mdb_stat`: see [`rlmdb_tools::stat`].

fn main() {
    std::process::exit(rlmdb_tools::common::run_main(rlmdb_tools::stat::run));
}
