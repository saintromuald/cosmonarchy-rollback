#!/usr/bin/env python3
"""Command-stream generator for the Cosmonarchy desync farm.

Produces per-frame turn files consumed by the rollback plugin's
`CM_ROLLBACK_TURN_STREAM[0/1]` knobs (see crates/cosmo-rollback/src/farm.rs):
one hex line per frame; `05` is the no-op `GameCommand_Sequence`.

Modes
-----
fuzz     Pseudorandom command streams with legal payload lengths (read from the
         stock exe's `command_lengths` table at 0x005005F8). Mostly no-ops with
         occasional Select / RightClick / Train / Build / Upgrade / ability
         turns, so the run exercises command processing and unit paths without
         needing a stateful bot.
capture  Convert a cosmo_rollback.log with `capture #N: frame F turn_len L
         bytes [...]` lines into a stream file (human-played command streams).

Usage
-----
  genstream.py fuzz --seed N --frames N [--density D] [--players 1|2] \
      [--exe PATH] --out DIR
  genstream.py capture --log cosmo_rollback.log --out FILE
"""
import argparse
import random
import re
import struct
import sys

NOP = "05"
STOCK_EXE_DEFAULT = "/dev/null"  # set by caller; resolved below if omitted


# --------------------------------------------------------------------------
# PE section mapping (minimal) so we can read .data tables from the exe.
# --------------------------------------------------------------------------
def pe_rva_to_offset(path, rva):
    with open(path, "rb") as f:
        data = f.read()
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    nsec = struct.unpack_from("<H", data, e_lfanew + 6)[0]
    opt_size = struct.unpack_from("<H", data, e_lfanew + 20)[0]
    sec_off = e_lfanew + 24 + opt_size
    for i in range(nsec):
        hdr = sec_off + i * 40
        name = data[hdr : hdr + 8].rstrip(b"\x00").decode("latin1")
        vsize, vaddr, rawsize, rawoff = struct.unpack_from("<IIII", data, hdr + 8)
        if vaddr <= rva < vaddr + max(vsize, rawsize):
            return rawoff + (rva - vaddr), name
    raise SystemExit(f"rva 0x{rva:x} not in any section of {path}")


def command_lengths(exe_path):
    """Read `command_lengths` ([u32; 0x60], bwdata.h:299) from the stock exe."""
    off, _ = pe_rva_to_offset(exe_path, 0x005005F8 - 0x400000)
    with open(exe_path, "rb") as f:
        f.seek(off)
        raw = f.read(0x60 * 4)
    if len(raw) < 0x60 * 4:
        raise SystemExit(f"short read for command_lengths from {exe_path}")
    return struct.unpack("<%dI" % 0x60, raw)


# --------------------------------------------------------------------------
# fuzz mode
# --------------------------------------------------------------------------
def sel_cmd(rng, idx_hi):
    count = rng.randint(1, 12)
    payload = bytes([count])
    for _ in range(count):
        # Early workers are the realistic selection; garbage is legal too.
        idx = rng.randint(0, 11) if rng.random() < 0.8 else rng.randint(0, idx_hi)
        payload += struct.pack("<H", idx)
    return payload


def fuzz_streams(args):
    lens = command_lengths(args.exe)
    rng = random.Random(args.seed)

    def one_turn(slot):
        if rng.randint(1, args.density) != 1:
            return NOP
        cmd = rng.choice(
            [
                0x09,  # Select
                0x0A,  # ShiftSelect
                0x0B,  # ShiftDeselect
                0x14,  # RightClick
                0x15,  # TargetCommand
                0x0C,  # Build
                0x1F,  # Train
                0x27,  # TrainFighter
                0x30,  # Research
                0x32,  # Upgrade
                0x1A,  # Stop
                0x2B,  # HoldPosition
                0x2C,  # Burrow
                0x2D,  # Unburrow
                0x21,  # Cloak
                0x22,  # Decloak
                0x23,  # UnitMorph
                0x35,  # BuildingMorph
                0x1E,  # ReturnCargo
                0x28,  # UnloadAll
                0x26,  # CancelTrain
            ]
        )
        plen = lens[cmd] if cmd < len(lens) else 0
        if plen == 0 or plen > 64:
            return "%02x" % cmd  # no payload per the engine's table
        payload = bytearray(rng.getrandbits(8) for _ in range(plen - 1))
        if cmd in (0x09, 0x0A, 0x0B):  # selection payloads
            payload = bytearray(sel_cmd(rng, 63))[: plen - 1]
        elif cmd in (0x14, 0x15):  # map coords in the first 4 bytes
            if len(payload) >= 4:
                payload[0:2] = struct.pack("<H", rng.randint(0, 255))
                payload[2:4] = struct.pack("<H", rng.randint(0, 255))
        elif cmd in (0x0C, 0x1F, 0x27):  # unit-type payloads: real + extended ids
            if len(payload) >= 2:
                uid = rng.randint(0, 0xAD7) if rng.random() < 0.5 else rng.randint(0, 227)
                payload[0:2] = struct.pack("<H", uid)
        elif cmd in (0x30, 0x32):  # tech/upgrade ids
            payload[0] = rng.randint(0, 63)
        line = "%02x" % cmd + payload.hex()
        return line

    players = int(args.players)
    for p in range(players):
        out = f"{args.out}/slot{p}.txt"
        with open(out, "w") as f:
            for _ in range(int(args.frames)):
                f.write(one_turn(p) + "\n")
        print(f"wrote {out} ({args.frames} turns, density 1/{args.density}, seed {args.seed})")


# --------------------------------------------------------------------------
# capture mode
# --------------------------------------------------------------------------
def capture_stream(args):
    pat = re.compile(
        r"capture #\d+: frame (\d+) turn_len (\d+) bytes \[((?:[0-9a-fA-F]{2}(?:, )?)*)\]"
    )
    turns = {}
    with open(args.log, errors="replace") as f:
        for line in f:
            m = pat.search(line)
            if not m:
                continue
            frame = int(m.group(1))
            size = int(m.group(2))
            byts = m.group(3).replace(",", "").replace(" ", "")
            if len(byts) != size * 2:
                continue
            turns[frame] = byts
    if not turns:
        raise SystemExit(f"no capture lines found in {args.log}")
    last = max(turns)
    with open(args.out, "w") as f:
        for frame in range(last + 1):
            f.write(turns.get(frame, NOP) + "\n")
    print(f"wrote {args.out}: {len(turns)} turns over {last + 1} frames")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="mode", required=True)

    p = sub.add_parser("fuzz", help="pseudorandom command streams")
    p.add_argument("--seed", type=int, required=True)
    p.add_argument("--frames", type=int, default=40000)
    p.add_argument("--density", type=int, default=600, help="1 command turn per N frames")
    p.add_argument("--players", choices=["1", "2"], default="2")
    p.add_argument("--exe", default=None, help="stock StarCraft.exe (auto-resolved)")
    p.add_argument("--out", required=True)
    p.set_defaults(fn=fuzz_streams)

    p = sub.add_parser("capture", help="turn a capture log into a stream")
    p.add_argument("--log", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(fn=capture_stream)

    args = ap.parse_args()
    if args.mode == "fuzz" and not args.exe:
        # Usual Cosmonarchy layout: <tree>/prefix/drive_c/Cosmonarchy/Starcraft/StarCraft.exe
        import os
        for cand in [
            os.path.expanduser(
                "~/Games/cosmonarchy/prefix/drive_c/Cosmonarchy/Starcraft/StarCraft.exe"
            )
        ]:
            if os.path.exists(cand):
                args.exe = cand
                break
        else:
            raise SystemExit("pass --exe <stock StarCraft.exe>")
    args.fn(args)


if __name__ == "__main__":
    main()
