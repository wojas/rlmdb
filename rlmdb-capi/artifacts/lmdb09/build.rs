//! Link options of the LMDB 0.9 stub (`liblmdb09.so`, installed as
//! `liblmdb.so.0`): SONAME `liblmdb.so.0`, the name distributions give C
//! LMDB 0.9, so that a program linked against either finds the other;
//! install name `@rpath/liblmdb.dylib`; `-z nodelete` (see `../link.rs`).

include!("../link.rs");

fn main() {
    link_options("liblmdb.so.0", "liblmdb.dylib");
}
