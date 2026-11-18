#!/usr/bin/env python3
"""Normalise capture-machine absolute paths out of the frozen golden artefacts.

plan §7 step 2c: "normalise every absolute path in the fixtures to
``${FIXTURE_ROOT}``" and "no fixture contains a capture-machine path".

What is rewritten
-----------------
Every *string* value inside every ``tests/golden/*.json`` (keys are left alone,
so the schema is untouched), mapping:

    $FIXTURE_ROOT   (default /tmp/zm-fix)      -> ${FIXTURE_ROOT}
    $HOME_ROOT      (default /tmp/zm-go-home)  -> ${HOME_ROOT}
    /home/jang      (capture machine home)     -> ${HOME_ROOT}
    /media/jang     (capture machine data root)-> ${CAPTURE_MEDIA}

``/home/jang`` maps to the same placeholder as the disposable HOME because the
only artefact that carries it is ``legacy-config-real.json`` (a read-only copy of
the developer's real ``config.json``), and for a fixture consumer both are
"the home root of the machine that ran the capture".  ``${CAPTURE_MEDIA}`` exists
because ``/api/places`` reports real ``/media/<user>/...`` mount points.

Why ``action.log`` is NOT rewritten
-----------------------------------
Read ``api/log.go``: every record ends with

    crc32 IEEE over **all preceding bytes** of the record

(api/log.go:79 "end 4 crc32 IEEE checksum of all bytes before this field",
written by ``encodeRecord`` and checked by ``verifyCRC``).  A placeholder is a
different length than the absolute path it would replace, so editing the payload
invalidates the CRC and the record would be *skipped* by
``ActionLog.rebuildIndex`` - the exact corruption path the Rust codec must prove
it survives (plan F4).  Therefore ``action.log`` is kept byte-exact and the
original -> placeholder mapping is recorded in ``LOG-PATHS.json`` instead.
``LOG-HEXDUMP.txt`` and ``LOG-RECORDS.json`` are likewise derived from the
byte-exact log and are regenerated rather than rewritten.

Usage:
  normalise-golden.py --golden tests/golden [--fixture-root /tmp/zm-fix]
                                          [--home-root /tmp/zm-go-home]
"""
from __future__ import annotations

import argparse
import json
import urllib.parse
import os
import re
import struct
import sys
import binascii
from pathlib import Path

CAPTURE_HOME = "/home/jang"
CAPTURE_MEDIA = "/media/jang"


PLACEHOLDER = {"fixture": "${FIXTURE_ROOT}", "home": "${HOME_ROOT}",
               "media": "${CAPTURE_MEDIA}"}


def _enc(s: str) -> str:
    """The way curl -G --data-urlencode writes a path into a query string."""
    return urllib.parse.quote(s, safe="")


def rules(fixture_root: str, home_root: str) -> list[tuple[str, str]]:
    """Literal *and* percent-encoded rewrites, longest first so that a nested root
    is replaced before its parent. Encoded sources (the recorded request URLs in
    INDEX.json) map to an encoded placeholder so the query-encoding evidence
    survives while the machine path does not."""
    pairs = [(fixture_root, PLACEHOLDER["fixture"]),
             (home_root, PLACEHOLDER["home"]),
             (CAPTURE_HOME, PLACEHOLDER["home"]),
             (CAPTURE_MEDIA, PLACEHOLDER["media"])]
    pairs += [(_enc(src), _enc(dst)) for src, dst in pairs]
    return sorted(pairs, key=lambda kv: -len(kv[0]))


def norm_str(value: str, rr: list[tuple[str, str]]) -> str:
    for needle, ph in rr:
        value = value.replace(needle, ph)
    return value


def norm_json(node, rr):
    if isinstance(node, str):
        return norm_str(node, rr)
    if isinstance(node, list):
        return [norm_json(x, rr) for x in node]
    if isinstance(node, dict):
        return {k: norm_json(v, rr) for k, v in node.items()}
    return node


# --------------------------------------------------------------------------- log
def log_records(raw: bytes):
    """Walk the ZMLG log the way api/log.go:rebuildIndex does."""
    off, out = 0, []
    while off + 26 <= len(raw):
        if raw[off:off + 4] != b"ZMLG":
            off += 1                                     # Go resyncs at off+1
            continue
        rid = struct.unpack(">q", raw[off + 4:off + 12])[0]
        reclen = struct.unpack(">I", raw[off + 22:off + 26])[0]
        if reclen < 26 or off + reclen + 4 > len(raw):
            off += 1
            continue
        body_len = reclen - 26
        p = off + 26
        n = struct.unpack(">H", raw[p:p + 2])[0]
        p += 2
        srcs = []
        for _ in range(n):
            sl = struct.unpack(">H", raw[p:p + 2])[0]
            p += 2
            srcs.append(raw[p:p + sl].decode("utf-8", "replace"))
            p += sl
        dl = struct.unpack(">H", raw[p:p + 2])[0]
        p += 2
        dest = raw[p:p + dl].decode("utf-8", "replace")
        p += dl
        nl = struct.unpack(">H", raw[p:p + 2])[0]
        p += 2
        name = raw[p:p + nl].decode("utf-8", "replace")
        p += nl
        crc_ok = (binascii.crc32(raw[off:off + reclen]) & 0xFFFFFFFF) == \
            struct.unpack(">I", raw[off + reclen:off + reclen + 4])[0]
        out.append({"offset": off, "id": rid, "sources": srcs, "dest": dest,
                    "name": name, "rec_len": reclen, "crc_ok": crc_ok})
        off += reclen + 4
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--golden", required=True, type=Path)
    ap.add_argument("--fixture-root", default="/tmp/zm-fix")
    ap.add_argument("--home-root", default="/tmp/zm-go-home")
    a = ap.parse_args()

    rr = rules(a.fixture_root, a.home_root)
    changed = 0
    for path in sorted(a.golden.glob("*.json")):
        try:
            doc = json.loads(path.read_text())
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            print(f"SKIP {path.name}: not valid JSON ({exc})", file=sys.stderr)
            continue
        new = norm_json(doc, rr)
        text = json.dumps(new, indent=2, ensure_ascii=False) + "\n"
        if text != path.read_text():
            path.write_text(text)
            changed += 1
    print(f"normalised {changed} JSON artefact(s) under {a.golden}")

    # byte-exact binary log + sidecar mapping
    log = a.golden / "action.log"
    if log.exists():
        recs = log_records(log.read_bytes())
        mapping: dict[str, str] = {}
        for r in recs:
            for p in r["sources"] + [r["dest"], r["name"]]:
                if p and p not in mapping:
                    mapping[p] = norm_str(p, rr)
        sidecar = {
            "note": ("action.log is deliberately NOT normalised: its per-record "
                     "crc32 (api/log.go encodeRecord/verifyCRC) covers the payload "
                     "bytes, so substituting a placeholder would corrupt every "
                     "record and the recovery scan would drop it. The log is kept "
                     "byte-exact and this sidecar maps each captured absolute path "
                     "to its placeholder."),
            "fixture_root_placeholder": "${FIXTURE_ROOT}",
            "home_root_placeholder": "${HOME_ROOT}",
            "record_count": len(recs),
            "all_crc_ok": all(r["crc_ok"] for r in recs),
            "paths": dict(sorted(mapping.items())),
        }
        (a.golden / "LOG-PATHS.json").write_text(
            json.dumps(sidecar, indent=2, ensure_ascii=False) + "\n")
        print(f"LOG-PATHS.json: {len(recs)} records, crc_ok={sidecar['all_crc_ok']}, "
              f"{len(mapping)} path strings")

    leftovers = []
    for path in sorted(a.golden.glob("*.json")):
        if path.name == "LOG-PATHS.json":
            continue                      # the mapping keys must keep the originals
        body = path.read_text()
        for needle, _ in rr:
            if needle in body:
                leftovers.append((path.name, needle))
    if leftovers:
        for name, needle in leftovers:
            print(f"LEFTOVER {name}: still contains {needle}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
