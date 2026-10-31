# Patches to the C reference sources

`scripts/update-c-sources.sh` applies the `*.patch` files of `0.9/` to the
upstream files of `tests/lmdb-c/0.9`, and those of `1.0/` to
`tests/lmdb-c/1.0`, in name order. There are none at the moment; prefer
none: the C sources are the oracle, and every change to them is a change to
what rlmdb is tested against.

When a change is unavoidable (a build fix for a new compiler or platform, a
diagnostic the tests need):

* One patch per change, named `NNNN-short-name.patch` with a four-digit
  sequence number (`0001-fix-build-with-gcc-15.patch`).
* A header before the diff says why the change is needed, what it changes
  and whether it has been reported upstream (ITS number) or is fixed in a
  later upstream commit, so it can be dropped when the pin moves past it.
  `git apply` ignores text before the first `diff` line.
* Paths are relative to the version directory with one leading component
  (`a/mdb.c`, `b/mdb.c`); the script applies each patch with `git apply
  -p1` inside the directory. Edit the file in `tests/lmdb-c/<version>/`
  and write the patch with

  ```sh
  git diff --src-prefix=a/ --dst-prefix=b/ --relative=tests/lmdb-c/0.9 \
    -- tests/lmdb-c/0.9 >> tests/lmdb-c/patches/0.9/0001-short-name.patch
  ```

  after writing the header into that file (the explicit prefixes override
  a `diff.noprefix` setting).
* Commit the patch together with the patched file, then check with
  `scripts/update-c-sources.sh --check` that the tree is upstream plus
  patches.
