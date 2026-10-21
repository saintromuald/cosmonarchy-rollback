#!/usr/bin/env bash
# Install a built CrownLink.snp (the fork carrying the cosmo-rollback channel) into a
# Cosmonarchy build's Starcraft dir, backing up whatever is there.
#
# The stock CrownLink.snp has no `cl_rollback_*` exports, so rollback turns cannot travel
# until this is installed. Build it with the `build-crownlink` workflow in
# `saintromuald/CrownLink` and download the `CrownLink-snp` artifact, or pass a path:
#
#   tools/install-crownlink.sh [path-to-CrownLink.snp] [Build]
#
# Isolation: set COSMO_GAME_ROOT to install into a copy (see tools/headless_probe.sh).
set -euo pipefail

SNP="${1:-}"
BUILD="${2:-Release}"
GAME_ROOT="${COSMO_GAME_ROOT:-$HOME/Games/cosmonarchy}"
DEST_DIR="$GAME_ROOT/prefix/drive_c/Cosmonarchy/Starcraft"
DEST="$DEST_DIR/CrownLink.snp"

if [[ -z "$SNP" ]]; then
    echo "usage: $0 <path-to-CrownLink.snp> [Build]" >&2
    echo "  (download it from the build-crownlink workflow artifact in saintromuald/CrownLink)" >&2
    exit 2
fi
[[ -f "$SNP" ]] || { echo "missing $SNP" >&2; exit 1; }
[[ -d "$DEST_DIR" ]] || { echo "missing Starcraft dir $DEST_DIR" >&2; exit 1; }

# Verify it actually carries the channel before installing it over a working .snp.
if command -v objdump >/dev/null; then
    # Capture first: `grep -q` closes the pipe early, which makes objdump die of SIGPIPE and
    # (with `pipefail`) fails the pipeline even on a match.
    exports="$(objdump -p "$SNP" 2>/dev/null || true)"
    if ! grep -q cl_rollback_abi <<<"$exports"; then
        echo "$SNP does not export cl_rollback_abi — refusing to install" >&2
        exit 1
    fi
fi

if [[ -f "$DEST" && ! -f "$DEST.shipped-backup" ]]; then
    cp "$DEST" "$DEST.shipped-backup"
    echo "backed up: $DEST.shipped-backup"
fi

cp "$SNP" "$DEST"
echo "installed: $DEST"
echo "restore the stock one with: cp \"$DEST.shipped-backup\" \"$DEST\""
