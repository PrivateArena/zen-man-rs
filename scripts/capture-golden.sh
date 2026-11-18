#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# capture-golden.sh - step 2 of ARCHITECTURE_PLAN_zen-man-rs-egui-port.md
#
# Captures the Go reference implementation's wire format as frozen golden
# artefacts under tests/golden/, then normalises every capture-machine path.
#
#   * runs against a *disposable* zen-man instance under a private HOME so that
#     every byte of state it creates (config.json / zen-man.db / action.log)
#     belongs to this capture and nothing under the developer's real
#     ~/.config, ~/.local/share or ~/.zen-man is touched;
#     it never contacts the user's own instance on :7890;
#   * is idempotent: the fixture tree is rebuilt from scripts/make-fixtures.py
#     on every run, so the mutating operations below always start clean;
#   * fails loudly (set -euo pipefail + explicit probes) if the server is
#     unreachable, if a call that must succeed does not, or if a required
#     artefact is missing.
#
# Usage:
#   ZM_URL=http://127.0.0.1:7891 bash scripts/capture-golden.sh
#   (the instance is normally started for you by the block in START SERVER
#    below when nothing is listening on $ZM_URL)
# ---------------------------------------------------------------------------
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GOLDEN="$REPO/tests/golden"
FIXTURE_SRC="$REPO/tests/fixtures"
GO_REPO="${GO_REPO:-/media/jang/home/Deve/zen-man}"
ZM_URL="${ZM_URL:-http://127.0.0.1:7891}"

# --- capture-machine roots (the normaliser replaces exactly these) ---------
export FIXTURE_ROOT="${FIXTURE_ROOT:-/tmp/zm-fix}"
export HOME_ROOT="${HOME_ROOT:-/tmp/zm-go-home}"
CAPTREE="$FIXTURE_ROOT/tree"
CAPARCH="$FIXTURE_ROOT/archives"
OPSDIR="$FIXTURE_ROOT/ops"
USER_REAL_CONFIG="$HOME/.config/zen-man/config.json"
MINBIN="${TMPDIR:-/tmp}/zm-golden-bin"
SRVLOG="${TMPDIR:-/tmp}/zm-golden-server.log"
SRVPID="${TMPDIR:-/tmp}/zm-golden-server.pid"
# The instance is launched from here and never `cd`s, so its working directory is this one: that is
# where a relative extract `dest` lands (see step 10, `probe-only-not-used/` and `relative/`).
SRVCWD="$PWD"

export ZM_URL FIXTURE_ROOT HOME_ROOT
CURL=/usr/bin/curl
STARTED_OURS=0

wipe_fixtures() {  # relax hostile modes first, otherwise rm -rf fails
  if [ -d "$FIXTURE_ROOT" ]; then
    python3 "$REPO/scripts/make-fixtures.py" --root "$CAPTREE" --relax >/dev/null 2>&1 || true
    chmod -R u+rwX "$FIXTURE_ROOT" 2>/dev/null || true
    rm -rf "$FIXTURE_ROOT" || die "could not remove $FIXTURE_ROOT"
  fi
  mkdir -p "$FIXTURE_ROOT"
}

say() { printf '\033[1;36m== %s\033[0m\n' "$*"; }
die() { printf '\033[1;31mFATAL: %s\033[0m\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 0. tooling
# ---------------------------------------------------------------------------
command -v "$CURL" >/dev/null 2>&1 || [ -x "$CURL" ] || die "curl not found"
command -v python3 >/dev/null 2>&1 || die "python3 not found"
GO_SHA="$(cd "$GO_REPO" && git rev-parse HEAD 2>/dev/null || echo unknown)"
[ "$GO_SHA" != "unknown" ] || die "cannot read the Go commit SHA from $GO_REPO"

# ---------------------------------------------------------------------------
# 1. disposable instance (private HOME => private config.json / db / log)
# ---------------------------------------------------------------------------
start_server() {
  say "starting disposable instance on $ZM_URL (HOME=$HOME_ROOT)"
  [ -x "$GO_REPO/zen-man" ] || die "prebuilt binary not found: $GO_REPO/zen-man"
  wipe_fixtures
  rm -rf "$HOME_ROOT" "$SRVLOG" "$SRVPID"
  mkdir -p "$HOME_ROOT"
  # PATH is deliberately stripped down: no xclip/wl-copy, so the clipboard ops
  # cannot touch the developer's real system clipboard; a private bin dir with
  # only /bin/sh is exported so that /api/action/exec (which runs sh -c) has an
  # interpreter to find, giving us a *successful* action-exec capture.
  rm -rf "$MINBIN"; mkdir -p "$MINBIN"; ln -sf /bin/sh "$MINBIN/sh"; ln -sf /usr/bin/env "$MINBIN/env" 2>/dev/null || true
  local -a launch=()
  if command -v setsid >/dev/null 2>&1; then launch+=(setsid); fi
  launch+=(env -i HOME="$HOME_ROOT"
                XDG_DATA_HOME="$HOME_ROOT/.local/share"
                XDG_CONFIG_HOME="$HOME_ROOT/.config"
                PATH="$MINBIN"
                "$GO_REPO/zen-man" -port "${ZM_PORT:-7891}" -dev=false)
  "${launch[@]}" > "$SRVLOG" 2>&1 < /dev/null &
  echo $! > "$SRVPID"
  STARTED_OURS=1
  for _ in $(seq 1 50); do
    if "$CURL" -fsS -o /dev/null "$ZM_URL/"; then return 0; fi
    sleep 0.2
  done
  die "disposable instance never answered on $ZM_URL (see $SRVLOG)"
}

if ! "$CURL" -fsS -o /dev/null "$ZM_URL/" 2>/dev/null; then
  start_server
fi
"$CURL" -fsS -o /dev/null "$ZM_URL/" || die "zen-man is not reachable at $ZM_URL"
say "server reachable at $ZM_URL, Go SHA $GO_SHA"

if [ "$STARTED_OURS" = "0" ]; then
  say "reusing an already-running instance; wiping its state directory is your call"
fi

# ---------------------------------------------------------------------------
# 2. fixture tree (deterministic, rebuilt every run -> idempotent capture)
# ---------------------------------------------------------------------------
say "building fixture tree at $CAPTREE"
wipe_fixtures
python3 "$REPO/scripts/make-fixtures.py" \
  --root "$CAPTREE" --archives "$CAPARCH" --with-heavy --fifo --hostile \
  || die "fixture generation failed"
mkdir -p "$OPSDIR"
[ -d "$CAPTREE/fake.zip" ] || die "fixture tree incomplete: fake.zip missing"
[ -p "$CAPTREE/myfifo" ] || die "fixture tree incomplete: myfifo missing"

# ---------------------------------------------------------------------------
# 3. capture helpers
#    cap <name> <url> [curl args...]   -> tests/golden/<name>.json (+ INDEX row)
#    post <name> <url> <json>          -> same, POST with a JSON body
#    any <name> <http> <url> [args]    -> expectation-carrying GET (errors live)
# ---------------------------------------------------------------------------
mkdir -p "$GOLDEN"
: > "$FIXTURE_ROOT/index.tsv"

cap() {  # cap NAME EXPECT_STATUS URL [extra curl args...]
  local name="$1" want="$2" url="$3"; shift 3
  local out="$GOLDEN/$name.json" ct code
  local hdr; hdr="$(mktemp)"
  code="$("$CURL" -sS -o "$out" -D "$hdr" -w '%{http_code}' "$@" "$url")" \
    || die "curl failed (transport error) for $name: $url"
  ct="$(tr -d '\r' < "$hdr" | awk 'tolower($1)=="content-type:"{sub(/^[^:]*:[ \t]*/,"");print;exit}')"
  [ "$code" = "$want" ] || { head -c 400 "$out" >&2 || true; die "$name: HTTP $code, expected $want for $url"; }
  printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$code" "${ct:-none}" "$url" "$out" \
    >> "$FIXTURE_ROOT/index.tsv"
  rm -f "$hdr"
  printf '  %-34s %s %s\n' "$name" "$code" "${ct:-}"
}
get() { cap "$1" 200 "$2"; }

postj() { # post NAME WANT URL JSON
  cap "$1" "$2" "$3" -X POST -H 'Content-Type: application/json' --data "$4"
}

enc() { # enc PARAM=VALUE ... -> urlencoded query string
  "$CURL" -sG --data-urlencode "$1" -o /dev/null -w '%{url_effective}' "about:blank" 2>/dev/null \
    | true
}
# query-string encoder using python3 (curl -G is used per-call instead)
q() { python3 -c 'import sys,urllib.parse as u;print("&".join(u.quote_plus(a,"=") for a in sys.argv[1:]))' "$@"; }

# ---------------------------------------------------------------------------
# 4. PROVENANCE.json
# ---------------------------------------------------------------------------
say "PROVENANCE.json"
{
  echo "{"
  printf '  "go_repo": "%s",\n' "$GO_REPO"
  printf '  "go_commit": "%s",\n' "$GO_SHA"
  printf '  "go_binary_sha256": "%s",\n' "$(sha256sum "$GO_REPO/zen-man" | cut -d' ' -f1)"
  printf '  "base_url": "%s",\n' "$ZM_URL"
  printf '  "server_args": %s,\n' '"-port 7891 -dev=false"'
  printf '  "disposable_home": "%s",\n' "$HOME_ROOT"
  printf '  "fixture_root": "%s",\n' "$FIXTURE_ROOT"
  printf '  "captured_at_utc": "%s",\n' "$(LC_ALL=C date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '  "capture_machine_user": "%s",\n' "$(id -un)"
  printf '  "platform": "%s",\n' "$(uname -sr)"
  printf '  "normalised": true\n'
  echo "}"
} > "$GOLDEN/PROVENANCE.json"

# ---------------------------------------------------------------------------
# 5. /api/dir  (one file per fixture directory, incl. pagination pages)
# ---------------------------------------------------------------------------
say "/api/dir"
T="$CAPTREE"
get dir-root              "$ZM_URL/api/dir?$(q path=$T)"
get dir-subdir            "$ZM_URL/api/dir?$(q path=$T/subdir)"
get dir-empty             "$ZM_URL/api/dir?$(q path=$T/empty-dir)"
get dir-deep              "$ZM_URL/api/dir?$(q path=$T/deep/d0/d1/d2/d3/d4/d5/d6/d7/d8/d9/d10/d11/d12/d13/d14/d15/d16/d17/d18/d19)"
get dir-sortcase-asc      "$ZM_URL/api/dir?$(q path=$T/SortCase)&$(q order=asc)"
get dir-sortcase-desc     "$ZM_URL/api/dir?$(q path=$T/SortCase)&$(q order=desc)"
get dir-sort-size         "$ZM_URL/api/dir?$(q path=$T)&$(q sort=size)"
get dir-sort-date         "$ZM_URL/api/dir?$(q path=$T)&$(q sort=date)"
get dir-sort-date-desc    "$ZM_URL/api/dir?$(q path=$T)&$(q sort=date)&$(q order=desc)"
get dir-many-p1           "$ZM_URL/api/dir?$(q path=$T/many)&$(q limit=200)&$(q cursor=0)"
get dir-many-p2           "$ZM_URL/api/dir?$(q path=$T/many)&$(q limit=200)&$(q cursor=200)"
get dir-many-last         "$ZM_URL/api/dir?$(q path=$T/many)&$(q limit=200)&$(q cursor=1000)"
get dir-many-smallpage    "$ZM_URL/api/dir?$(q path=$T/many)&$(q limit=7)&$(q cursor=1197)"
get dir-flat-subdir       "$ZM_URL/api/dir?$(q path=$T/subdir)&$(q flat=true)"
get dir-flat-deep         "$ZM_URL/api/dir?$(q path=$T/deep)&$(q flat=true)"
get dir-flat-nofolders    "$ZM_URL/api/dir?$(q path=$T/subdir)&$(q flat=true)&$(q no_folders=true)"
get dir-flat-cap          "$ZM_URL/api/dir?$(q path=$T/flatcap)&$(q flat=true)&$(q limit=5)"
# error shapes
cap err-nonexistent       404 "$ZM_URL/api/dir?$(q path=$FIXTURE_ROOT/does-not-exist)"
cap err-not-a-directory   400 "$ZM_URL/api/dir?$(q path=$T/normal.txt)"
cap err-perm-denied-dir   500 "$ZM_URL/api/dir?$(q path=$T/noread-dir)"
# FINDING: a mode-444 (readable, non-traversable) directory is listed with
# HTTP 200 but every entry degrades to mode="unknown" size=0
# mod_time=-62135596800 (Go's zero Time.Unix()), because DirEntry.Info() is an
# lstat and EACCES on traversal is silently swallowed in api/dir.go.
get dir-perm-noexec       "$ZM_URL/api/dir?$(q path=$T/noexec-dir)"
# FINDING: unlike /api/dir/size, /api/dir never checks the method: a POST is
# served the listing with HTTP 200 (api/dir.go only special-cases OPTIONS).
get dir-post-returns-listing "$ZM_URL/api/dir?$(q path=$T)" -X POST

# /api/dir/size
say "/api/dir/size"
get dir-size-subdir       "$ZM_URL/api/dir/size?$(q path=$T/subdir)"
get dir-size-root         "$ZM_URL/api/dir/size?$(q path=$T)"
get dir-size-many         "$ZM_URL/api/dir/size?$(q path=$T/many)"
cap err-dir-size-nonexistent 404 "$ZM_URL/api/dir/size?$(q path=$FIXTURE_ROOT/does-not-exist)"

# ---------------------------------------------------------------------------
# 6. /api/search  (live walk) and /api/index/*  (SQLite index)
# ---------------------------------------------------------------------------
say "/api/search (live)"
get search-live-flat      "$ZM_URL/api/search?$(q path=$T)&$(q q=.txt)"
get search-live-recursive "$ZM_URL/api/search?$(q path=$T)&$(q q=leaf)"
get search-live-regex     "$ZM_URL/api/search?$(q path=$T)&$(q q=^a)&$(q regex=true)"
get search-live-multi     "$ZM_URL/api/search?$(q path=$T/subdir)&$(q q=b c)"
get search-live-capped    "$ZM_URL/api/search?$(q path=$T/many)&$(q q=f)&$(q limit=3)"
get search-live-crlf      "$ZM_URL/api/search?$(q path=$T)&$(q q=weird)"
get search-live-empty-q   "$ZM_URL/api/search?$(q path=$T/subdir)&$(q q=)"
cap err-search-bad-regex  400 "$ZM_URL/api/search?$(q path=$T)&$(q q=[)&$(q regex=true)"
cap err-search-nonexistent 404 "$ZM_URL/api/search?$(q path=$FIXTURE_ROOT/nope)&$(q q=x)"

say "/api/index/*"
# point the indexer at the fixture tree, rebuild, wait for it to finish
postj index-config-apply 200 "$ZM_URL/api/index/config" \
  "{\"roots\":[\"$CAPTREE\"],\"excludes\":[\"/proc\",\"/sys\",\"/dev\",\"**/.git\",\"**/node_modules\",\"**/__pycache__\",\"**/flatcap\"],\"auto_index\":false,\"worker_count\":2,\"follow_symlinks\":false}"
get index-status-idle     "$ZM_URL/api/index/status"
postj index-rebuild       200 "$ZM_URL/api/index/rebuild" '{}'
for _ in $(seq 1 120); do
  if ! "$CURL" -fsS "$ZM_URL/api/index/status" | grep -q '"is_indexing":true'; then break; fi
  sleep 0.5
done
get index-status          "$ZM_URL/api/index/status"
get search-index-word     "$ZM_URL/api/index/search?$(q q=leaf)"
get search-index-ext      "$ZM_URL/api/index/search?$(q q=ext:sh)"
get search-index-type-dir  "$ZM_URL/api/index/search?$(q q=type:dir)"
get search-index-in       "$ZM_URL/api/index/search?$(q q=in:$T/subdir)"
get search-index-paged    "$ZM_URL/api/index/search?$(q q=.txt)&$(q limit=3)&$(q offset=3)"
get search-index-empty    "$ZM_URL/api/index/search?$(q q=zzzznope)"
postj index-cancel        200 "$ZM_URL/api/index/cancel" '{}'
# search-config.json is a file, not an endpoint: it is copied in step 11.

# ---------------------------------------------------------------------------
# 7. /api/props, /api/diskspace, /api/places, /api/workspaces, /api/actions
# ---------------------------------------------------------------------------
say "/api/props + /api/diskspace"
get props-file            "$ZM_URL/api/props?$(q path=$T/normal.txt)"
get props-dir             "$ZM_URL/api/props?$(q path=$T/subdir)"
get props-symlink         "$ZM_URL/api/props?$(q path=$T/link-to-dir)"
get props-crlf-name       "$ZM_URL/api/props?$(q path=$T/weird"$'\r'"name.txt)"
get diskspace             "$ZM_URL/api/diskspace?$(q path=$FIXTURE_ROOT)"
cap err-diskspace-badpath 500 "$ZM_URL/api/diskspace?$(q path=$FIXTURE_ROOT/nope)"

say "/api/places /api/workspaces /api/actions"
get places-initial        "$ZM_URL/api/places"
postj places-add          200 "$ZM_URL/api/places" "{\"action\":\"add\",\"path\":\"$CAPTREE\"}"
postj places-add-dup      200 "$ZM_URL/api/places" "{\"action\":\"add\",\"path\":\"$CAPTREE\"}"
get places                "$ZM_URL/api/places"
postj places-remove       200 "$ZM_URL/api/places" "{\"action\":\"remove\",\"path\":\"$CAPTREE\"}"
postj workspaces-save     200 "$ZM_URL/api/workspaces" "{\"action\":\"save\",\"name\":\"fixture\",\"session\":{\"left_tabs\":[{\"id\":\"t1\",\"path\":\"$CAPTREE\",\"name\":\"Fixtures\",\"group\":\"A\",\"color\":\"#ff4a4a\"}],\"left_active\":\"t1\",\"right_tabs\":[],\"right_active\":\"\",\"split\":true}}"
get workspaces            "$ZM_URL/api/workspaces"
postj workspaces-restore  200 "$ZM_URL/api/workspaces" '{"action":"restore","name":"fixture"}'
cap err-workspaces-restore-missing 404 "$ZM_URL/api/workspaces" -X POST -H 'Content-Type: application/json' --data '{"action":"restore","name":"nope"}'
postj actions-create      200 "$ZM_URL/api/actions" '{"action":"create","action_data":{"name":"Fixture echo","icon":"terminal","command":"printf %s {name}","role":"files","patterns":"*.txt","show_output":true}}'
get actions               "$ZM_URL/api/actions"
ACTION_ID="$(python3 -c 'import json,sys;print(json.load(open("'"$GOLDEN"'/actions.json"))[0]["id"])')"
postj action-exec         200 "$ZM_URL/api/action/exec" "{\"id\":\"$ACTION_ID\",\"paths\":[\"$T/normal.txt\"],\"dir\":\"$T\"}"
cap err-action-exec-unknown 404 "$ZM_URL/api/action/exec" -X POST -H 'Content-Type: application/json' --data '{"id":"deadbeef","paths":[],"dir":"'"$T"'"}'
postj actions-delete      200 "$ZM_URL/api/actions" "{\"action\":\"delete\",\"action_data\":{\"id\":\"$ACTION_ID\"}}"

# ---------------------------------------------------------------------------
# 8. mutating operations, all confined to $FIXTURE_ROOT  -> action.log
# ---------------------------------------------------------------------------
say "/api/op  (fixtures stay under \$FIXTURE_ROOT)"
mkdir -p "$OPSDIR/inbox" "$OPSDIR/src" "$OPSDIR/dest"
printf 'payload one\n' > "$OPSDIR/src/one.txt"
printf 'payload two\n' > "$OPSDIR/src/two.txt"
printf 'victim\n'      > "$OPSDIR/src/victim.txt"
printf 'conflict file\n' > "$OPSDIR/dest/one.txt"
mkdir -p "$OPSDIR/src/mergedir" "$OPSDIR/dest/mergedir"
printf 'x\n' > "$OPSDIR/dest/mergedir/keep.txt"
mkdir -p "$OPSDIR/dotdot..name"                    # legal name, ".." substring
mkdir -p "$OPSDIR/bulk"; for i in $(seq 1 105); do printf 'b%s\n' "$i" > "$OPSDIR/bulk/b$i.txt"; done

postj op-mkdir            200 "$ZM_URL/api/op" "{\"op\":\"mkdir\",\"dest\":\"$OPSDIR\",\"name\":\"made-dir\"}"
postj op-rename           200 "$ZM_URL/api/op" "{\"op\":\"rename\",\"sources\":[\"$OPSDIR/made-dir\"],\"name\":\"renamed-dir\"}"
postj op-copy             200 "$ZM_URL/api/op" "{\"op\":\"copy\",\"sources\":[\"$OPSDIR/src/two.txt\"]}"
postj op-paste-copy       200 "$ZM_URL/api/op" "{\"op\":\"paste\",\"sources\":[\"$OPSDIR/src/two.txt\"],\"dest\":\"$OPSDIR/dest\"}"
postj op-cut              200 "$ZM_URL/api/op" "{\"op\":\"cut\",\"sources\":[\"$OPSDIR/src/victim.txt\"]}"
postj op-paste-move       200 "$ZM_URL/api/op" "{\"op\":\"paste\",\"sources\":[\"$OPSDIR/src/victim.txt\"],\"dest\":\"$OPSDIR/dest\"}"
postj op-delete-file      200 "$ZM_URL/api/op" "{\"op\":\"delete\",\"sources\":[\"$OPSDIR/bulk/b1.txt\"]}"
postj op-delete-dir       200 "$ZM_URL/api/op" "{\"op\":\"delete\",\"sources\":[\"$OPSDIR/empty-nonexistent\"]}" # error path
postj op-chmod            200 "$ZM_URL/api/op" "{\"op\":\"chmod\",\"sources\":[\"$OPSDIR/dest/two.txt\"],\"name\":\"600\"}"
postj op-chmod-leading-zero 200 "$ZM_URL/api/op" "{\"op\":\"chmod\",\"sources\":[\"$OPSDIR/dest/two.txt\"],\"name\":\"0644\"}"
cap err-chmod-octaltext   400 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data "{\"op\":\"chmod\",\"sources\":[\"$OPSDIR/dest/two.txt\"],\"name\":\"0o755\"}"
cap err-op-unknown        400 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data '{"op":"frobnicate","sources":["'"$OPSDIR"'"]}'
cap err-op-bad-json       400 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data 'not json'
# FINDING: delete with an empty source list, and delete of a path that does not
# exist, both answer {"status": "success"} 200 (os.RemoveAll on a missing path
# returns nil).  The same is true of a delete whose target was never there.
cap op-delete-empty-sources 200 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data '{"op":"delete","sources":[]}'
cap err-vfs-readonly      405 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data "{\"op\":\"delete\",\"sources\":[\"$T/fake.zip/inner.txt\"]}"
# quirk 8: dir-into-dir paste conflict => HTTP 200 + status:conflict
postj paste-conflict      200 "$ZM_URL/api/op" "{\"op\":\"paste\",\"sources\":[\"$OPSDIR/src/mergedir\"],\"dest\":\"$OPSDIR/dest\"}"
postj paste-conflict-merge 200 "$ZM_URL/api/op" "{\"op\":\"paste\",\"sources\":[\"$OPSDIR/src/mergedir\"],\"dest\":\"$OPSDIR/dest\",\"merge\":true}"
# quirk 9: copyFile silently skips an existing destination, still returns success
postj paste-skip-existing 200 "$ZM_URL/api/op" "{\"op\":\"paste\",\"sources\":[\"$OPSDIR/src/one.txt\"],\"dest\":\"$OPSDIR/dest\"}"
# quirk 10: moveItem errors on collision (cut + paste onto an existing *file*)
postj op-cut-collision    200 "$ZM_URL/api/op" "{\"op\":\"cut\",\"sources\":[\"$OPSDIR/src/one.txt\"]}"
cap err-paste-move-collision 500 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data "{\"op\":\"paste\",\"sources\":[\"$OPSDIR/src/one.txt\"],\"dest\":\"$OPSDIR/dest\"}"
# quirk 11: batch rename smuggled as a JSON array inside `name`
postj op-batch-rename     200 "$ZM_URL/api/op" "{\"op\":\"rename\",\"sources\":[\"$OPSDIR/dest/one.txt\",\"$OPSDIR/dest/two.txt\"],\"name\":\"[\\\"renamed-one.txt\\\",\\\"renamed-two.txt\\\"]\"}"
# FINDING (quirk 11 sharp edge): with exactly ONE source the handler takes the
# single-rename branch, so a JSON-array `name` is not sniffed at all - the file
# is literally renamed to ["a","b"] on disk and logged as ActionRename.
cap op-single-rename-json-name 200 "$ZM_URL/api/op" -X POST -H 'Content-Type: application/json' --data "{\"op\":\"rename\",\"sources\":[\"$OPSDIR/dest/renamed-one.txt\"],\"name\":\"[\\\"a\\\",\\\"b\\\"]\"}"
# quirk 12: >100 sources => reload_required instead of entries
BULK="$(python3 -c 'import json,os;d="'"$OPSDIR"'/bulk";print(json.dumps([os.path.join(d,f) for f in sorted(os.listdir(d))]))')"
postj op-copy-bulk        200 "$ZM_URL/api/op" "{\"op\":\"copy\",\"sources\":$BULK}"
postj op-paste-bulk       200 "$ZM_URL/api/op" "{\"op\":\"paste\",\"sources\":$BULK,\"dest\":\"$OPSDIR/dest\"}"
# ActionPasteLink (type 8)
postj op-copy-link        200 "$ZM_URL/api/op" "{\"op\":\"copy\",\"sources\":[\"$OPSDIR/dest/renamed-one.txt\"]}"
postj op-paste-link       200 "$ZM_URL/api/op" "{\"op\":\"paste_link\",\"dest\":\"$OPSDIR/dest\"}"
postj op-paste-link-conflict 200 "$ZM_URL/api/op" "{\"op\":\"paste_link\",\"dest\":\"$OPSDIR/dest\"}"
postj op-clear            200 "$ZM_URL/api/op" '{"op":"clear"}'

# ---------------------------------------------------------------------------
# 9. /api/log + /api/log/revert  (covers the CRC re-patch path)
# ---------------------------------------------------------------------------
say "/api/log"
get log                   "$ZM_URL/api/log?$(q limit=500)"
get log-path-filtered     "$ZM_URL/api/log?$(q path=$OPSDIR/dest/two.txt)&$(q limit=10)"
get log-limit1            "$ZM_URL/api/log?$(q limit=1)"
# revert the mkdir record: this is the only in-place log mutation Go performs
# pick the newest not-yet-reverted mkdir record (the log accumulates across
# capture runs against a reused instance; a fresh run appends a new one)
MKID="$(python3 -c '
import json
rs=json.load(open("'"$GOLDEN"'/log.json"))["records"]
m=[r["id"] for r in rs if r["action"]==6 and r["status"]==0]
print(max(m) if m else "")')"
if [ -n "$MKID" ]; then
  postj log-revert-mkdir  200 "$ZM_URL/api/log/revert" "{\"id\":$MKID}"
  cap err-log-revert-twice 409 "$ZM_URL/api/log/revert" -X POST -H 'Content-Type: application/json' --data "{\"id\":$MKID}"
  NRID="$(python3 -c '
import json
rs=json.load(open("'"$GOLDEN"'/log.json"))["records"]
d=[r["id"] for r in rs if not r["reversible"]]
print(max(d) if d else 0)')"
  cap err-log-revert-nonreversible 422 "$ZM_URL/api/log/revert" -X POST -H 'Content-Type: application/json' --data "{\"id\":$NRID}"
  cap err-log-revert-unknown 404 "$ZM_URL/api/log/revert" -X POST -H 'Content-Type: application/json' --data '{"id":987654}'
  get log-after-revert    "$ZM_URL/api/log?$(q limit=500)"
fi

# ---------------------------------------------------------------------------
# 10. /api/archive/*
# ---------------------------------------------------------------------------
say "/api/archive/*"
A="$CAPARCH"
# The manifest cache lives in the server process and is keyed by host mtime; a
# rebuilt fixture tree therefore has fresh mtimes and the first listing of every
# archive answers 409 archive_modified.  That probe is exactly the
# archive-modified-409 evidence we want, so warm the cache here (each 409 also
# invalidates the stale entry) and capture the deterministic 409 separately.
for qs in "$(q archive=$A/plain.zip)" \
          "$(q archive=$A/plain.zip)&$(q path=docs/nested)" \
          "$(q archive=$A/nested.zip)" \
          "$(q archive=$A/nested.zip)&$(q nested=inner.zip)" \
          "$(q archive=$A/zipslip.zip)" \
          "$(q archive=$A/symlink.zip)"; do
  "$CURL" -s -o /dev/null "$ZM_URL/api/archive/dir?$qs" || true
done
get archive-list-plain    "$ZM_URL/api/archive/dir?$(q archive=$A/plain.zip)"
get archive-list-plain-sub "$ZM_URL/api/archive/dir?$(q archive=$A/plain.zip)&$(q path=docs/nested)"
get archive-list-nested   "$ZM_URL/api/archive/dir?$(q archive=$A/nested.zip)"
get archive-nested-inner  "$ZM_URL/api/archive/dir?$(q archive=$A/nested.zip)&$(q nested=inner.zip)"
get archive-list-zipslip  "$ZM_URL/api/archive/dir?$(q archive=$A/zipslip.zip)"
get archive-list-symlink  "$ZM_URL/api/archive/dir?$(q archive=$A/symlink.zip)"
cap err-archive-missing   400 "$ZM_URL/api/archive/dir?$(q path=x)"
cap err-archive-notfound  500 "$ZM_URL/api/archive/dir?$(q archive=$A/nope.zip)"
cap err-archive-baddir    404 "$ZM_URL/api/archive/dir?$(q archive=$A/plain.zip)&$(q path=no-such-dir)"
"$CURL" -fsS -o "$GOLDEN/archive-stream-plain.txt" \
  "$ZM_URL/api/archive/stream?$(q archive=$A/plain.zip)&$(q path=top.txt)" \
  || die "archive stream failed"
cap err-archive-stream-missing 404 "$ZM_URL/api/archive/stream?$(q archive=$A/plain.zip)&$(q path=nope.txt)"
# one-file extract + a full extract + the zip-slip refusal
postj archive-extract-one 200 "$ZM_URL/api/archive/extract" "{\"archive\":\"$A/plain.zip\",\"sources\":[\"top.txt\"],\"dest\":\"$OPSDIR/ex-one\"}"
EXID="$(python3 -c 'import json;print(json.load(open("'"$GOLDEN"'/archive-extract-one.json"))["job_id"])')"
for _ in $(seq 1 40); do
  "$CURL" -fsS "$ZM_URL/api/archive/extract/status?$(q job_id=$EXID)" | grep -q '"done":true' && break
  sleep 0.25
done
get archive-extract-one-status "$ZM_URL/api/archive/extract/status?$(q job_id=$EXID)"
cap err-archive-status-noparam 400 "$ZM_URL/api/archive/extract/status"
postj archive-extract-zipslip 200 "$ZM_URL/api/archive/extract" "{\"archive\":\"$A/zipslip.zip\",\"sources\":[],\"dest\":\"$OPSDIR/ex-slip\"}"
SLIPID="$(python3 -c 'import json;print(json.load(open("'"$GOLDEN"'/archive-extract-zipslip.json"))["job_id"])')"
sleep 1
get archive-extract-zipslip-status "$ZM_URL/api/archive/extract/status?$(q job_id=$SLIPID)"
cap err-archive-status-unknown 404 "$ZM_URL/api/archive/extract/status?$(q job_id=deadbeef)"
# FINDING (quirk 14 / F19): a *relative* dest is NOT rejected. ResolvePath runs
# filepath.Abs, so "relative/path" silently becomes <server cwd>/relative/path -
# validateDest never sees a relative path, and "absoluteness" is therefore
# meaningless.  This is NOT a read-only probe: the extraction WRITES INTO THE
# SERVER'S WORKING DIRECTORY (the directory capture-golden.sh was invoked from,
# $SRVCWD - the server never `cd`s).  That is exactly how `probe-only-not-used/`
# and `relative/` showed up next to the checkout. The response is captured and the
# two directories are removed again at the end of this script (step 14); if the run
# dies before that, delete them by hand before committing anything.
cap archive-extract-relative-dest 200 "$ZM_URL/api/archive/extract" -X POST -H 'Content-Type: application/json' --data "{\"archive\":\"$A/plain.zip\",\"sources\":[],\"dest\":\"./probe-only-not-used\"}"
cap archive-extract-dotdot-falsepositive 403 "$ZM_URL/api/archive/extract" -X POST -H 'Content-Type: application/json' --data "{\"archive\":\"$A/plain.zip\",\"sources\":[],\"dest\":\"$OPSDIR/dotdot..name\"}"
get archive-extracted-tree "$ZM_URL/api/dir?$(q path=$OPSDIR/ex-one)"
# 409 archive_modified: stale cache after the host file changed
"$CURL" -fsS -o /dev/null "$ZM_URL/api/archive/dir?$(q archive=$A/symlink.zip)"
python3 -c "import os,sys;st=os.stat(sys.argv[1]);os.utime(sys.argv[1],(st.st_atime+1,st.st_mtime+1))" "$A/symlink.zip"
cap archive-modified-409  409 "$ZM_URL/api/archive/dir?$(q archive=$A/symlink.zip)"

# ---------------------------------------------------------------------------
# 11. copy the state artefacts out of the disposable HOME
# ---------------------------------------------------------------------------
say "state artefacts"
STATE="$HOME_ROOT/.local/share/zen-man"
[ -f "$STATE/action.log" ] || die "no action.log produced at $STATE/action.log"
cp "$STATE/action.log"     "$GOLDEN/action.log"
cp "$STATE/zen-man.db"     "$GOLDEN/zen-man.db"
cp "$HOME_ROOT/.config/zen-man/config.json" "$GOLDEN/config.json"
cp "$HOME_ROOT/.config/zen-man/search-config.json" "$GOLDEN/search-config.json"
for sidecar in wal shm; do
  [ -e "$STATE/zen-man.db-$sidecar" ] && cp "$STATE/zen-man.db-$sidecar" "$GOLDEN/zen-man.db-$sidecar" || true
done
if [ -f "$USER_REAL_CONFIG" ]; then
  cp "$USER_REAL_CONFIG" "$GOLDEN/legacy-config-real.json"   # read-only copy, never moved
  say "copied the developer's real config.json -> legacy-config-real.json (copy only)"
else
  say "no real config.json at $USER_REAL_CONFIG - legacy-config-real.json not produced"
fi
"$CURL" -fsS "$ZM_URL/api/log?limit=500" > "$FIXTURE_ROOT/log-at-capture.json"

# ---------------------------------------------------------------------------
# 12. binary-log hexdump evidence + capture index
# ---------------------------------------------------------------------------
say "hexdump + INDEX.json"
python3 - "$GOLDEN" "$FIXTURE_ROOT" <<'PY'
import json, os, struct, sys, binascii, pathlib
golden, fixroot = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
raw = (golden / "action.log").read_bytes()
off, recs, out = 0, [], []
while off < len(raw):
    if raw[off:off+4] != b"ZMLG":
        off += 1
        continue
    if off + 26 > len(raw):
        break
    rid, ts, action, status, reclen = (
        struct.unpack(">q", raw[off+4:off+12])[0],
        struct.unpack(">q", raw[off+12:off+20])[0],
        raw[off+20], raw[off+21],
        struct.unpack(">I", raw[off+22:off+26])[0])
    total = reclen + 4
    body = raw[off:off+total]
    ok = len(body) == total and binascii.crc32(body[:-4]) & 0xFFFFFFFF == struct.unpack(">I", body[-4:])[0]
    recs.append({"offset": off, "id": rid, "ts_ns": ts, "action": action, "status": status,
                 "rec_len": reclen, "total_len": total, "crc_ok": ok})
    off += total
(golden / "LOG-RECORDS.json").write_text(json.dumps({
    "file_bytes": len(raw), "record_count": len(recs), "records": recs}, indent=2) + "\n")
# hexdump of the first three records
def hexdump(b, base):
    lines = []
    for i in range(0, len(b), 16):
        ch = b[i:i+16]
        lines.append("%08d  %s  |%s|" % (base+i, " ".join("%02x" % c for c in ch),
                     "".join(chr(c) if 32 <= c < 127 else "." for c in ch)))
    return "\n".join(lines)
end = recs[2]["offset"] + recs[2]["total_len"] if len(recs) >= 3 else len(raw)
for r in recs:
    if not r["crc_ok"]:
        print("WARN: bad CRC on a freshly written record", file=sys.stderr)
(golden / "LOG-HEXDUMP.txt").write_text(hexdump(raw[:end], 0) + "\n")
rows = []
for line in (fixroot / "index.tsv").read_text().splitlines():
    name, code, ct, url, _ = line.split("\t")
    rows.append({"name": name, "http_status": int(code), "content_type": ct,
                 "request": url.replace(os.environ.get("ZM_URL", ""), ""),
                 "file": "tests/golden/%s.json" % name})
# `xtask contracts`' golden-index gate requires every file on disk to appear in INDEX.json, and
# records derived artefacts in a separate `artifacts[]` array (plan §7 step 2c). This table is the
# authority for those artefacts; if you add one, list it here or the gate fails.
ARTIFACTS = [
    ("PROVENANCE.json", "provenance", "Go commit SHA, base URL and capture date"),
    ("LOG-HEXDUMP.txt", "derived-evidence", "hexdump of the first three action.log records"),
    ("LOG-RECORDS.json", "derived-evidence", "decoded record table with byte offsets for all records"),
    ("LOG-PATHS.json", "path-mapping", "original absolute path -> placeholder; action.log stays byte-exact because the CRC covers the payload"),
    ("action.log", "binary-artefact", "the frozen ZMLG log: the codec golden input"),
    ("zen-man.db", "binary-artefact", "the frozen SQLite index: the schema golden"),
    ("config.json", "state-artefact", "config written by the disposable instance"),
    ("search-config.json", "state-artefact", "indexer config as stored on disk"),
    ("legacy-config-real.json", "developer-config-copy", "read-only copy of the real app config, for step 4's migration test"),
    ("archive-stream-plain.txt", "raw-payload", "non-JSON body of /api/archive/stream"),
    ("dir-search-term.json", "unmatched-duplicate-candidate", "provenance unknown: no kept capture differs from it only in a timestamp (checked against all 139 files during step 2); kept until a human verdict, and it is NOT regenerated by this script"),
]
# These five captures are byte-identical to, or differ from a kept capture only in a timestamp field
# (verified with cmp/diff during step 2); they are recorded as superseded and removed here so a
# re-capture cannot resurrect them behind the gate's back.
SUPERSEDED = {
    "err-method-POST.json": "dir-post-returns-listing.json",
    "err-op-empty-sources.json": "op-delete-empty-sources.json",
    "err-perm-denied-exec.json": "dir-perm-noexec.json",
    "index-config-file.json": "search-config.json",
    "err-batch-rename-mismatch.json": "op-single-rename-json-name.json",
}
artifacts = [{"file": "tests/golden/" + n, "kind": k, "note": t} for n, k, t in ARTIFACTS]
artifacts += [{"file": "tests/golden/" + n, "kind": "superseded-by:" + v,
               "note": "duplicate capture, removed in step 2"} for n, v in SUPERSEDED.items()]
for n in SUPERSEDED:
    (golden / n).unlink(missing_ok=True)
(golden / "INDEX.json").write_text(json.dumps(
    {"captures": rows, "artifacts": artifacts, "count": len(rows) + len(artifacts)}, indent=2) + "\n")
print("action.log: %d bytes, %d records, all CRCs ok=%s"
      % (len(raw), len(recs), all(r["crc_ok"] for r in recs)))
PY

# ---------------------------------------------------------------------------
# 13. normalise capture-machine paths
# ---------------------------------------------------------------------------
say "normalise-golden.py"
python3 "$REPO/scripts/normalise-golden.py" --golden "$GOLDEN" \
  --fixture-root "$FIXTURE_ROOT" --home-root "$HOME_ROOT"

say "done"
printf 'golden artefacts in %s\n' "$GOLDEN"
printf 'binary artefacts: action.log=%s bytes  zen-man.db=%s bytes\n' \
  "$(stat -c %s "$GOLDEN/action.log")" "$(stat -c %s "$GOLDEN/zen-man.db")"
# ---------------------------------------------------------------------------
# 14. undo what the relative-dest probe wrote into the server's working directory
# ---------------------------------------------------------------------------
say "cleaning up the relative-dest side effects in $SRVCWD"
for polluted in probe-only-not-used relative; do
  if [ -e "$SRVCWD/$polluted" ]; then
    rm -rf "$SRVCWD/$polluted" && say "removed $SRVCWD/$polluted" \
      || say "WARN: could not remove $SRVCWD/$polluted - delete it by hand"
  fi
done

if [ "$STARTED_OURS" = "1" ]; then
  printf 'note: this script started the instance (pid %s, log %s); it is left running so the capture can be re-run. Stop it with\n      kill %s\n' "$(cat "$SRVPID")" "$SRVLOG" "$(cat "$SRVPID")"
fi
exit 0
