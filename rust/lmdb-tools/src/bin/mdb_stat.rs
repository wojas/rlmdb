//! `mdb_stat`: see [`lmdb_tools::stat`].

fn main() {
    std::process::exit(lmdb_tools::common::run_main(lmdb_tools::stat::run));
}
