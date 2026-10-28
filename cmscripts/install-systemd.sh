#!/usr/bin/env bash
# install-systemd.sh — install and enable the cosmo-farm timer on this machine.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT_DIR="$HOME/.config/systemd/user"
mkdir -p "$UNIT_DIR"
cp "$ROOT/cmscripts/systemd/cosmo-farm.service" "$ROOT/cmscripts/systemd/cosmo-farm.timer" "$UNIT_DIR/"
systemctl --user daemon-reload
systemctl --user enable --now cosmo-farm.timer
echo "installed. schedule:"
systemctl --user list-timers cosmo-farm.timer --no-pager || true
echo
echo "the farm runs when the timer fires AND the time is inside FARM_WINDOW"
echo "(default 22:30-08:30; see cmscripts/farm.conf)."
echo "manual: farmctl run-once | start | stop | status | report | tail"
