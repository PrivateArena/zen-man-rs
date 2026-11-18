#!/usr/bin/env python3
"""Deterministic fixture generator for zen-man-rs golden capture (plan step 2).

Rebuilds the fixture tree (and the archive fixtures) from scratch so that the
byte content, the *name bytes* and the permission modes are reproducible.

Why the repo copy and the capture copy differ
--------------------------------------------
`tests/fixtures/tree/` is the checked-in tree. Git can only store regular
files, directories, symlinks and the single executable bit, so four fixture
cases cannot live in a commit:

  * `noperm.bin` (mode 000)   - git cannot even read it back to hash it
  * `noread-dir/` (mode 000)  - unreadable directory (ReadDirNames partial result)
  * `noexec-dir/` (mode 444)  - non-traversable directory
  * `myfifo`                  - FIFOs are not representable in git

`--hostile` applies those modes; `--relax` restores them to something git can
walk. `scripts/capture-golden.sh` builds a hostile tree under /tmp, captures,
then relaxes the in-repo tree so the repository is always walkable.

The `flatcap/` case (10 000+ entries, the Go flat-walk cap) is only created
with `--with-heavy` because 10 005 files must not be committed.

Usage:
  make-fixtures.py --root DIR [--archives DIR] [--hostile] [--relax] [--with-heavy]
"""

from __future__ import annotations

import argparse
import os
import shutil
import stat
import sys
import time
import gzip
import io
import zipfile
import tarfile
from pathlib import Path

# ---------------------------------------------------------------------------
# Fixed values that make the bytes reproducible
# ---------------------------------------------------------------------------
ZIP_DATE = (2026, 1, 1, 0, 0, 0)  # every zip entry gets this mtime
TAR_MTIME = 1_767_225_600  # 2026-01-01T00:00:00Z
UID_GID = 1000
Uname = "zenman"

FIXTURE_MTIME = 1_767_225_600  # 2026-01-01T00:00:00Z for tree files
FUTURE_MTIME = FIXTURE_MTIME + 10 * 365 * 24 * 3600  # ~10 years ahead

LONG_NAME = ("very-long-name-" + "x" * 190 + ".txt")  # 205 bytes, > 200
assert len(LONG_NAME.encode()) > 200

# The CRLF-in-name file.  The shell recipe in the plan is
#   "we"$(printf 'ird')$'\r'"name.txt"
# which is exactly the string below: 'weird' + CR + 'name.txt'.
CRLF_NAME = "weird\rname.txt"

# NFC "café.txt" and NFD "cafe\u0301.txt" are two *different* byte sequences
# that many terminals render identically - that is the point.
NFC_NAME = "caf\u00e9.txt"
NFD_NAME = "cafe\u0301.txt"
assert NFC_NAME != NFD_NAME


def w(path: Path, data: bytes, mode: int = 0o644, mtime: int = FIXTURE_MTIME) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    os.chmod(path, mode)
    os.utime(path, (mtime, mtime))


def d(path: Path, mode: int = 0o755) -> None:
    path.mkdir(parents=True, exist_ok=True)
    os.chmod(path, mode)


def symlink(link: Path, target: str) -> None:
    link.parent.mkdir(parents=True, exist_ok=True)
    if link.is_symlink() or link.exists():
        link.unlink()
    os.symlink(target, link)
    # pin the *link's own* mtime (lstat) so listings are byte-reproducible
    os.utime(link, (FIXTURE_MTIME, FIXTURE_MTIME), follow_symlinks=False)


# ---------------------------------------------------------------------------
# The tree
# ---------------------------------------------------------------------------
def build_tree(root: Path, hostile: bool, heavy: bool) -> None:
    if root.exists():
        relax(root)
        shutil.rmtree(root)
    root.mkdir(parents=True)

    # --- plain files -----------------------------------------------------
    w(root / "normal.txt", b"hello zen-man\n")
    w(root / ".hidden", b"hidden payload\n")  # backend must NOT filter this
    w(root / "a9.txt", b"a9\n")               # byte compare => a10 sorts before a9
    w(root / "a10.txt", b"a10\n")

    # --- awkward names ---------------------------------------------------
    w(root / CRLF_NAME, b"crlf-in-name\n")
    w(root / NFC_NAME, b"nfc e-acute\n")
    w(root / NFD_NAME, b"combining acute\n")
    w(root / "\u65e5\u672c.txt", b"nihon\n")           # 日本.txt
    w(root / "space name.txt", b"space\n")
    w(root / "hash#name.txt", b"hash\n")
    w(root / "quote'name.txt", b"quote\n")
    w(root / "back\\slash.txt", b"backslash\n")
    w(root / LONG_NAME, b"long\n")

    # --- modes / stat oddities -------------------------------------------
    w(root / "executable.sh", b"#!/bin/sh\necho hi\n", 0o755)
    w(root / "future.txt", b"from the future\n", 0o644, FUTURE_MTIME)

    sparse = root / "sparse.bin"
    with sparse.open("wb") as fh:
        fh.write(b"\x5a" * 4096)
        fh.truncate(1024 * 1024)          # 1 MiB logical, ~4 KiB on disk
    os.chmod(sparse, 0o644)
    os.utime(sparse, (FIXTURE_MTIME, FIXTURE_MTIME))

    # --- hard link pair ---------------------------------------------------
    w(root / "hard1.txt", b"hard linked\n")
    os.link(root / "hard1.txt", root / "hard2.txt")
    os.utime(root / "hard2.txt", (FIXTURE_MTIME, FIXTURE_MTIME))

    # --- symlinks ---------------------------------------------------------
    symlink(root / "link-to-file.txt", "normal.txt")
    symlink(root / "link-to-dir", "subdir")
    symlink(root / "link-chain.txt", "link-to-file.txt")  # symlink -> symlink
    symlink(root / "broken-link", "nowhere.txt")

    # --- directories ------------------------------------------------------
    w(root / "subdir" / "a.txt", b"a\n")
    w(root / "subdir" / "b" / "c.txt", b"c\n")
    d(root / "empty-dir")

    # sort-order probe: byte compare on lowercased names puts 'B.txt' before 'a.txt'
    w(root / "SortCase" / "B.txt", b"B\n")
    w(root / "SortCase" / "a.txt", b"a\n")

    # A *real directory* whose name ends in .zip.  The Go VFS read-only guard is
    # a substring test for ".zip/" / ".rar/" / ".7z/" on the raw path, so writing
    # to a legitimate local path with such a name is refused (quirk 13 false
    # positive, plan F9/F12 family).
    w(root / "fake.zip" / "inner.txt", b"not an archive\n")
    d(root / "fake.zip")

    # depth: 20 nested directories + a leaf (exercises index max_depth)
    deep = root / "deep"
    cur = deep
    for i in range(20):
        cur = cur / f"d{i}"
    w(cur / "leaf.txt", b"depth 20\n")

    # pagination: Go default page size is 200 (api/dir.go) -> 6 pages
    many = root / "many"
    for i in range(1, 1201):
        w(many / f"f{i:04d}.txt", f"entry {i}\n".encode())

    if heavy:
        # > the 10 000 entry cap of the flat walk (api/dir.go limitWalk)
        cap = root / "flatcap"
        for i in range(10_005):
            w(cap / f"g{i:05d}.txt", b"x")

    # --- hostile modes (never committed) ----------------------------------
    w(root / "noperm.bin", b"unreadable\n", 0o000)
    w(root / "noread-dir" / "inner.txt", b"inside unreadable dir\n")
    d(root / "noread-dir")
    w(root / "noexec-dir" / "inner.txt", b"inside non-executable dir\n")
    d(root / "noexec-dir")

    # Pin every directory mtime (deepest first) so listings are reproducible.
    for dirpath, dirnames, filenames in os.walk(root):
        for fn in filenames:
            fp = os.path.join(dirpath, fn)
            if not os.path.islink(fp):
                os.utime(fp, (FIXTURE_MTIME, FIXTURE_MTIME))
    for dirpath, dirnames, filenames in os.walk(root):
        pass
    for dirpath, dirnames, filenames in os.walk(root, topdown=False):
        os.utime(dirpath, (FIXTURE_MTIME, FIXTURE_MTIME))

    if hostile:
        apply_hostile(root)


def apply_hostile(root: Path) -> None:
    """Apply the modes the capture needs."""
    os.chmod(root / "noperm.bin", 0o000)
    os.chmod(root / "noread-dir", 0o000)   # no read => ReadDirNames fails
    os.chmod(root / "noexec-dir", 0o444)   # read ok, traversal denied


def relax(root: Path) -> None:
    """Undo hostile modes so git/`cp`/`tar` can walk the tree again."""
    for name, mode in (("noperm.bin", 0o644), ("noread-dir", 0o755),
                       ("noexec-dir", 0o755)):
        p = root / name
        try:
            os.chmod(p, mode)
        except OSError:
            pass


def make_fifo(root: Path) -> None:
    """Create the FIFO in a *runtime* tree only (git cannot store one)."""
    fifo = root / "myfifo"
    if fifo.is_symlink() or fifo.exists():
        fifo.unlink()
    os.mkfifo(fifo, 0o644)
    os.utime(fifo, (FIXTURE_MTIME, FIXTURE_MTIME))


# ---------------------------------------------------------------------------
# Archives
# ---------------------------------------------------------------------------
def _zinfo(name: str, *, mode: int = 0o644, is_dir: bool = False) -> zipfile.ZipInfo:
    zi = zipfile.ZipInfo(name, date_time=ZIP_DATE)
    zi.compress_type = zipfile.ZIP_DEFLATED
    zi.create_system = 3  # unix
    zi.external_attr = (mode & 0xFFFF) << 16
    if is_dir:
        zi.external_attr |= 0x10
    zi.external_attr |= stat.S_IFDIR << 16 if is_dir else stat.S_IFREG << 16
    return zi


def _add(fh: zipfile.ZipFile, name: str, data: bytes, mode: int = 0o644) -> None:
    fh.writestr(_zinfo(name, mode=mode), data)



def _make_inner_zip() -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as iz:
        _add(iz, "inner-file.txt", b"inside inner.zip\n")
    return buf.getvalue()


def build_archives(adir: Path) -> None:
    adir.mkdir(parents=True, exist_ok=True)

    # plain.zip --------------------------------------------------------------
    with zipfile.ZipFile(adir / "plain.zip", "w", zipfile.ZIP_DEFLATED) as z:
        _add(z, "top.txt", b"top level file\n")
        _add(z, "docs/readme.md", b"# readme\n")
        _add(z, "docs/nested/", b"", 0o755)
        _add(z, "docs/nested/deep.txt", b"deep\n")

    # nested.zip: zip -> 7z nesting case.  A real .7z cannot be created here
    # (no 7z/7za/p7zip on this host, and we must not download one), so the
    # nested member is itself a zip and the inner-7z member is a *stub* whose
    # bytes are a valid 7z signature header.  See docs/contracts.md.
    inner_zip_bytes = _make_inner_zip()
    sevenz_stub = b"7z\xbc\xaf\x27\x1c" + bytes(20)  # sig + empty header stub

    with zipfile.ZipFile(adir / "nested.zip", "w", zipfile.ZIP_DEFLATED) as z:
        _add(z, "outer.txt", b"outer file\n")
        _add(z, "inner.zip", inner_zip_bytes)
        _add(z, "inner.7z", sevenz_stub)

    # zipslip.zip: "../escape.txt" and an absolute-path entry ---------------
    with zipfile.ZipFile(adir / "zipslip.zip", "w", zipfile.ZIP_DEFLATED) as z:
        _add(z, "../escape.txt", b"zip slip payload\n")
        _add(z, "/tmp/absolute-escape.txt", b"absolute entry\n")
        _add(z, "safe.txt", b"safe\n")

    # symlink.zip: a zip entry whose mode says symlink ----------------------
    with zipfile.ZipFile(adir / "symlink.zip", "w", zipfile.ZIP_DEFLATED) as z:
        _add(z, "real.txt", b"symlink target\n")
        zi = _zinfo("link.txt", mode=0o777)
        zi.external_attr = (stat.S_IFLNK | 0o777) << 16
        z.writestr(zi, "real.txt")

    # plain.tar / plain.tar.gz  (gzip mtime pinned to 0 for byte reproducibility)
    tar_plain = _tar_bytes([("top.txt", b"top level file\n"),
                           ("docs/nested/deep.txt", b"deep\n")])
    (adir / "plain.tar").write_bytes(tar_plain)
    (adir / "plain.tar.gz").write_bytes(gzip.compress(tar_plain, mtime=0))


def _tar_add(t: tarfile.TarFile, name: str, data: bytes) -> None:
    ti = tarfile.TarInfo(name)
    ti.size = len(data)
    ti.mtime = TAR_MTIME
    ti.mode = 0o644
    ti.type = tarfile.REGTYPE
    ti.uid = UID_GID
    ti.gid = UID_GID
    ti.uname = Uname
    ti.gname = Uname
    t.addfile(ti, io.BytesIO(data))


def _tar_bytes(entries: list[tuple[str, bytes]]) -> bytes:
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, data in entries:
            _tar_add(t, name, data)
    return out.getvalue()


# ---------------------------------------------------------------------------
def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True, type=Path)
    ap.add_argument("--archives", type=Path)
    ap.add_argument("--hostile", action="store_true")
    ap.add_argument("--relax", action="store_true")
    ap.add_argument("--with-heavy", action="store_true")
    ap.add_argument("--fifo", action="store_true")
    a = ap.parse_args()

    if a.relax:
        relax(a.root)
        print(f"relaxed {a.root}")
        return 0

    build_tree(a.root, hostile=a.hostile, heavy=a.with_heavy)
    if a.fifo:
        make_fifo(a.root)
    if a.archives is not None:
        build_archives(a.archives)
    print(f"built tree at {a.root} (hostile={a.hostile}, heavy={a.with_heavy})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
