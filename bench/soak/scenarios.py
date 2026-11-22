"""Soak scenarios S1-S14 (and S0, the probe) and their scale profiles.

Each scenario is a function of the Run (soak.py). Commands that both clients should handle alike
go through Run.each, which diffs their output and exit codes; what a client stored or fetched is
verified with the independent checker (ETag, key sets, metadata) and with SHA-256 of local files.
"""
import collections
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
import zlib

import s3check
from s3check import S3Error
from soak import CAP_BYTES, HERE, MiB, PRIV, THROTTLE_RE, RETRY_RE, Skip, link_bytes, small_bytes

GiB = 1 << 30
PART = 50 * MiB  # both clients' default part size

# Shares are MiB (up, down) of the plan's volume per endpoint: about 80 GiB up and 55 GiB down.
SCALES = {
    "dry": {
        "probe_mib": 256,
        "s1_sizes": [0, 1, 5 * MiB - 1, 5 * MiB, 5 * MiB + 1, PART - 1, PART, PART + 1, 160 * MiB],
        "s3_dirs": 6, "s3_files": 500,
        "s4_files": 140,
        "s9_workers": 32, "s9_keys": 2000, "s9_seconds": 0, "s9_test_stop": True, "s9_test_429": True,
        "s10_slow_mbps": 1.0, "s10_slow_seconds": 60, "s10_file_mib": 1,
        "s13_seconds": 210, "s13_mbps": 5.0, "s13_sizes_mib": [1, 4, 16, 60], "s13_keep": 6,
        "s14_w1": 1000, "s14_w3_mib": 64, "s14_rounds": 2,
        "shares": {"S0": (400, 400), "S1": (1500, 1500), "S2": (20, 20), "S3": (60, 60), "S4": (300, 300),
                   "S5": (40, 40), "S6": (20, 20), "S7": (20, 20), "S8": (20, 20), "S9": (60, 60),
                   "S10": (300, 30), "S11": (60, 20), "S12": (20, 20), "S13": (1200, 700), "S14": (400, 400)},
    },
    "full": {
        "probe_mib": 512,
        "s1_sizes": [0, 1, 5 * MiB - 1, 5 * MiB, 5 * MiB + 1, PART - 1, PART, PART + 1, GiB, 6 * GiB],
        "s3_dirs": 25, "s3_files": 1000,
        "s4_files": 2000,
        "s9_workers": 256, "s9_keys": 100000, "s9_seconds": 60, "s9_test_stop": False, "s9_test_429": False,
        "s10_slow_mbps": 1.0, "s10_slow_seconds": 2400, "s10_file_mib": 4,
        "s13_seconds": 10800, "s13_mbps": 5.0, "s13_sizes_mib": [1, 8, 64, 200], "s13_keep": 12,
        "s14_w1": 10000, "s14_w3_mib": 1024, "s14_rounds": 3,
        "shares": {"S0": (1100, 1100), "S1": (16500, 16500), "S2": (100, 100), "S3": (400, 400),
                   "S4": (2500, 2500), "S5": (200, 200), "S6": (50, 50), "S7": (50, 50), "S8": (50, 50),
                   "S9": (1500, 1500), "S10": (5500, 300), "S11": (100, 50), "S12": (50, 200),
                   "S13": (37000, 19000), "S14": (7000, 7000)},
    },
}

LS_RE = re.compile(r"^(?:\S+ \S+)?\s*(?:DIR|\d+)\s+(.*)$")


def size_name(n):
    for unit, name in ((GiB, "GiB"), (MiB, "MiB")):
        for d in (-1, 0, 1):
            if n >= unit - 1 and (n - d) % unit == 0:
                return f"{(n - d) // unit}{name}" + ("" if d == 0 else f"{d:+d}")
    return f"{n}B"


def req_up(size, part=PART):
    return 1 if size <= part else -(-size // part) + 2


def req_down(size, part=PART):
    return 1 + max(1, -(-size // part))


def tree_ready(root, n, size):
    return os.path.exists(os.path.join(os.path.dirname(root), "." + os.path.basename(root) + f".made-{n}-{size}"))


def make_tree(root, n, size, seed, per_dir=1000):
    if not tree_ready(root, n, size):
        shutil.rmtree(root, ignore_errors=True)
        for i in range(n):
            d = os.path.join(root, f"d{i // per_dir:02d}")
            if i % per_dir == 0:
                os.makedirs(d, exist_ok=True)
            with open(os.path.join(d, f"f{i % per_dir:04d}"), "wb") as f:
                f.write(small_bytes(f"{seed}/{i}", size))
        open(os.path.join(os.path.dirname(root), "." + os.path.basename(root) + f".made-{n}-{size}"), "w").close()
    return root


def local_files(root):
    out = {}
    for d, _, fs in os.walk(root):
        for n in fs:
            p = os.path.join(d, n)
            out[os.path.relpath(p, root)] = os.path.getsize(p)
    return out


def md5_file(path):
    import hashlib
    with open(path, "rb") as f:
        return hashlib.md5(f.read()).hexdigest()


def ls_names(res):
    return [m.group(1) for m in (LS_RE.match(ln) for ln in res.lines) if m]


def copied(res, verb="cp"):
    return sum(1 for ln in res.lines if ln.startswith(verb + " "))


def parity(r, label, outcomes):
    """outcomes: {client: (ok, detail)} for something the clients should do alike, with s5cmd as
    the reference. All fine: a passed check each. All failing: a note (the port matches upstream,
    or the endpoint refuses it). The reference fails where another client does not: a `diff`
    finding (the port does more than upstream). Otherwise a failed check for each client that
    fails."""
    ref = next((c for c in outcomes if c.kind == "s5cmd"), None)
    if all(ok for ok, _ in outcomes.values()):
        for c in outcomes:
            r.check(label, True, c=c)
    elif not any(ok for ok, _ in outcomes.values()):
        r.finding("note", f"{label}: fails for every client", {c.name: str(d)[:300] for c, (_, d) in outcomes.items()})
    elif ref is not None and not outcomes[ref][0]:
        r.finding("diff", f"{label}: {ref.name} does not, " + ", ".join(c.name for c, (ok, _) in outcomes.items() if ok) + " does",
                  {c.name: ("ok" if ok else str(d)[:300]) for c, (ok, d) in outcomes.items()})
        for c, (ok, d) in outcomes.items():
            if c is not ref:
                r.check(label, ok, d, c)
    else:
        for c, (ok, d) in outcomes.items():
            r.check(label, ok, d, c)


def gap(r, c, sub, flag, others):
    have = [o.name for o in others if o is not c and o.has(sub, flag)]
    r.finding("gap", f"`{sub} {flag}` is not in {c.name}'s help" + (f" ({', '.join(have)} has it)" if have else ""),
              client=c.name)
    r.stats[r.scn]["clients"][c.name]["note"] = "flags missing"


# --- S0 --------------------------------------------------------------------------------------

def s0(r):
    """Probe: the first real transfer, with the rate cap checked on the container's counters"""
    f = r.gen("probe.bin", r.scale["probe_mib"] * MiB)
    c = r.clients[0]
    up = r.run(c, ["cp", f, r.url(c, "probe.bin")], "probe upload", est={"put": req_up(os.path.getsize(f))})
    r.verify_upload(c, r.prefix(c, "probe.bin"), f, "probe upload")
    dl = r.work(c, "probe.dl")
    down = r.run(c, ["cp", r.url(c, "probe.bin"), dl], "probe download", est={"get": req_down(os.path.getsize(f))})
    r.verify_download(c, dl, f, "probe download")
    os.unlink(dl)
    ur, dr = up.tx / up.wall, down.rx / down.wall
    r.event("probe", up_bps=round(ur), down_bps=round(dr))
    r.say(f"   probe: {ur / 1e6:.1f} MB/s up, {dr / 1e6:.1f} MB/s down (cap {CAP_BYTES / 1e6:.0f} MB/s)")
    r.check("probe: upload rate at or below the cap", ur <= CAP_BYTES * 1.03, f"{ur / 1e6:.2f} MB/s")
    r.check("probe: download rate at or below the cap", dr <= CAP_BYTES * 1.03, f"{dr / 1e6:.2f} MB/s")
    if r.t.kind == "minio":  # the sidecar is faster than the cap, so the cap must be what limits
        r.check("probe: the cap is the limit against the sidecar (upload)", ur >= CAP_BYTES * 0.7, f"{ur / 1e6:.2f} MB/s")
        r.check("probe: the cap is the limit against the sidecar (download)", dr >= CAP_BYTES * 0.7, f"{dr / 1e6:.2f} MB/s")


# --- S1 --------------------------------------------------------------------------------------

def s1(r):
    """Sizes: 0 B to the largest object, up, down and copied, ETag and SHA-256"""
    files = [(size_name(n), n) for n in r.scale["s1_sizes"]]
    for name, size in files:
        path = r.gen(f"s1/{name}", size)
        tmo = 900 + size // 1_000_000
        r.each(f"up {name}", lambda c: ["cp", path, r.url(c, name)], timeout=tmo, est={"put": req_up(size)})
        for c in r.clients:
            r.verify_upload(c, r.prefix(c, name), path, f"up {name}")
        r.each(f"down {name}", lambda c: ["cp", r.url(c, name), r.work(c, "dl", name)], timeout=tmo,
               est={"get": req_down(size)})
        for c in r.clients:
            r.verify_download(c, r.work(c, "dl", name), path, f"down {name}")
            if os.path.exists(r.work(c, "dl", name)):
                os.unlink(r.work(c, "dl", name))
    # the multipart minimum: 5 MiB parts on the files around 5 MiB
    for name, size in files:
        if abs(size - 5 * MiB) <= 1:
            path = r.src(f"s1/{name}")
            r.each(f"up {name} --part-size 5", lambda c: ["cp", "--part-size", "5", path, r.url(c, "p5", name)],
                   est={"put": req_up(size, 5 * MiB)})
            for c in r.clients:
                r.verify_upload(c, r.prefix(c, "p5", name), path, f"up {name} --part-size 5")
    # cat and pipe
    text = r.src("s1/text.txt")
    with open(text, "w") as f:
        f.write("".join(f"line {i:04d} of the soak text object\n" for i in range(120)))
    r.each("up text", lambda c: ["cp", text, r.url(c, "text.txt")], est={"put": 1})
    res = r.each("cat text", lambda c: ["cat", r.url(c, "text.txt")], est={"get": 1})
    with open(text) as f:
        want = f.read()
    for x in res:
        r.check("cat text: content", x.out == want, f"{len(x.out)} characters, expected {len(want)}", x.client)
    mid = next((r.src(f"s1/{n}") for n, s in files if s == 5 * MiB + 1), None)
    if mid:
        r.each("pipe 5MiB+1", lambda c: ["pipe", r.url(c, "piped")], stdin=mid, est={"put": 3})
        for c in r.clients:
            r.verify_upload(c, r.prefix(c, "piped"), mid, "pipe 5MiB+1")
    # copies inside the bucket: one single-part object, one multipart object, the largest object
    todo = [(n, s) for n, s in files if s in (1, PART + 1)]
    if files[-1] not in todo:
        todo.append(files[-1])
    for name, size in todo:   # above 5 GiB a copy needs UploadPartCopy: a client may not have it
        res = r.each(f"copy {name} in the bucket", lambda c: ["cp", r.url(c, name), r.url(c, "copy", name)],
                     timeout=3600, expect=None, est={"put": req_up(size)})
        for x in res:
            if x.rc == 0:
                r.verify_upload(x.client, r.prefix(x.client, "copy", name), r.src(f"s1/{name}"), f"copy {name}")
        parity(r, f"copy {name} in the bucket: works", {x.client: (x.rc == 0, f"exit {x.rc}: {x.err.strip()[:240]}") for x in res})
    shutil.rmtree(r.src("s1"), ignore_errors=True)


# --- S2 --------------------------------------------------------------------------------------

def s2(r):
    """Key names: space, + % = & ? # ~, unicode, //, leading and trailing /, a 1024-byte key"""
    names = [("space", "with space.txt"), ("plus", "a+b.txt"), ("percent", "100%done.txt"),
             ("percent-enc", "a%20b%2Fc.txt"), ("equals", "k=v.txt"), ("amp", "a&b.txt"), ("question", "what?.txt"),
             ("hash", "a#b.txt"), ("tilde", "~tilde~.txt"), ("accent", "café.txt"),
             ("cjk", "日本/語.txt"), ("double-slash", "dir//file.txt")]
    for i, (tag, _) in enumerate(names + [("long-1024", ""), ("lead", ""), ("trail", "")]):
        with open(r.src(f"s2/{tag}.bin"), "wb") as f:
            f.write(small_bytes(f"s2/{tag}", 257 + i))
    refused = {"minio": {"double-slash", "lead"}}.get(r.t.kind, set())   # key shapes the endpoint itself refuses
    if refused:
        r.finding("endpoint", f"not tested here, this endpoint refuses or rewrites such keys: {', '.join(sorted(refused))}")
    names = [(tag, name) for tag, name in names if tag not in refused]
    for c in r.clients:
        base = r.prefix(c, "k") + "/"
        segs, left = [], 1024 - len(base.encode())
        while left > 0:
            n = min(200, left)
            segs.append("x" * (n - 1) + "/" if left > n else "x" * n)
            left -= n
        mine = names + [("long-1024", "".join(segs))]
        assert len((base + mine[-1][1]).encode()) == 1024
        for tag, name in mine:
            src, key = r.src(f"s2/{tag}.bin"), base + name
            raw = ["--raw"] if "?" in name and c.has("cp", "--raw") else []
            if "?" in name and not raw:
                gap(r, c, "cp", "--raw", r.clients)
            up = r.run(c, ["cp", *raw, src, f"s3://{r.t.bucket}/{key}"], f"up {tag}", est={"put": 1})
            if up.rc == 0:
                r.verify_upload(c, key, src, f"up {tag}")
            dl = r.work(c, "dl", f"{tag}.bin")
            down = r.run(c, ["cp", *raw, f"s3://{r.t.bucket}/{key}", dl], f"down {tag}", expect=None, est={"get": 2})
            ok = down.rc == 0 and os.path.isfile(dl) and md5_file(dl) == md5_file(src)
            what = "there" if os.path.isfile(dl) else f"a directory holding {sorted(local_files(dl))[:3]}" if os.path.isdir(dl) else "missing"
            r.check(f"down {tag}: content", ok, f"exit {down.rc}, destination file {what}; " + down.err.strip()[:200], c)
        want = {name for _, name in mine}
        res = r.run(c, ["ls", f"s3://{r.t.bucket}/{base}*"], "ls all keys", est={"put": 1})
        got = set(ls_names(res))
        r.check("ls all keys: the names", got == want, f"missing {sorted(want - got)[:4]}, extra {sorted(got - want)[:4]}", c)
        r.run(c, ["ls", f"s3://{r.t.bucket}/{base}"], "ls one level", est={"put": 1})
        r.run(c, ["rm", f"s3://{r.t.bucket}/{base}*"], "rm all keys", expect=None)
        left = sorted(k[len(base):] for k in r.remote_keys(base))
        r.check("rm all keys: nothing left", not left, f"left: {left[:5]}", c)
        # a key that starts with "/" (at the bucket root) and a key that ends with "/"
        if "lead" not in refused:
            lead = f"/soak-lead-{r.id}-{c.name}.txt"
            r.note_key(lead)
            r.note_key(lead[1:])
            src = r.src("s2/lead.bin")
            r.run(c, ["cp", src, f"s3://{r.t.bucket}/{lead}"], "up leading-slash key", expect=None, est={"put": 1})
            where = [k for k in (lead, lead[1:]) if r.s3.exists(r.t.bucket, k)]
            r.check("leading-slash key: stored under its exact name", where == [lead], f"found as {where}", c)
            if where:
                r.run(c, ["rm", f"s3://{r.t.bucket}/{where[0]}"], "rm leading-slash key", expect=None)
                r.check("leading-slash key: removed", not r.s3.exists(r.t.bucket, where[0]), "still there", c)
        trail = r.prefix(c, "trail", "marker") + "/"
        with open(r.src("s2/trail.bin"), "rb") as f:
            r.s3.put_object(r.t.bucket, trail, f.read())
        r.run(c, ["ls", f"s3://{r.t.bucket}/{r.prefix(c, 'trail')}/"], "ls trailing-slash key", expect=None, est={"put": 1})
        r.run(c, ["rm", f"s3://{r.t.bucket}/{trail}"], "rm trailing-slash key", expect=None)
        r.stats[r.scn]["clients"][c.name]["trail_gone"] = not r.s3.exists(r.t.bucket, trail)
    parity(r, "trailing-slash key: removed by rm",
           {c: (r.stats[r.scn]["clients"][c.name].pop("trail_gone"), "still there") for c in r.clients})


# --- S3 and S5 share the listing keys -------------------------------------------------------

def keys_prefix(r, c):
    return f"{r.base}/{c.name}/s3/k"


def keys_url(r, c):
    return f"s3://{r.t.bucket}/{keys_prefix(r, c)}"


def upload_keys(r):
    D, F = r.scale["s3_dirs"], r.scale["s3_files"]
    fresh = not tree_ready(r.src("keys"), D * F, 1024)
    tree = make_tree(r.src("keys"), D * F, 1024, "keys", per_dir=F)
    if fresh:
        time.sleep(2.1)   # files older than the second of their upload (see S4, same-second sync)
    r.each("upload the keys", lambda c: ["cp", tree + "/", keys_url(r, c) + "/"], timeout=3600, est={"put": D * F})
    return tree


def s3(r):
    """Listing: many small keys, prefixes and delimiters, wildcards, ListObjects v1, ls and du"""
    D, F = r.scale["s3_dirs"], r.scale["s3_files"]
    n = D * F
    tree = upload_keys(r)
    for c in r.clients:
        keys = r.remote_keys(keys_prefix(r, c) + "/")
        r.check("upload the keys: count", len(keys) == n, f"{len(keys)} of {n}", c)
        r.check("upload the keys: sizes", all(o["size"] == 1024 for o in keys.values()), "a size differs", c)
        bad = [k for i, k in enumerate(sorted(keys)) if i % 211 == 0
               and keys[k]["etag"] != md5_file(os.path.join(tree, k[len(keys_prefix(r, c)) + 1:]))]
        r.check("upload the keys: ETag of a sample", not bad, f"mismatch: {bad[:3]}", c)
    pages = {"put": -(-n // 1000)}

    def lines(label, res, want):
        for x in res:
            r.check(f"{label}: {want} lines", len(x.lines) == want, f"{len(x.lines)} lines", x.client)

    lines("ls prefix", r.each("ls prefix", lambda c: ["ls", keys_url(r, c) + "/"], est={"put": 1}), D)
    lines("ls wildcard", r.each("ls wildcard", lambda c: ["ls", keys_url(r, c) + "/*"], est=pages), n)
    lines("ls one directory", r.each("ls one directory", lambda c: ["ls", keys_url(r, c) + "/d03/"], est={"put": 1}), F)
    rx = re.compile(r"d0.*/f00.7$")
    want = sum(1 for d in range(D) for f in range(F) if rx.match(f"d{d:02d}/f{f:04d}"))
    lines("ls pattern", r.each("ls pattern", lambda c: ["ls", keys_url(r, c) + "/d0*/f00?7"], est=pages), want)
    lines("ls --use-list-objects-v1", r.each("ls --use-list-objects-v1", lambda c: ["ls", keys_url(r, c) + "/*"],
                                             gflags=["--use-list-objects-v1"], est=pages), n)
    res = r.each("ls --json", lambda c: ["ls", keys_url(r, c) + "/d00/*"], gflags=["--json"], est={"put": 1})
    for x in res:
        try:
            ok = len([json.loads(ln) for ln in x.lines]) == F
        except ValueError:
            ok = False
        r.check("ls --json: one JSON object per key", ok, f"{len(x.lines)} lines", x.client)
    for label, args in (("du", []), ("du --group", ["--group"]), ("du --humanize", ["--humanize"])):
        res = r.each(label, lambda c: ["du", *args, keys_url(r, c) + "/*"], est=pages)
        if label == "du":
            for x in res:
                m = re.search(r"(\d+) bytes in (\d+) objects", x.out)
                r.check("du: totals", bool(m) and int(m.group(1)) == n * 1024 and int(m.group(2)) == n,
                        x.out.strip()[:120], x.client)


def s5(r):
    """rm: a wildcard, a prefix, sync --delete, then everything"""
    D, F = r.scale["s3_dirs"], r.scale["s3_files"]
    n = D * F
    if any(not r.s3.exists(r.t.bucket, f"{keys_prefix(r, c)}/d00/f0000") for c in r.clients):
        upload_keys(r)
    tree = r.src("keys")

    def count(c, sub=""):
        return len(r.remote_keys(f"{keys_prefix(r, c)}/{sub}"))

    r.each("rm wildcard", lambda c: ["rm", keys_url(r, c) + "/d00/*"], est={"put": 1})
    for c in r.clients:
        r.check("rm wildcard: one directory gone, the rest there", (count(c, "d00/"), count(c)) == (0, n - F),
                f"{count(c, 'd00/')} left in d00, {count(c)} in all", c)
    r.each("rm prefix", lambda c: ["rm", keys_url(r, c) + "/d01/"], expect=None)
    for c in r.clients:   # upstream refuses a bare prefix ("forgot wildcard character?") and deletes nothing
        r.check("rm of a bare prefix (no wildcard) deletes nothing", count(c, "d01/") == F,
                f"{F - count(c, 'd01/')} of {F} keys under the prefix were deleted", c)
    keep = r.src("keep")
    shutil.rmtree(keep, ignore_errors=True)
    shutil.copytree(os.path.join(tree, "d02"), os.path.join(keep, "d02"), copy_function=shutil.copy2)
    r.each("sync --delete", lambda c: ["sync", "--delete", keep + "/", keys_url(r, c) + "/"], timeout=3600,
           est={"put": D})
    for c in r.clients:
        keys = r.remote_keys(keys_prefix(r, c) + "/")
        r.check("sync --delete: only the kept directory is left", len(keys) == F and all("/d02/" in k for k in keys),
                f"{len(keys)} keys left", c)
    r.each("rm everything", lambda c: ["rm", keys_url(r, c) + "/*"])
    for c in r.clients:
        r.check("rm everything: nothing left", count(c) == 0, f"{count(c)} keys left", c)


# --- S4 --------------------------------------------------------------------------------------

def s4(r):
    """sync: cold, warm, changed, --delete, --size-only, --exclude / --include, and down"""
    N = r.scale["s4_files"]
    sizes = [0, 1, 700, 4096, 65536, 300_000, MiB + 3]

    def rel(i):
        return f"d{i % 7}/sub{i % 3}/f{i:05d}." + ("log" if i % 10 == 0 else "txt" if i % 2 else "bin")

    def put(root, i, gen=0, size=None):
        p = os.path.join(root, rel(i))
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(small_bytes(f"s4/{i}/{gen}", sizes[i % len(sizes)] if size is None else size))

    trees = {}
    for c in r.clients:
        trees[c] = r.work(c, "tree", "x")[:-2]
        for i in range(N):
            put(trees[c], i)

    def remote(c, sub="t"):
        p = r.prefix(c, sub) + "/"
        return {k[len(p):]: o for k, o in r.remote_keys(p).items()}

    def same(c, label, sub="t", want=None):
        want = local_files(trees[c]) if want is None else want
        got = remote(c, sub)
        ok = r.check(f"{label}: remote keys and sizes equal the local tree",
                     {k: o["size"] for k, o in got.items()} == want,
                     f"{len(got)} remote, {len(want)} local; missing {sorted(set(want) - set(got))[:3]}, "
                     f"extra {sorted(set(got) - set(want))[:3]}", c)
        bad = [k for k in sorted(want) if k in got and got[k]["etag"] != md5_file(os.path.join(trees[c], k))]
        return r.check(f"{label}: ETags equal the local MD5s", not bad, f"{len(bad)} differ: {bad[:3]}", c) and ok

    def sync(label, flags=(), sub="t", **kw):
        return r.each(label, lambda c: ["sync", *flags, trees[c] + "/", r.url(c, sub) + "/"], **kw)

    settle = 2.1   # a file written in the second of its upload looks newer than its copy (a known sync limit)
    time.sleep(settle)
    for x in sync("sync cold", est={"put": N + 1}):
        r.check("sync cold: one copy per file", copied(x) == N, f"{copied(x)} of {N}", x.client)
        same(x.client, "sync cold")
    res = sync("sync warm", est={"put": 1})
    parity(r, "sync warm: copies nothing", {x.client: (copied(x) == 0, f"{copied(x)} copies") for x in res})
    time.sleep(1.2)
    A, B, M, Dl = range(1, 11), range(11, 21), range(N, N + 5), range(21, 26)
    for c in r.clients:
        for i in A:
            put(trees[c], i, gen=1)
        for i in B:
            put(trees[c], i, gen=1, size=sizes[i % len(sizes)] + 17)
        for i in M:
            put(trees[c], i)
        for i in Dl:
            os.unlink(os.path.join(trees[c], rel(i)))
    time.sleep(settle)
    want_n = len(A) + len(B) + len(M)
    for x in sync("sync changed", est={"put": want_n + 1}):
        r.check("sync changed: copies the changed and the new files", copied(x) == want_n, f"{copied(x)} of {want_n}", x.client)
        got = remote(x.client)
        r.check("sync changed: files deleted locally stay remote", all(rel(i) in got for i in Dl), "a key is gone", x.client)
        bad = [rel(i) for i in [*A, *B, *M] if got.get(rel(i), {}).get("etag") != md5_file(os.path.join(trees[x.client], rel(i)))]
        r.check("sync changed: new content is remote", not bad, f"{bad[:3]}", x.client)
    for x in sync("sync --delete", flags=["--delete"], est={"put": 2}):
        same(x.client, "sync --delete")
    time.sleep(1.2)
    C = range(31, 39)
    old = {c: {rel(i): md5_file(os.path.join(trees[c], rel(i))) for i in C} for c in r.clients}
    for c in r.clients:
        for i in C:
            put(trees[c], i, gen=2)
    time.sleep(settle)
    for x in sync("sync --size-only", flags=["--size-only"], est={"put": 1}):
        got = remote(x.client)
        r.check("sync --size-only: same-size changes are not copied",
                copied(x) == 0 and all(got[k]["etag"] == v for k, v in old[x.client].items()), f"{copied(x)} copies", x.client)
    for x in sync("sync after --size-only", est={"put": len(C) + 1}):
        r.check("sync after --size-only: the changed files are copied", copied(x) == len(C), f"{copied(x)} of {len(C)}", x.client)
        same(x.client, "sync after --size-only")
    for label, flags, sub, keep in (("sync --exclude", ["--exclude", "*.log"], "x", lambda k: not k.endswith(".log")),
                                    ("sync --include", ["--include", "*.txt"], "i", lambda k: k.endswith(".txt"))):
        res = sync(label, flags=flags, sub=sub, expect=None, est={"put": N})
        outcomes = {}
        for x in res:
            want = {k for k in local_files(trees[x.client]) if keep(k)}
            got = set(remote(x.client, sub))
            outcomes[x.client] = (x.rc == 0 and got == want, f"exit {x.rc}, {len(got)} keys, expected {len(want)}")
        parity(r, f"{label}: the right keys", outcomes)
    # The known limit made visible: a file written in the same second as its upload looks newer than
    # its copy afterwards (remote times have whole seconds), so the next sync may copy it again.
    outcomes = {}
    for c in r.clients:
        while time.time() % 1 > 0.15:
            time.sleep(0.02)
        for i in range(N + 10, N + 30):
            put(trees[c], i, size=512)
        first = r.run(c, ["sync", trees[c] + "/", r.url(c, "t") + "/"], "sync in the second of the write", est={"put": 21})
        again = r.run(c, ["sync", trees[c] + "/", r.url(c, "t") + "/"], "sync again right after", est={"put": 1})
        outcomes[c] = (copied(first) == 20 and copied(again) == 0, f"{copied(first)} then {copied(again)} copies")
    parity(r, "sync right after a same-second sync copies nothing", outcomes)
    time.sleep(settle)
    # down: into a directory that does not exist yet, then again
    res = r.each("sync down (new directory)", lambda c: ["sync", r.url(c, "t") + "/*", r.work(c, "down", "x")[:-2] + "/"],
                 expect=None, est={"get": N, "put": 1})
    outcomes = {}
    for x in res:
        d = r.work(x.client, "down", "x")[:-2]
        want, got = local_files(trees[x.client]), local_files(d) if os.path.isdir(d) else {}
        same_bytes = want == got and all(md5_file(os.path.join(d, k)) == md5_file(os.path.join(trees[x.client], k)) for k in want)
        outcomes[x.client] = (x.rc == 0 and same_bytes, f"exit {x.rc}, {len(got)} of {len(want)} files; {x.err.strip()[:160]}")
    parity(r, "sync down (new directory): the tree arrives intact", outcomes)
    res = r.each("sync down again", lambda c: ["sync", r.url(c, "t") + "/*", r.work(c, "down", "x")[:-2] + "/"],
                 expect=None, est={"put": 1})
    parity(r, "sync down again: copies nothing", {x.client: (x.rc == 0 and copied(x) == 0, f"exit {x.rc}, {copied(x)} copies")
                                                  for x in res})


# --- S6 --------------------------------------------------------------------------------------

def s6(r):
    """Metadata and options: content headers, storage classes, SSE, request payer, ACL, select"""
    f = r.src("s6/obj.bin")
    with open(f, "wb") as fh:
        fh.write(small_bytes("s6", 1024))
    cases = [
        ("content-type", ["--content-type", "text/x-soak"], lambda h: h.get("content-type") == "text/x-soak"),
        ("cache-control", ["--cache-control", "max-age=3600, public"], lambda h: h.get("cache-control") == "max-age=3600, public"),
        ("expires", ["--expires", "2030-01-02T03:04:05Z"], lambda h: "2030" in h.get("expires", "")),
        ("content-encoding", ["--content-encoding", "gzip"], lambda h: "gzip" in h.get("content-encoding", "")),
        ("content-disposition", ["--content-disposition", 'attachment; filename="soak.bin"'],
         lambda h: h.get("content-disposition") == 'attachment; filename="soak.bin"'),
        ("sse AES256", ["--sse", "AES256"], lambda h: h.get("x-amz-server-side-encryption") == "AES256"),
        ("sse aws:kms", ["--sse", "aws:kms"], lambda h: h.get("x-amz-server-side-encryption") == "aws:kms"),
    ] + [(f"storage-class {sc}", ["--storage-class", sc], lambda h, sc=sc: h.get("x-amz-storage-class", "STANDARD") == sc)
         for sc in r.t.storage_classes]
    for tag, flags, pred in cases:
        outcomes, results = {}, []
        for c in r.clients:
            key = tag.replace(" ", "-").replace(":", "-")
            if c.has("cp", flags[0]):
                x = r.run(c, ["cp", *flags, f, r.url(c, key)], tag, expect=None, est={"put": 1})
            elif c.has("pipe", flags[0]):
                gap(r, c, "cp", flags[0], r.clients)
                x = r.run(c, ["pipe", *flags, r.url(c, key)], f"{tag} (pipe)", stdin=f, expect=None, est={"put": 1})
            else:
                gap(r, c, "cp", flags[0], r.clients)
                continue
            results.append(x)
            detail = f"exit {x.rc}: {x.err.strip()[:200]}"
            ok = x.rc == 0
            if ok:
                try:
                    h = r.s3.head_object(r.t.bucket, r.prefix(c, key))
                    ok = pred(h)
                    detail = "header not as asked: " + json.dumps({k: v for k, v in h.items() if k.startswith(
                        ("content-", "cache-", "expires", "x-amz-s"))})[:300]
                except S3Error as e:
                    ok, detail = False, f"exit 0 but HEAD says {e}"
            outcomes[c] = (ok, detail)
        if outcomes:
            parity(r, f"{tag}: stored as asked", outcomes)
    # --request-payer on a bucket that is not requester-pays: the request must still work
    res = r.each("request-payer cp", lambda c: ["cp", f, r.url(c, "payer")], gflags=["--request-payer", "requester"],
                 expect=None, est={"put": 1})
    parity(r, "request-payer: the upload works", {x.client: (x.rc == 0 and r.s3.exists(r.t.bucket, r.prefix(x.client, "payer")),
                                                         f"exit {x.rc}: {x.err.strip()[:200]}") for x in res})
    r.each("request-payer ls", lambda c: ["ls", r.url(c, "payer")], gflags=["--request-payer", "requester"], expect=None,
           est={"put": 1})
    # --acl: with ACLs disabled the bucket must refuse it; nothing may become public
    res = r.each("acl public-read", lambda c: ["cp", "--acl", "public-read", f, r.url(c, "acl")], expect=None, est={"put": 1})
    for x in res:
        key = r.prefix(x.client, "acl")
        if r.t.kind == "aws":
            r.check("acl: refused with AccessControlListNotSupported",
                    x.rc != 0 and "AccessControlListNotSupported" in x.err, f"exit {x.rc}: {x.err.strip()[:200]}", x.client)
        else:
            r.finding("endpoint", f"--acl public-read: exit {x.rc} on this endpoint", x.err.strip()[:200], x.client.name)
        if r.s3.exists(r.t.bucket, key):
            status = r.s3.request("GET", r.t.bucket, key, unsigned=True, ok=None)[0]
            r.check("acl: the object is not readable without credentials", status in (401, 403), f"status {status}", x.client)
    # S3 Select, only where the endpoint has it
    sel = "".join(json.dumps({"id": i, "name": f"row{i}"}) + "\n" for i in range(50)).encode()
    for c in r.clients:
        r.s3.put_object(r.t.bucket, r.prefix(c, "select.jsonl"), sel)
    res = r.each("select", lambda c: ["select", "--query", "SELECT s.id FROM S3Object s WHERE s.id < 3", r.url(c, "select.jsonl")],
                 expect=None, est={"get": 1})
    if all(x.rc != 0 for x in res):
        r.finding("note", "S3 Select: not available on this endpoint (or for every client)",
                  {x.client.name: x.err.strip()[:200] for x in res})
    else:
        for x in res:
            r.check("select: three rows", x.rc == 0 and len(x.lines) == 3, f"exit {x.rc}, {len(x.lines)} lines; {x.err.strip()[:160]}",
                    x.client)


# --- S7 --------------------------------------------------------------------------------------

def s7(r):
    """Versions, only on the scratch bucket <bucket>-mb-versions: mb to rb"""
    vb = r.t.bucket + "-mb-versions"
    r.note_bucket("-mb-versions")
    u = f"s3://{vb}"
    v1, v2 = r.src("s7/v1.bin"), r.src("s7/v2.bin")
    for p, n in ((v1, 2000), (v2, 3111)):
        with open(p, "wb") as f:
            f.write(small_bytes(p[-6:], n))
    if r.s3.head_bucket(vb) == 200:
        r.finding("note", "the versions scratch bucket existed before S7: emptied and removed by the checker")
        for i in range(0, 10**6, 1000):
            items = [(v["key"], v["version_id"]) for v in r.s3.list_versions(vb)][:1000]
            if not items:
                break
            r.s3.delete_objects(vb, items)
        r.s3.delete_bucket(vb)
    per = collections.defaultdict(list)
    for c in r.clients:
        def run(args, label, **kw):
            x = r.run(c, args, label, **kw)
            per[label].append(x)
            return x

        for attempt in range(1, 9):   # the name of a bucket deleted a moment ago may not be free yet
            x = run(["mb", u], "mb", expect=None, est={"put": 1})
            if x.rc == 0 or not re.search(r"OperationAborted|conflicting|BucketAlreadyExists", x.err):
                break
            per["mb"].pop()
            time.sleep(15)
        if not r.check("mb: the bucket exists", r.s3.head_bucket(vb) == 200, f"exit {x.rc} after {attempt} tries: {x.err.strip()[:200]}", c):
            continue
        run(["bucket-version", "--set", "Enabled", u], "bucket-version --set Enabled", est={"put": 1})
        r.check("bucket-version: Enabled", r.s3.get_bucket_versioning(vb) == "Enabled", r.s3.get_bucket_versioning(vb), c)
        x = run(["bucket-version", u], "bucket-version", est={"get": 1})
        r.check("bucket-version: reports Enabled", "nabled" in x.out, x.out.strip()[:100], c)
        run(["cp", v1, f"{u}/obj"], "put version 1", est={"put": 1})
        run(["cp", v2, f"{u}/obj"], "put version 2", est={"put": 1})
        vers = r.s3.list_versions(vb, "obj")
        if not r.check("two versions exist", len(vers) == 2 and not any(v["delete_marker"] for v in vers), f"{len(vers)} entries", c):
            continue
        first = next(v["version_id"] for v in vers if not v["is_latest"])
        x = run(["ls", "--all-versions", f"{u}/obj"], "ls --all-versions", est={"put": 1})
        r.check("ls --all-versions: shows both version ids", all(v["version_id"] in x.out for v in vers), x.out.strip()[:200], c)
        dl = r.work(c, "v1.dl")
        run(["cp", "--version-id", first, f"{u}/obj", dl], "cp --version-id", est={"get": 1})
        r.verify_download(c, dl, v1, "cp --version-id")
        run(["rm", f"{u}/obj"], "rm (adds a delete marker)")
        vers = r.s3.list_versions(vb, "obj")
        r.check("rm: a delete marker on top of two versions", len(vers) == 3 and sum(v["delete_marker"] for v in vers) == 1,
                f"{len(vers)} entries", c)
        if c.has("rm", "--all-versions"):
            run(["rm", "--all-versions", f"{u}/obj"], "rm --all-versions")
        else:
            gap(r, c, "rm", "--all-versions", r.clients)
            for v in vers:
                run(["rm", "--version-id", v["version_id"], f"{u}/obj"], "rm --version-id")
        left = r.s3.list_versions(vb)
        r.check("all versions removed", not left, f"{len(left)} entries left", c)
        if left:
            r.s3.delete_objects(vb, [(v["key"], v["version_id"]) for v in left])
        run(["rb", u], "rb")
        r.check("rb: the bucket is gone", r.s3.head_bucket(vb) == 404, f"HEAD says {r.s3.head_bucket(vb)}", c)
    for label, xs in per.items():
        r.compare(label, xs, streams=() if "version" in label or label == "rm (adds a delete marker)" else ("out", "err"))


# --- S8 --------------------------------------------------------------------------------------

def s8(r):
    """Errors: NoSuchKey, NoSuchBucket, AccessDenied, wrong region"""
    f = r.src("s8/obj.bin")
    with open(f, "wb") as fh:
        fh.write(small_bytes("s8", 100))
    nb = f"s3://{r.t.bucket}-mb-nosuch-{zlib.crc32(r.id.encode()) % 10**8}"
    cases = [("cat a missing key", lambda c: ["cat", r.url(c, "no-such-key")], r"NoSuchKey|404|[Nn]ot [Ff]ound", {"get": 1}),
             ("cp a missing key", lambda c: ["cp", r.url(c, "no-such-key"), r.work(c, "none.dl")], r"NoSuchKey|404|[Nn]ot [Ff]ound", {"get": 1}),
             ("head a missing key", lambda c: ["head", r.url(c, "no-such-key")], r"NoSuchKey|404|[Nn]ot [Ff]ound", {"get": 1}),
             ("ls a missing bucket", lambda c: ["ls", nb + "/"], r"NoSuchBucket|404", {"put": 1}),
             ("cp into a missing bucket", lambda c: ["cp", f, nb + "/x"], r"NoSuchBucket|404", {"put": 1})]
    if r.t.denied_bucket:
        cases.append(("ls a bucket outside the policy", lambda c: ["ls", f"s3://{r.t.denied_bucket}/"], r"AccessDenied|403", {"put": 1}))
    else:
        r.finding("note", "AccessDenied on a bucket outside the policy not tested: SOAK_DENIED_BUCKET is not set")
    for label, fn, pat, est in cases:
        for x in r.each(label, fn, expect=None, est=est):
            r.check(f"{label}: fails", x.rc != 0, "exit 0", x.client)
            if x.rc != 0 and not re.search(pat, x.err + x.out):
                r.finding("note", f"{label}: the error does not name the cause", x.err.strip()[:200], x.client.name)
            r.check(f"{label}: no file left behind", not os.path.exists(r.work(x.client, "none.dl")), "a file was written", x.client)
    res = r.each("ls without arguments", lambda c: ["ls"], expect=None, est={"get": 1})
    if r.t.kind == "aws":   # the soak user may not list buckets there; RGW and MinIO show a user its own buckets
        for x in res:
            r.check("ls without arguments: AccessDenied", x.rc != 0 and re.search(r"AccessDenied|403", x.err + x.out) is not None,
                    f"exit {x.rc}: {(x.err + x.out).strip()[:160]}", x.client)
    wrong = "us-west-2" if r.t.region != "us-west-2" else "eu-west-1"
    for c in r.clients:
        r.s3.put_object(r.t.bucket, r.prefix(c, "region.bin"), b"region")
    res = r.each("ls with the wrong region", lambda c: ["ls", r.url(c, "region.bin")], expect=None,
                 env={"AWS_REGION": wrong, "AWS_DEFAULT_REGION": wrong}, est={"put": 2})
    for x in res:
        r.finding("note", f"wrong region ({'works' if x.rc == 0 else 'fails'}): exit {x.rc}", (x.err + x.out).strip()[:200], x.client.name)


# --- S9 --------------------------------------------------------------------------------------

def s9(r):
    """Throttling: many workers of 1 KiB PUTs and GETs on one prefix; 503 SlowDown retried to success"""
    sc, t = r.scale, r.t
    workers, stop_first = sc["s9_workers"], False
    if t.kind == "rgw":   # a shared cluster: 32 workers, stop at the first 503
        workers, stop_first = min(workers, 32), True
    env, proxy, stats = None, None, os.path.join(r.scratch, "tmp", "proxy.json")
    os.makedirs(os.path.dirname(stats), exist_ok=True)
    if t.throttle_endpoint:   # dry run: a local proxy that answers some requests with 503 SlowDown
        host, port = t.throttle_endpoint.split("://")[1].split(":")
        proxy = subprocess.Popen([sys.executable, os.path.join(HERE, "throttle_proxy.py"), "--listen", f"{host}:{port}",
                                  "--upstream", t.endpoint.split("://")[1], "--every", "40", "--stats", stats])
        time.sleep(1.0)
        env = {"S3_ENDPOINT_URL": t.throttle_endpoint, "AWS_ENDPOINT_URL": t.throttle_endpoint}
    watch = re.compile("|".join(p.pattern for p in RETRY_RE.values())) if stop_first else THROTTLE_RE
    gf = ["--numworkers", str(workers)]
    try:
        K = sc["s9_keys"]
        if sc["s9_seconds"]:   # size the run for about s9_seconds per phase from a short measurement
            cal = make_tree(r.src("s9cal"), 2000, 1024, "s9cal")
            x = r.run(r.clients[0], ["cp", cal + "/", r.url(r.clients[0], "cal") + "/"], "calibrate", gflags=gf, env=env,
                      debug=True, watch=watch, stop_on_watch=stop_first, expect=None, est={"put": 2000})
            if stop_first and x.hit:
                r.finding("endpoint", f"S9 stopped at the first retry during calibration: {x.hit}", x.err.strip()[-300:])
                return
            K = int(min(K, max(2000, 2000 / x.wall * sc["s9_seconds"])) // 1000 * 1000)
            r.say(f"   S9: {2000 / x.wall:.0f} PUT/s measured, {K} keys per phase")
        tree = make_tree(r.src("s9"), K, 1024, "s9")
        for c in r.clients:
            up = r.run(c, ["cp", tree + "/", r.url(c, "k") + "/"], "put 1 KiB objects", gflags=gf, env=env, debug=True,
                       watch=watch, stop_on_watch=stop_first, expect=None, timeout=3600, est={"put": K})
            if stop_first and up.hit:
                r.finding("endpoint", f"S9 stopped at the first retry ({up.hit}) after {up.wall:.0f} s of PUTs with {workers} workers",
                          up.err.strip()[-300:], c.name)
                return
            n = len(r.remote_keys(r.prefix(c, "k") + "/"))
            r.check("put: every object is there, exit 0", up.rc == 0 and n == K, f"exit {up.rc}, {n} of {K} keys; {up.err.strip()[-200:]}", c)
            dl = r.work(c, "dl", "x")[:-2]
            down = r.run(c, ["cp", r.url(c, "k") + "/*", dl + "/"], "get 1 KiB objects", gflags=gf, env=env, debug=True,
                         watch=watch, stop_on_watch=stop_first, expect=None, timeout=3600, est={"get": K, "put": K // 1000 + 1})
            if stop_first and down.hit:
                r.finding("endpoint", f"S9 stopped at the first retry ({down.hit}) after {down.wall:.0f} s of GETs with {workers} workers",
                          down.err.strip()[-300:], c.name)
                return
            got = local_files(dl) if os.path.isdir(dl) else {}
            bad = [k for i, k in enumerate(sorted(got)) if i % 97 == 0 and md5_file(os.path.join(dl, k)) != md5_file(os.path.join(tree, k))]
            r.check("get: every object arrived intact, exit 0", down.rc == 0 and len(got) == K and not bad,
                    f"exit {down.rc}, {len(got)} of {K} files, {len(bad)} of the sample differ; {down.err.strip()[-200:]}", c)
            shutil.rmtree(dl, ignore_errors=True)
            r.event("throttle", client=c.name, put_per_s=round(K / up.wall), get_per_s=round(K / down.wall),
                    put_retries=up.retries, put_throttled=up.throttled, get_retries=down.retries, get_throttled=down.throttled)
            r.say(f"   S9 {c.name}: {K / up.wall:.0f} PUT/s ({up.retries} retries, {up.throttled} throttled), "
                  f"{K / down.wall:.0f} GET/s ({down.retries} retries, {down.throttled} throttled)")
            if proxy:
                r.check("the injected 503s were seen and retried", up.throttled > 0 and down.throttled > 0,
                        f"{up.throttled} on PUT, {down.throttled} on GET", c)
        if proxy and sc["s9_test_stop"]:   # dry run only: the stop-at-the-first-503 path used on a shared cluster
            stop = re.compile("|".join(p.pattern for p in RETRY_RE.values()))
            for c in r.clients:
                x = r.run(c, ["cp", tree + "/", r.url(c, "stop") + "/"], "stop at the first 503", gflags=gf, env=env,
                          debug=True, watch=stop, stop_on_watch=True, expect=None, est={"put": 100})
                n = len(r.remote_keys(r.prefix(c, "stop") + "/"))
                r.check("stop at the first 503: the retry was seen and the client stopped early",
                        x.hit is not None and n < K, f"hit {x.hit}, exit {x.rc}, {n} of {K} keys uploaded", c)
                r.say(f"   S9 {c.name}: stopped on '{x.hit}' after {x.wall:.1f} s with {n} of {K} keys uploaded")
        if proxy and sc["s9_test_429"]:   # dry run only: some S3-compatible servers throttle with 429 TooManyRequests
            p2 = subprocess.Popen([sys.executable, os.path.join(HERE, "throttle_proxy.py"), "--listen", f"{host}:{int(port) + 1}",
                                   "--upstream", t.endpoint.split("://")[1], "--every", "25", "--status", "429",
                                   "--code", "TooManyRequests"])
            time.sleep(1.0)
            url2 = f"http://{host}:{int(port) + 1}"
            small = make_tree(r.src("s9small"), 500, 1024, "s9small")
            try:
                res = r.each("put through 429 TooManyRequests", lambda c: ["cp", small + "/", r.url(c, "k429") + "/"], gflags=gf,
                             env={"S3_ENDPOINT_URL": url2, "AWS_ENDPOINT_URL": url2}, debug=True, expect=None, compare=False,
                             est={"put": 500})
            finally:
                p2.terminate()
                p2.wait()
            parity(r, "429 TooManyRequests is retried to success", {
                x.client: (x.rc == 0 and len(r.remote_keys(r.prefix(x.client, "k429") + "/")) == 500,
                           f"exit {x.rc}, {len(r.remote_keys(r.prefix(x.client, 'k429') + '/'))} of 500 keys, {x.retries} retries seen; "
                           + x.err.strip()[:200]) for x in res})
    finally:
        if proxy:
            proxy.terminate()
            proxy.wait()
            try:
                with open(stats) as f:
                    r.event("throttle_proxy", **json.load(f))
            except (OSError, ValueError):
                pass


# --- S10 -------------------------------------------------------------------------------------

def s10(r):
    """Credentials: environment keys, a profile, a credentials file, and a slow sync (with the role, if any)"""
    t, sc = r.t, r.scale
    f = r.src("s10/obj.bin")
    with open(f, "wb") as fh:
        fh.write(small_bytes("s10", 2048))
    d = f"{PRIV}/aws"
    os.makedirs(d, mode=0o700, exist_ok=True)
    cred, conf = f"{d}/credentials", f"{d}/config"
    fd = os.open(cred, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write(f"[soak-prof]\naws_access_key_id = {t.ak}\naws_secret_access_key = {t.sk}\n"
                 + (f"aws_session_token = {t.token}\n" if t.token else ""))
    fd = os.open(conf, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write(f"[profile soak-prof]\nregion = {t.region}\n")
        if t.role_arn:
            fh.write(f"[profile soak-role]\nrole_arn = {t.role_arn}\nsource_profile = soak-prof\n"
                     f"duration_seconds = 900\nrole_session_name = rs5cmd-soak\nregion = {t.region}\n")
    nokeys = {"AWS_ACCESS_KEY_ID": None, "AWS_SECRET_ACCESS_KEY": None, "AWS_SESSION_TOKEN": None}
    files = {**nokeys, "AWS_SHARED_CREDENTIALS_FILE": cred, "AWS_CONFIG_FILE": conf, "AWS_SDK_LOAD_CONFIG": "1"}

    def put(label, name, **kw):
        res = r.each(label, lambda c: ["cp", f, r.url(c, name)], est={"put": 1}, **kw)
        for x in res:
            if x.rc == 0:
                r.verify_upload(x.client, r.prefix(x.client, name), f, label)

    put("environment keys: cp", "env.bin")
    put("profile: cp", "prof.bin", gflags=["--profile", "soak-prof"], env=files)
    put("credentials file flag: cp", "credfile.bin", gflags=["--credentials-file", cred, "--profile", "soak-prof"],
        env={**nokeys, "AWS_CONFIG_FILE": conf})
    r.each("no credentials: must fail", lambda c: ["ls", r.url(c, "env.bin")], env=nokeys, expect="fail", est={"get": 1})
    # A sync slowed down by the rate limit, long enough to outlive the role's sessions
    seconds, mbps, fsize = sc["s10_slow_seconds"], sc["s10_slow_mbps"], sc["s10_file_mib"] * MiB
    n = max(4, int(seconds * mbps * 1_000_000 / fsize))
    tree = r.src("s10/slow")
    for i in range(n):
        r.gen(f"s10/slow/f{i:04d}.bin", fsize)
    profile = "soak-role" if t.role_arn else "soak-prof"
    if not t.role_arn:
        r.finding("note", "no AWS_ROLE_ARN for this target: the slow sync ran with the profile's static keys, "
                          "so credential refresh was not tested")
    r.set_rate(mbps)
    try:
        res = r.each("slow sync", lambda c: ["sync", tree + "/", r.url(c, "slow") + "/"], env=files, compare=False,
                     gflags=["--profile", profile, "--numworkers", "4"], debug=True, timeout=seconds * 4 + 900,
                     est={"put": n + 1})
    finally:
        r.set_rate(CAP_BYTES / 1_000_000)
    for x in res:
        keys = r.remote_keys(r.prefix(x.client, "slow") + "/")
        r.check("slow sync: every file is there", len(keys) == n and all(o["size"] == fsize for o in keys.values()),
                f"{len(keys)} of {n}", x.client)
        r.check("slow sync: no failed request", x.rc == 0 and not re.search(r"(?im)^error|ExpiredToken|InvalidToken|AccessDenied", x.err),
                x.err.strip()[:300], x.client)
        r.check(f"slow sync: lasted about {seconds} s under the lowered rate", x.wall >= seconds * 0.8,
                f"{x.wall:.0f} s at {x.tx / x.wall / 1e6:.2f} MB/s", x.client)
        r.say(f"   S10 {x.client.name}: slow sync {x.wall:.0f} s, {x.tx / x.wall / 1e6:.2f} MB/s, {x.retries} retries")
    shutil.rmtree(tree, ignore_errors=True)


# --- S11 -------------------------------------------------------------------------------------

def s11(r):
    """Checksums: what the endpoint checks (CRC32C header, aws-chunked trailer, BadDigest) and what the clients store"""
    s3, b = r.s3, r.t.bucket
    body = small_bytes("s11", 3000)
    good, bad = s3check.checksums(body)["crc32c"], s3check.checksums(body + b"x")["crc32c"]
    base = f"{r.base}/checker/s11/"
    hdr = {"x-amz-sdk-checksum-algorithm": "CRC32C"}
    st, _, _ = s3.request("PUT", b, base + "header-good", body=body, headers={**hdr, "x-amz-checksum-crc32c": good}, ok=None)
    r.check("endpoint: a PUT with a correct CRC32C header is accepted", st == 200, f"status {st}")
    if st == 200:
        got = s3.head_object(b, base + "header-good", checksum=True).get("x-amz-checksum-crc32c")
        if got != good:
            r.finding("endpoint", f"the endpoint does not return the stored CRC32C on HEAD (got {got})")
    st, _, rb = s3.request("PUT", b, base + "header-bad", body=body, headers={**hdr, "x-amz-checksum-crc32c": bad}, ok=None, retry=False)
    code = s3check.parse_error(st, rb).code if st >= 300 else ""
    r.check("endpoint: a PUT with a wrong CRC32C header is refused", st == 400, f"status {st} {code}")
    if st == 400 and code != "BadDigest":
        r.finding("endpoint", f"a wrong CRC32C header is refused with {code}, not BadDigest")
    st, code = s3.put_object_trailer(b, base + "trailer-good", body, good)
    if st != 200:
        r.finding("endpoint", f"aws-chunked upload with a CRC32C trailer is not accepted: {st} {code}")
    else:
        r.check("endpoint: trailer upload stored the right bytes", s3.get_object(b, base + "trailer-good") == body, "content differs")
        st, code = s3.put_object_trailer(b, base + "trailer-bad", body, bad)
        r.check("endpoint: a wrong CRC32C trailer is refused", st == 400, f"status {st} {code}")
        if st == 400 and code != "BadDigest":
            r.finding("endpoint", f"a wrong CRC32C trailer is refused with {code}, not BadDigest")
    # the clients: which checksums does an upload leave on the object, and are they right?
    small, multi = r.src("s11/small.bin"), r.gen("s11/multi.bin", 12 * MiB)
    with open(small, "wb") as f:
        f.write(body)
    want = s3check.checksums(body)
    r.each("up small", lambda c: ["cp", small, r.url(c, "small")], est={"put": 1})
    r.each("up multipart", lambda c: ["cp", "--part-size", "5", multi, r.url(c, "multi")], est={"put": 5})
    for c in r.clients:
        seen = {}
        for name in ("small", "multi"):
            try:
                h = s3.head_object(b, r.prefix(c, name), checksum=True)
            except S3Error as e:
                r.check(f"up {name}: object exists", False, e, c)
                continue
            sums = {k[len("x-amz-checksum-"):]: v for k, v in h.items() if k.startswith("x-amz-checksum-")}
            seen[name] = sums
            if name == "small":
                wrong = {k: v for k, v in sums.items() if k in want and v != want[k]}
                r.check("up small: every checksum on the object is right", not wrong, f"{wrong}", c)
        r.finding("note", f"checksums left on uploaded objects: single PUT {seen.get('small') or 'none'}, "
                          f"multipart {seen.get('multi') or 'none'}", client=c.name)
        r.verify_upload(c, r.prefix(c, "multi"), multi, "up multipart")


# --- S12 -------------------------------------------------------------------------------------

def s12(r):
    """--no-sign-request against a public object, with no credentials in the environment"""
    t = r.t
    if not t.public_url:
        raise Skip("no public object configured for this target (SOAK_PUBLIC_URL)")
    bucket, key = t.public_url[5:].split("/", 1)
    env = {"AWS_ACCESS_KEY_ID": None, "AWS_SECRET_ACCESS_KEY": None, "AWS_SESSION_TOKEN": None}
    if t.public_region:
        env.update(AWS_REGION=t.public_region, AWS_DEFAULT_REGION=t.public_region)
    region = t.public_region or t.region
    anon = s3check.S3("-", "-", region, endpoint_url=t.endpoint, addressing=t.addressing, ca_file=t.ca_file)
    try:
        want = anon.get_object(bucket, key, unsigned=True)
    except S3Error as e:
        raise Skip(f"the public object is not readable without credentials ({e.status} {e.code})")
    res = r.each("no-sign-request cp", lambda c: ["cp", t.public_url, r.work(c, "pub.dl")], gflags=["--no-sign-request"],
                 env=env, expect=None, compare=False, est={"get": 2})
    for x in res:
        dl = r.work(x.client, "pub.dl")
        ok = x.rc == 0 and os.path.isfile(dl) and open(dl, "rb").read() == want
        if x.client.kind == "rs5cmd":   # expected to fail until issue #6 is fixed: recorded, not counted
            r.stats[r.scn]["clients"][x.client.name]["note"] = "works" if ok else "expected failure (#6), not counted"
            r.finding("note", "--no-sign-request " + ("works in this build (issue #6 looks fixed)" if ok else
                      f"fails as expected (issue #6): exit {x.rc}"), x.err.strip()[:300], x.client.name, x.argv)
        else:
            r.check("no-sign-request: the public object is fetched", ok, f"exit {x.rc}: {x.err.strip()[:200]}", x.client)
    r.each("no-sign-request ls", lambda c: ["ls", t.public_url], gflags=["--no-sign-request"], env=env, expect=None,
           compare=False, est={"put": 1})


# --- S13 -------------------------------------------------------------------------------------

def s13(r):
    """Mixed loop: uploads, downloads, small batches, sync and metadata at a few MB/s, errors and retries counted"""
    sc = r.scale
    target, deadline = sc["s13_mbps"] * 1_000_000, time.time() + sc["s13_seconds"]
    pool = {m: r.gen(f"s13/f{m}.bin", m * MiB) for m in sc["s13_sizes_mib"]}
    batch = make_tree(r.src("s13batch"), 200, 4096, "s13batch")
    rng = random.Random(r.id)
    held = {c: collections.deque() for c in r.clients}
    ops, it = collections.Counter(), 0
    tx0, rx0 = link_bytes()
    t0 = time.time()
    rnd = 0
    while time.time() < deadline:
        # One kind of operation and one size per round, done by every client in turn (the order
        # alternates), so that the clients get the same mix. Choosing per operation instead lets
        # one client end up with the uploads and the other with the downloads.
        if it % len(r.clients) == 0:
            rnd += 1
            tx, rx = link_bytes()
            up, down = tx - tx0, rx - rx0
            if all(held.values()) and down * 2 < up:
                op = "down"
            else:
                op = "batch" if rnd % 9 == 5 else "sync" if rnd % 9 == 7 else "meta" if rnd % 9 == 8 else "up"
            pick = rng.randrange(1 << 30)
            order = r.clients if rnd % 2 else list(reversed(r.clients))
        c = order[it % len(r.clients)]
        it += 1
        ops[op] += 1
        tag = f"{rnd:04d}"
        if op == "down":
            key, src = held[c][pick % len(held[c])]
            dl = r.work(c, "dl", tag)
            r.run(c, ["cp", f"s3://{r.t.bucket}/{key}", dl], f"down {tag}", debug=True, quiet=True,
                  est={"get": req_down(os.path.getsize(src))})
            r.verify_download(c, dl, src, f"down {tag}")
            if os.path.exists(dl):
                os.unlink(dl)
        elif op == "up":
            m = sc["s13_sizes_mib"][pick % len(sc["s13_sizes_mib"])]
            key = r.prefix(c, "obj", f"{tag}-{m}MiB")
            x = r.run(c, ["cp", pool[m], f"s3://{r.t.bucket}/{key}"], f"up {tag}", debug=True, quiet=True, est={"put": req_up(m * MiB)})
            if x.rc == 0 and r.verify_upload(c, key, pool[m], f"up {tag}"):
                held[c].append((key, pool[m]))
            while len(held[c]) > sc["s13_keep"]:
                old, _ = held[c].popleft()
                r.run(c, ["rm", f"s3://{r.t.bucket}/{old}"], f"rm {tag}", debug=True, quiet=True)
                r.check(f"rm {tag}: gone", not r.s3.exists(r.t.bucket, old), "still there", c)
        elif op == "batch":
            u = r.url(c, "batch", tag)
            r.run(c, ["cp", batch + "/", u + "/"], f"batch up {tag}", debug=True, quiet=True, est={"put": 200})
            x = r.run(c, ["ls", u + "/*"], f"batch ls {tag}", debug=True, quiet=True, est={"put": 1})
            r.check(f"batch ls {tag}: 200 keys", len(x.lines) == 200, f"{len(x.lines)} lines", c)
            r.run(c, ["rm", u + "/*"], f"batch rm {tag}", debug=True, quiet=True)
            r.check(f"batch rm {tag}: nothing left", not r.remote_keys(r.prefix(c, "batch", tag) + "/"), "keys left", c)
        elif op == "sync":
            u = r.url(c, "sync", tag)
            r.run(c, ["sync", batch + "/", u + "/"], f"sync {tag}", debug=True, quiet=True, est={"put": 201})
            n = len(r.remote_keys(r.prefix(c, "sync", tag) + "/"))
            r.check(f"sync {tag}: 200 keys", n == 200, f"{n} keys", c)
            r.run(c, ["rm", u + "/*"], f"sync rm {tag}", debug=True, quiet=True)
        else:
            key = r.prefix(c, "meta", tag)
            src = os.path.join(batch, "d00", "f0000")
            x = r.run(c, ["cp", "--content-type", "text/x-soak", src, f"s3://{r.t.bucket}/{key}"], f"meta {tag}", debug=True,
                      quiet=True, est={"put": 1})
            if x.rc == 0:
                r.check(f"meta {tag}: content type", r.s3.head_object(r.t.bucket, key).get("content-type") == "text/x-soak",
                        "not stored", c)
        # pace to the target average
        tx, rx = link_bytes()
        ahead = (tx - tx0 + rx - rx0) / target - (time.time() - t0)
        while ahead > 0 and time.time() < deadline:
            r.guard()
            nap = min(ahead, 5, max(0.0, deadline - time.time()))
            time.sleep(nap)
            ahead -= nap
        if it % 10 == 0:
            el = time.time() - t0
            r.say(f"   S13 {el:5.0f} s: {it} operations {dict(ops)}, {(tx - tx0) / el / 1e6:.2f} MB/s up, {(rx - rx0) / el / 1e6:.2f} MB/s down")
    el = time.time() - t0
    tx, rx = link_bytes()
    st = r.stats[r.scn]
    failed = sum(c["failed"] for c in st["clients"].values())
    retries = {n: c["retries"] for n, c in st["clients"].items()}
    r.event("mixed_loop", seconds=round(el), operations=it, ops=dict(ops), up_bps=round((tx - tx0) / el),
            down_bps=round((rx - rx0) / el), failed=failed, retries=retries)
    r.say(f"   S13: {it} operations in {el:.0f} s, {(tx - tx0 + rx - rx0) / el / 1e6:.2f} MB/s on average "
          f"({(tx - tx0) / max(1, tx - tx0 + rx - rx0):.0%} up), {failed} failed steps or checks, retries {retries}")
    r.check("mixed loop: no failed step or check", failed == 0, f"{failed} failed")


# --- S14 -------------------------------------------------------------------------------------

def s14(r):
    """Performance rows: W1 many 4 KiB objects, W3 one large object, W4 listing; wall, CPU and RSS per round"""
    sc = r.scale
    n = sc["s14_w1"]
    tree = make_tree(r.src("w1"), n, 4096, "w1")
    w3 = r.gen("s14/w3.bin", sc["s14_w3_mib"] * MiB)
    size = os.path.getsize(w3)
    for rnd in range(1, sc["s14_rounds"] + 1):
        for c in (r.clients if rnd % 2 else list(reversed(r.clients))):
            u = r.url(c, f"r{rnd}", "w1")
            r.run(c, ["cp", tree + "/", u + "/"], f"W1 up r{rnd}", est={"put": n}, timeout=3600)
            x = r.run(c, ["ls", u + "/*"], f"W4 ls r{rnd}", est={"put": -(-n // 1000)})
            r.check(f"W4 ls r{rnd}: {n} keys", len(x.lines) == n, f"{len(x.lines)} lines", c)
            dl = r.work(c, f"r{rnd}", "w1dl", "x")[:-2]
            r.run(c, ["cp", u + "/*", dl + "/"], f"W1 down r{rnd}", est={"get": n, "put": -(-n // 1000)}, timeout=3600)
            got = local_files(dl) if os.path.isdir(dl) else {}
            bad = [k for i, k in enumerate(sorted(got)) if i % 97 == 0 and md5_file(os.path.join(dl, k)) != md5_file(os.path.join(tree, k))]
            r.check(f"W1 down r{rnd}: every file intact", len(got) == n and not bad, f"{len(got)} of {n} files, {len(bad)} differ", c)
            shutil.rmtree(dl, ignore_errors=True)
            r.run(c, ["cp", w3, r.url(c, f"r{rnd}", "w3.bin")], f"W3 up r{rnd}", est={"put": req_up(size)}, timeout=3600)
            r.verify_upload(c, r.prefix(c, f"r{rnd}", "w3.bin"), w3, f"W3 up r{rnd}")
            d3 = r.work(c, f"r{rnd}", "w3.dl")
            r.run(c, ["cp", r.url(c, f"r{rnd}", "w3.bin"), d3], f"W3 down r{rnd}", est={"get": req_down(size)}, timeout=3600)
            r.verify_download(c, d3, w3, f"W3 down r{rnd}")
            if os.path.exists(d3):
                os.unlink(d3)
            # not measured: empty the round's prefix with the checker, so storage stays bounded
            keys = list(r.remote_keys(r.prefix(c, f"r{rnd}") + "/"))
            for i in range(0, len(keys), 1000):
                r.s3.delete_objects(r.t.bucket, keys[i:i + 1000])


SCENARIOS = [("S0", s0), ("S1", s1), ("S2", s2), ("S3", s3), ("S4", s4), ("S5", s5), ("S6", s6), ("S7", s7),
             ("S8", s8), ("S9", s9), ("S10", s10), ("S11", s11), ("S12", s12), ("S14", s14), ("S13", s13)]
