#!/bin/sh
# Imports Slot's cloud sync (the slot-cloud and slot-net crates) from a checkout of Slot, so BananaOS syncs with the
# same rules and the same server folders as Slot on the TrimUI Brick and the RG SP (github.com/machomano11/slot).
#
#   tools/import-slot-cloud.sh <Slot checkout>
#
# Writes system/banana-cloud/slot/ (with Slot's licence, GPL-3.0-or-later) and records the Slot commit in system/banana-cloud/slot/UPSTREAM.
set -e
SRC=${1:?usage: import-slot-cloud.sh <Slot checkout>}
HERE=$(cd "$(dirname "$0")/.." && pwd)
DST=$HERE/system/banana-cloud/slot
for c in slot-cloud slot-net; do [ -f "$SRC/crates/$c/Cargo.toml" ] || { echo "no $SRC/crates/$c"; exit 1; }; done

rm -rf "$DST"
for c in slot-cloud slot-net; do
    mkdir -p "$DST/$c"
    cp "$SRC/crates/$c/Cargo.toml" "$DST/$c/"
    cp -r "$SRC/crates/$c/src" "$DST/$c/src"
done
cp "$SRC/LICENSE" "$DST/LICENSE"

commit=$(git -C "$SRC" rev-parse HEAD 2>/dev/null || echo unknown)
cat > "$DST/UPSTREAM" <<EOT
Slot's cloud sync (crates/slot-cloud and crates/slot-net), imported by tools/import-slot-cloud.sh. Don't edit these
files here: change them in Slot and import again, so every device syncs the same way.
Slot commit: $commit
EOT
