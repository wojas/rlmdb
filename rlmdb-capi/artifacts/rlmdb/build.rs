//! Link options of `librlmdb.so` / `librlmdb.dylib`: SONAME
//! `librlmdb.so`, install name `@rpath/librlmdb.dylib`, `-z nodelete`
//! (see `../link.rs`).

include!("../link.rs");

fn main() {
    link_options("librlmdb.so", "librlmdb.dylib");
}
