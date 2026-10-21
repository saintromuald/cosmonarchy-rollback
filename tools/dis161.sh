#!/usr/bin/env bash
# Disassemble a range of the shipped stock 1.16.1 exe by virtual address.
#   dis161.sh <hex-vma> <hex-length> [exe]
# File offset = VMA - 0x400000 (.text VMA 0x401000 maps to file offset 0x1000).
set -euo pipefail
A=$((16#${1#0x}))
L=$((16#${2#0x}))
EXE="${3:-$HOME/Games/cosmonarchy/prefix/drive_c/Cosmonarchy/Starcraft/StarCraft.exe}"
dd if="$EXE" bs=1 skip=$((A - 0x400000)) count="$L" 2>/dev/null | ndisasm -b 32 -o "$A" -
