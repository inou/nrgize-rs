#!/usr/bin/env python3
"""Rewrite Formula/nrg.rb's version and per-target sha256 values from a release checksums.txt.

    bump-formula.py VERSION CHECKSUMS_FILE [FORMULA_FILE]

VERSION may be "v0.2.0" or "0.2.0". CHECKSUMS_FILE is the release's combined checksums.txt
("<sha256>  nrg-<target>.tar.gz" per line; extra assets such as the musl archives are ignored).
FORMULA_FILE defaults to Formula/nrg.rb next to this script's repository root.

Exits non-zero, leaving the formula untouched, if the version is malformed, or if any of the
four formula targets is missing from either the checksums file or the formula.
"""
import os
import re
import sys

TARGETS = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
]


def die(msg):
    sys.stderr.write("bump-formula: %s\n" % msg)
    sys.exit(1)


def parse_checksums(path):
    sums = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            m = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?(\S+)", line)
            if not m:
                die("unparseable line in %s: %r" % (path, line))
            sums[m.group(2)] = m.group(1).lower()
    return sums


def main(argv):
    if len(argv) not in (3, 4):
        die("usage: bump-formula.py VERSION CHECKSUMS_FILE [FORMULA_FILE]")
    version = argv[1][1:] if argv[1].startswith("v") else argv[1]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", version):
        die("version must look like vX.Y.Z (got %r)" % argv[1])
    sums = parse_checksums(argv[2])
    formula = argv[3] if len(argv) == 4 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "Formula", "nrg.rb"
    )
    with open(formula, encoding="utf-8") as f:
        lines = f.read().split("\n")

    out = list(lines)
    version_hits = [i for i, l in enumerate(out) if re.fullmatch(r'  version "[^"]*"', l)]
    if len(version_hits) != 1:
        die("expected exactly one top-level `version \"...\"` line in %s" % formula)
    out[version_hits[0]] = '  version "%s"' % version

    for target in TARGETS:
        asset = "nrg-%s.tar.gz" % target
        if asset not in sums:
            die("%s missing from %s" % (asset, argv[2]))
        url_hits = [i for i, l in enumerate(out) if l.strip().startswith("url ") and asset in l]
        if len(url_hits) != 1:
            die("expected exactly one url line for %s in %s" % (asset, formula))
        i = url_hits[0] + 1
        if i >= len(out) or not re.fullmatch(r'\s+sha256 "[0-9a-f]{64}"', out[i]):
            die("no sha256 line directly after the url for %s" % asset)
        indent = out[i][: len(out[i]) - len(out[i].lstrip())]
        out[i] = '%ssha256 "%s"' % (indent, sums[asset])

    with open(formula, "w", encoding="utf-8") as f:
        f.write("\n".join(out))


main(sys.argv)
