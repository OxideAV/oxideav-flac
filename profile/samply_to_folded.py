#!/usr/bin/env python3
"""Convert a samply ``--unstable-presymbolicate`` profile + sidecar
syms file into the folded-stacks text format flamegraph generators
read on stdin: one line per unique stack, frames separated by ``;``
from root to leaf, followed by a single integer sample count.

Usage:
    samply_to_folded.py <profile.json.gz> <profile.json.syms.json> > out.folded

This is a one-off helper for the round-147 oxideav-flac profile run;
the script is committed under profile/ alongside the SVGs so the
recipe is reproducible later without re-discovering it.
"""
import sys
import json
import gzip
import bisect


def load_profile(profile_path):
    if profile_path.endswith(".gz"):
        with gzip.open(profile_path, "rt") as f:
            return json.load(f)
    return json.load(open(profile_path))


def main():
    if len(sys.argv) != 3:
        sys.stderr.write(__doc__)
        sys.exit(2)
    profile = load_profile(sys.argv[1])
    syms = json.load(open(sys.argv[2]))

    sym_strings = syms["string_table"]
    # debug_id -> list of (rva, size, symbol_name) sorted by rva
    sym_by_dbg = {}
    for entry in syms["data"]:
        dbg = entry["debug_id"]
        table = [
            (s["rva"], s["size"], sym_strings[s["symbol"]])
            for s in entry.get("symbol_table", [])
        ]
        table.sort()
        sym_by_dbg[dbg] = table

    def normalize(s):
        return s.replace("-", "").upper()

    dbg_lookup = {normalize(k): k for k in sym_by_dbg.keys()}

    libs = profile["libs"]
    lib_syms = []
    for lib in libs:
        code = lib.get("codeId") or ""
        key = normalize(code)
        if key in dbg_lookup:
            lib_syms.append(sym_by_dbg[dbg_lookup[key]])
        else:
            lib_syms.append([])

    def lookup(lib_idx, rva):
        table = lib_syms[lib_idx]
        if not table:
            return None
        rvas = [t[0] for t in table]
        i = bisect.bisect_right(rvas, rva) - 1
        if i < 0:
            return None
        start, sz, name = table[i]
        if rva < start + sz:
            return name
        return None

    folded_counts = {}
    for t in profile["threads"]:
        strs = t["stringArray"]
        frames = t["frameTable"]
        stacks = t["stackTable"]
        funcs = t["funcTable"]
        samples = t["samples"]
        resources = t["resourceTable"]
        res_lib = resources["lib"]

        func_name = [strs[funcs["name"][fi]] for fi in range(funcs["length"])]
        func_lib_idx = []
        for fi in range(funcs["length"]):
            rid = funcs["resource"][fi]
            if 0 <= rid < len(res_lib):
                func_lib_idx.append(res_lib[rid])
            else:
                func_lib_idx.append(-1)

        frame_func = frames["func"]
        frame_addr = frames["address"]

        def frame_label(fi):
            fn_name = func_name[frame_func[fi]]
            if fn_name and not fn_name.startswith("0x"):
                return fn_name
            lib_idx = func_lib_idx[frame_func[fi]]
            addr = frame_addr[fi]
            if lib_idx >= 0 and addr >= 0:
                sym = lookup(lib_idx, addr)
                if sym:
                    return sym
            return fn_name or f"addr_0x{addr:x}"

        def stack_frames(si):
            out = []
            while si is not None and si >= 0:
                fi = stacks["frame"][si]
                out.append(frame_label(fi))
                prefix = stacks["prefix"][si]
                si = prefix if prefix is not None else -1
            out.reverse()
            return out

        weights = samples.get("weight") or [1] * samples["length"]
        sample_stacks = samples["stack"]
        for i in range(samples["length"]):
            si = sample_stacks[i]
            if si is None or si < 0:
                continue
            fs = stack_frames(si)
            if not fs:
                continue
            key = ";".join(fs)
            folded_counts[key] = folded_counts.get(key, 0) + int(weights[i] or 1)

    for k, v in sorted(folded_counts.items()):
        sys.stdout.write(f"{k} {v}\n")


if __name__ == "__main__":
    main()
