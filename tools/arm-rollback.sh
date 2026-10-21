#!/usr/bin/env bash
# Arm a Cosmonarchy install for rollback matches: plugin + rollback-capable .snp, checked.
#
#   tools/arm-rollback.sh [Build] [path-to-CrownLink.snp]
#
# Installs `zz-cosmo-rollback.qdp` (release build by default) and, when given, a
# rollback-capable `CrownLink.snp` from the `build-crownlink` workflow. It then verifies what
# matters for a match: the plugin loads, the .snp exports `cl_rollback_abi`, and the .snp
# carries its caps archive (without it Storm never lists the provider).
#
# Rollback needs no AI: Cosmonarchy's CrownLink lobby is human-only (Claimants/Witnesses), so
# an armed match never contains a computer player. AI state (`aise`) is outside the engine
# snapshot, which is why `live` refuses to drive if `first_ai_script` is set.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="${1:-Release}"
SNP="${2:-}"
GAME_ROOT="${COSMO_GAME_ROOT:-$HOME/Games/cosmonarchy}"
BASE="$GAME_ROOT/prefix/drive_c/Cosmonarchy"

[[ -d "$BASE/$BUILD" ]] || { echo "missing $BASE/$BUILD" >&2; exit 1; }

PROFILE=release "$ROOT/tools/build.sh" >/dev/null
PROFILE=release "$ROOT/tools/install-plugin.sh" "$BUILD"

if [[ -n "$SNP" ]]; then
    "$ROOT/tools/install-crownlink.sh" "$SNP" "$BUILD"
else
    echo "note: no .snp given — install one with tools/install-crownlink.sh <snp> $BUILD"
fi

# Verify the transport actually carries the channel.
installed="$BASE/Starcraft/CrownLink.snp"
[[ -f "$installed" ]] || { echo "no CrownLink.snp in $BASE/Starcraft" >&2; exit 1; }
if command -v objdump >/dev/null; then
    exports="$(objdump -p "$installed" 2>/dev/null || true)"
    grep -q cl_rollback_abi <<<"$exports" \
        || { echo "$installed has no rollback channel (install the CI .snp)" >&2; exit 1; }
fi
python3 - "$installed" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
if data.find(b"MPQ\x1a") < 0:
    sys.exit("error: .snp has no caps.mpq appended — Storm will not list the provider")
print(f"transport ok: {len(data)} bytes with caps")
PY

cat <<EOF

armed: $BASE/$BUILD
  plugin:  $BASE/$BUILD/plugins/zz-cosmo-rollback.qdp
  transport: $BASE/Starcraft/CrownLink.snp

At match time set, on each player (host slot 0, guest slot 1):
  CM_ROLLBACK_LIVE=1 CM_ROLLBACK_SLOT=<0|1>
or put the same keys in cosmo_rollback.conf next to the game.
Log: $BASE/$BUILD/cosmo_rollback.log — a healthy match shows
  live: CrownLink rollback channel bound
  live: transport CrownLink (engine applies turns) — injection off
  live rollback #…   and   live: hashes agree …
EOF
