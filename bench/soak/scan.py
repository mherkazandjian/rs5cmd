#!/usr/bin/env python3
"""Scan before every commit: no credentials and no private information in what is committed.

    env -u PYTHONSTARTUP python3 bench/soak/scan.py [path ...]     (default: bench/soak)

Looks in every file (gzip members are read too) for
  - AWS key ids (AKIA / ASIA followed by 16 characters);
  - the values in the soak credential files, if they exist (~/.config/rs5cmd-soak/*.env):
    keys, secrets, tokens, role ARN, bucket, endpoint host. Values are never printed, only the
    name of the variable that matched;
  - this machine's host name and alias (SOAK_HOST_ALIAS), the current user name, home directory
    paths and IPv4 addresses.
Exit code 1 if anything is found.
"""
import getpass
import glob
import gzip
import os
import re
import socket
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
roots = sys.argv[1:] or [HERE]
patterns = [("AWS key id", re.compile(r"\b(AKIA|ASIA)[0-9A-Z]{16}\b")),
            ("home path", re.compile(r"/home/[A-Za-z0-9_.-]+|/Users/[A-Za-z0-9_.-]+")),
            ("IPv4 address", re.compile(r"\b(?!127\.0\.0\.1\b|0\.0\.0\.0\b|169\.254\.169\.254\b)(?:\d{1,3}\.){3}\d{1,3}\b"))]
patterns.append(("user name", re.compile(r"(?i)\b" + re.escape(getpass.getuser()) + r"\b")))
if os.environ.get("SOAK_HOST_ALIAS"):   # a short name people use for this machine, if there is one
    patterns.append(("host alias", re.compile(r"\b" + re.escape(os.environ["SOAK_HOST_ALIAS"]) + r"\b")))
literals = []
host = socket.gethostname()
literals += [("host name", h) for h in {host, host.split(".")[0]} if len(h) > 2]
for path in glob.glob(os.path.expanduser(os.environ.get("SOAK_CREDS_DIR", "~/.config/rs5cmd-soak") + "/*.env")):
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if "=" not in line or line.startswith("#"):
                continue
            k, v = line.removeprefix("export ").split("=", 1)
            v = v.strip().strip("'\"")
            if k.strip() in ("AWS_REGION", "SOAK_ADDRESSING", "SOAK_KIND", "SOAK_RGW_VERSION", "SOAK_STORAGE_CLASSES") or len(v) < 6:
                continue
            if "://" in v:
                v = v.split("://", 1)[1].split("/", 1)[0].split(":")[0]
            literals.append((f"value of {k.strip()} in {os.path.basename(path)}", v))

found = 0
for root in roots:
    files = [root] if os.path.isfile(root) else [os.path.join(d, n) for d, _, ns in os.walk(root) for n in ns]
    for p in sorted(files):
        if "/.run/" in p or "/__pycache__/" in p:
            continue
        try:
            with (gzip.open if p.endswith(".gz") else open)(p, "rb") as f:
                text = f.read().decode("utf-8", "replace")
        except OSError as e:
            print(f"{p}: unreadable ({e})")
            found += 1
            continue
        for i, line in enumerate(text.splitlines(), 1):
            hits = [name for name, rx in patterns if rx.search(line)]
            hits += [name for name, lit in literals if lit in line]
            if p.endswith("scan.py"):
                hits = [h for h in hits if h not in ("home path",)]
            for h in hits:
                found += 1
                if found <= 40:
                    print(f"{os.path.relpath(p)}:{i}: {h}")
print(f"scan: {found} finding(s) in {', '.join(os.path.relpath(r) for r in roots)}")
sys.exit(1 if found else 0)
