#!/usr/bin/env sh
# Regenerate ui/fixtures/*.json from a real index — one command, after the
# DTOs change:
#
#     ./ui/regenerate-fixtures.sh
#
# The committed fixtures are snapshots of BrowseViewDto produced by
# crates/sagasu-ui/examples/dump_browse_view.rs from a real index (built with
# the real crawler and the real tag engine over a copy of this repository —
# the documented corpus, docs/browse.md §7-2). Nothing here hand-edits JSON:
# if the wire format changes, this script is what the new fixtures come from.
#
# The two states:
#   browse-root.json     the exploration root (empty selection).
#   browse-drilled.json  `kind:code` — one click down from the root. One of
#                        its previewed files is deleted from the corpus
#                        between two dumps, so this state also proves the
#                        `dropped` rendering: the index does not know about
#                        the deletion (no delta source reports one), the
#                        row moves from preview to dropped, and `matched`
#                        stays an upper bound.
#
# ui/fixtures/index.json is not regenerated: it is static plumbing (selection
# key -> fixture file) and only changes when the drilled selection changes,
# which is a deliberate edit, not a DTO-driven one. The script verifies that
# the two dumps still match the map's keys.
#
# Requirements: a Rust toolchain, a built CLI (`cargo build --workspace
# --examples` is run here), python3 (only to pick the deletion victim and to
# verify the dumps). Everything happens in a temporary directory that is
# removed on exit; nothing outside ui/fixtures/ is touched.

set -eu

cd "$(dirname "$0")/.."   # repo root

WORK=$(mktemp -d /tmp/sagasu-fixtures.XXXXXX)
trap 'rm -rf "$WORK"' EXIT HUP INT TERM
CORPUS="$WORK/corpus"
mkdir -p "$CORPUS"

cargo build --workspace --examples >&2

# The corpus: this repository (docs/browse.md §7-2 documents it as a browse
# corpus). `.git` and `target` are the crawler's own default excludes, but
# copying them is wasted work.
cp -a README.md Cargo.toml Cargo.lock crates docs prototypes bench ui "$CORPUS/"

CLI=target/debug/sagasu
DUMP=target/debug/examples/dump_browse_view
DB="$WORK/fixtures.db"

"$CLI" index "$CORPUS" --db "$DB" >/dev/null
"$CLI" tag --db "$DB" >/dev/null

# --- the root state: empty selection ---
"$DUMP" --db "$DB" --out ui/fixtures/browse-root.json

# --- the drilled state: kind:code, with one previewed file deleted ---
"$DUMP" --db "$DB" --out "$WORK/drilled-before.json" kind:code

# Delete the first previewed row of the drilled state from the corpus, then
# dump again: the row moves to `dropped`, `matched` stays an upper bound.
VICTIM=$(python3 - "$WORK/drilled-before.json" <<'PY'
import json, sys
view = json.load(open(sys.argv[1]))
assert view["matched"] > 0 and view["preview"], (
    "kind:code matched nothing — the drilled fixture needs a tag that exists "
    "in this corpus")
print(view["preview"][0]["path"])
PY
)
rm -f -- "$VICTIM"

"$DUMP" --db "$DB" --out ui/fixtures/browse-drilled.json kind:code

# --- verify the committed map still agrees with the dumps ---
python3 - <<'PY'
import json

root = json.load(open("ui/fixtures/browse-root.json"))
drilled = json.load(open("ui/fixtures/browse-drilled.json"))
index = json.load(open("ui/fixtures/index.json"))

assert root["selected"] == [], "browse-root.json must be the empty selection"
assert [t["tag"] for t in drilled["selected"]] == ["kind:code"], (
    "browse-drilled.json must be the kind:code selection")
assert index["[]"] == "browse-root.json", "fixture map root entry drifted"
assert index['["kind:code"]'] == "browse-drilled.json", (
    "fixture map drilled entry drifted")

assert drilled["dropped"], (
    "browse-drilled.json must exercise the dropped rendering — the script "
    "deletes one previewed file between the two dumps")

print("fixtures regenerated:")
print(f"  browse-root.json    : {root['matched']} of {root['corpus']} files, "
      f"{len(root['axes'])} axes shown")
print(f"  browse-drilled.json : {drilled['matched']} matched, "
      f"{len(drilled['preview'])} previewed, {len(drilled['dropped'])} dropped")
PY