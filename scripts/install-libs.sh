#!/bin/sh
# Build rlmdb's C libraries (rlmdb-capi) and install them with the headers
# and a pkg-config file, in the layout of rlmdb-capi/README.md
# ("Installing"):
#
#   $LIBDIR/librlmdb.a, $LIBDIR/librlmdb.so      (macOS: librlmdb.dylib)
#   $LIBDIR/pkgconfig/rlmdb.pc                    (-I.../rlmdb -lrlmdb)
#   $INCLUDEDIR/rlmdb/lmdb.h, $INCLUDEDIR/rlmdb/rlmdb.h
#   $LIBDIR/rlmdb/lmdb-0.9/liblmdb.so.0, liblmdb.so -> liblmdb.so.0,
#                          liblmdb.a              (the LMDB 0.9 stub)
#   $LIBDIR/rlmdb/lmdb-1.0/liblmdb.so, liblmdb.a  (the LMDB 1.0 stub)
#
# The 1.0 stub is installed under its SONAME (RLMDB_LMDB10_SONAME at build
# time, default liblmdb.so), with a liblmdb.so symlink if that differs. On
# macOS the stubs are liblmdb.dylib and each library's install name is set
# to its installed path (install_name_tool, then an ad-hoc signature). On
# Windows (run from MSYS2 or Git Bash): rlmdb.dll with its import library
# (rlmdb.dll.lib or librlmdb.dll.a) and static library (rlmdb.lib or
# librlmdb.a) in $LIBDIR, the stubs as lmdb.dll and lmdb.lib / liblmdb.a in
# the two directories (their import libraries name lmdb09.dll and
# lmdb10.dll and are not installed).
#
# The two lmdb-* directories are deliberately not on the default library
# path: the stubs are named like C LMDB and stand in for it only where a
# user or a distribution puts them first (LD_LIBRARY_PATH, LD_PRELOAD,
# rpath; DYLD_LIBRARY_PATH on macOS; lmdb.dll next to the program on
# Windows).
#
# Usage: scripts/install-libs.sh [--from <dir>] [--target <triple>]
#   --from <dir>       install the libraries cargo built in <dir> (for
#                      example target/release) instead of building them
#   --target <triple>  build for (or, with --from, install the libraries
#                      of) this Rust target
# Environment, as for make install:
#   PREFIX      /usr/local
#   LIBDIR      $PREFIX/lib
#   INCLUDEDIR  $PREFIX/include
#   DESTDIR     staging directory put in front of every path written; the
#               install names and rlmdb.pc name the paths without it
#   CARGO       cargo
#   RLMDB_LMDB10_SONAME  the 1.0 stub's SONAME (ELF; read when it is built)
# Works from any directory.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
from=
target=
while [ $# -gt 0 ]; do
  case $1 in
    --from) from=${2:?--from needs a directory}; shift 2 ;;
    --target) target=${2:?--target needs a triple}; shift 2 ;;
    -h|--help) sed -n '2,44p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install-libs.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done
cargo=${CARGO:-cargo}
prefix=${PREFIX:-/usr/local}
libdir=${LIBDIR:-$prefix/lib}
includedir=${INCLUDEDIR:-$prefix/include}
destdir=${DESTDIR:-}
soname10=${RLMDB_LMDB10_SONAME:-liblmdb.so}

# The target triple decides the file names.
triple=${target:-$(rustc -vV | sed -n 's/^host: //p')}
case $triple in
  *-apple-darwin) kind=macos ;;
  *-windows-msvc) kind=msvc ;;
  *-windows-gnu*) kind=mingw ;;
  *-linux-*|*-freebsd|*-netbsd|*-openbsd|*-dragonfly) kind=elf ;;
  *) echo "install-libs.sh: unsupported target $triple" >&2; exit 2 ;;
esac

# Where the libraries are.
if [ -n "$from" ]; then
  out=$(cd "$from" && pwd)
else
  "$cargo" build --release --manifest-path "$here/Cargo.toml" \
    -p librlmdb -p lmdb09-stub -p lmdb10-stub ${target:+--target "$target"}
  tdir=$("$cargo" metadata --format-version 1 --no-deps --manifest-path "$here/Cargo.toml" |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\\/g')
  out=$tdir/${target:+$target/}release
fi

# System libraries a program needs next to the static library (rustc's
# native-static-libs; rlmdb-capi/README.md has the table).
case $triple in
  *-linux-gnu*) syslibs="-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc" ;;
  *-apple-darwin) syslibs="-liconv" ;;
  *-freebsd) syslibs="-lrt -lutil -lexecinfo -lkvm -lmemstat -lprocstat -ldevstat -lpthread -lgcc_s -lc -lm" ;;
  *-netbsd) syslibs="-lutil -lrt -lexecinfo -lpthread -lgcc_s -lc -lm" ;;
  *-openbsd) syslibs="-lpthread -lc++abi -lc -lm -lutil -lexecinfo -lcompiler_rt" ;;
  *-dragonfly) syslibs="-lrt -lutil -lexecinfo -lkvm -lpthread -lgcc_pic -lc -lm" ;;
  *-windows-gnu*) syslibs="-lkernel32 -lntdll -luserenv -lws2_32 -ldbghelp" ;;
  *-windows-msvc) syslibs="-llegacy_stdio_definitions -lkernel32 -lntdll -luserenv -lws2_32 -ldbghelp" ;;
  *) syslibs= ;;
esac

# put <mode> <source> <destination>: copies through a temporary file and a
# rename, so that a running program keeps the library it has mapped.
put() {
  [ -f "$2" ] || { echo "install-libs.sh: $2 not found (build it first)" >&2; exit 1; }
  mkdir -p "$(dirname "$3")"
  cp "$2" "$3.tmp$$"
  chmod "$1" "$3.tmp$$"
  mv -f "$3.tmp$$" "$3"
  echo "installed $3"
}

# link <target> <name>: a symbolic link <name> -> <target>.
link() {
  rm -f "$2"
  ln -s "$1" "$2"
  echo "installed $2 -> $1"
}

# set_id <file> <install name>: the install name of a macOS library.
set_id() {
  install_name_tool -id "$2" "$1"
  # The linker's ad-hoc signature no longer matches; arm64 refuses to
  # load an unsigned or wrongly signed library.
  if command -v codesign >/dev/null 2>&1; then
    codesign --force --sign - "$1" 2>/dev/null
  fi
}

v09=$libdir/rlmdb/lmdb-0.9
v10=$libdir/rlmdb/lmdb-1.0
case $kind in
  elf)
    put 644 "$out/librlmdb.a" "$destdir$libdir/librlmdb.a"
    put 755 "$out/librlmdb.so" "$destdir$libdir/librlmdb.so"
    put 644 "$out/liblmdb09.a" "$destdir$v09/liblmdb.a"
    put 755 "$out/liblmdb09.so" "$destdir$v09/liblmdb.so.0"
    link liblmdb.so.0 "$destdir$v09/liblmdb.so"
    put 644 "$out/liblmdb10.a" "$destdir$v10/liblmdb.a"
    put 755 "$out/liblmdb10.so" "$destdir$v10/$soname10"
    [ "$soname10" = liblmdb.so ] || link "$soname10" "$destdir$v10/liblmdb.so"
    ;;
  macos)
    put 644 "$out/librlmdb.a" "$destdir$libdir/librlmdb.a"
    put 755 "$out/librlmdb.dylib" "$destdir$libdir/librlmdb.dylib"
    set_id "$destdir$libdir/librlmdb.dylib" "$libdir/librlmdb.dylib"
    put 644 "$out/liblmdb09.a" "$destdir$v09/liblmdb.a"
    put 755 "$out/liblmdb09.dylib" "$destdir$v09/liblmdb.dylib"
    set_id "$destdir$v09/liblmdb.dylib" "$v09/liblmdb.dylib"
    put 644 "$out/liblmdb10.a" "$destdir$v10/liblmdb.a"
    put 755 "$out/liblmdb10.dylib" "$destdir$v10/liblmdb.dylib"
    set_id "$destdir$v10/liblmdb.dylib" "$v10/liblmdb.dylib"
    ;;
  msvc)
    put 755 "$out/rlmdb.dll" "$destdir$libdir/rlmdb.dll"
    put 644 "$out/rlmdb.dll.lib" "$destdir$libdir/rlmdb.dll.lib"
    put 644 "$out/rlmdb.lib" "$destdir$libdir/rlmdb.lib"
    put 755 "$out/lmdb09.dll" "$destdir$v09/lmdb.dll"
    put 644 "$out/lmdb09.lib" "$destdir$v09/lmdb.lib"
    put 755 "$out/lmdb10.dll" "$destdir$v10/lmdb.dll"
    put 644 "$out/lmdb10.lib" "$destdir$v10/lmdb.lib"
    ;;
  mingw)
    put 755 "$out/rlmdb.dll" "$destdir$libdir/rlmdb.dll"
    put 644 "$out/librlmdb.dll.a" "$destdir$libdir/librlmdb.dll.a"
    put 644 "$out/librlmdb.a" "$destdir$libdir/librlmdb.a"
    put 755 "$out/lmdb09.dll" "$destdir$v09/lmdb.dll"
    put 644 "$out/liblmdb09.a" "$destdir$v09/liblmdb.a"
    put 755 "$out/lmdb10.dll" "$destdir$v10/lmdb.dll"
    put 644 "$out/liblmdb10.a" "$destdir$v10/liblmdb.a"
    ;;
esac

put 644 "$here/rlmdb-capi/include/lmdb.h" "$destdir$includedir/rlmdb/lmdb.h"
put 644 "$here/rlmdb-capi/include/rlmdb.h" "$destdir$includedir/rlmdb/rlmdb.h"

# rlmdb.pc, with paths below the prefix written relative to it.
relative() {
  case $1 in
    "$prefix"/*) printf '${prefix}%s' "${1#"$prefix"}" ;;
    *) printf '%s' "$1" ;;
  esac
}
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$here/Cargo.toml")
pc=$destdir$libdir/pkgconfig/rlmdb.pc
mkdir -p "$(dirname "$pc")"
cat > "$pc.tmp$$" <<EOF
prefix=$prefix
libdir=$(relative "$libdir")
includedir=$(relative "$includedir")

Name: rlmdb
Description: The LMDB C API of rlmdb, a rewrite of LMDB in Rust
URL: https://github.com/wojas/rlmdb
Version: $version
Cflags: -I\${includedir}/rlmdb
Libs: -L\${libdir} -lrlmdb
Libs.private: $syslibs
EOF
mv -f "$pc.tmp$$" "$pc"
echo "installed $pc"
