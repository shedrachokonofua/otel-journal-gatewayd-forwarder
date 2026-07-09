#!/usr/bin/env python3
"""Continuation E2E verifier.

Exits 0 iff the sink contains exactly the expected contiguous message range
with no duplicates, gaps, or ordering regressions.
"""
import glob
import json
import re
import sys


def load_messages(sink_dir: str, skip_files: int = 0) -> list[int]:
    files = sorted(glob.glob(f"{sink_dir}/req*.json"))[skip_files:]
    nums = []
    for path in files:
        with open(path, "rb") as f:
            data = json.load(f)
        for rl in data.get("resourceLogs", []):
            for sl in rl.get("scopeLogs", []):
                for lr in sl.get("logRecords", []):
                    body = lr["body"]["stringValue"]
                    nums.append(int(re.search(r"(\d+)$", body).group(1)))
    return nums


def main() -> int:
    if len(sys.argv) < 4:
        print(
            "usage: verify_continuation.py <sink_dir> <lo> <hi> [skip_files]",
            file=sys.stderr,
        )
        return 2

    sink_dir = sys.argv[1]
    lo = int(sys.argv[2])
    hi = int(sys.argv[3])
    skip_files = int(sys.argv[4]) if len(sys.argv) > 4 else 0

    nums = load_messages(sink_dir, skip_files)
    expected = set(range(lo, hi + 1))
    uniq = set(nums)
    dups = len(nums) - len(uniq)
    missing = sorted(expected - uniq)
    extra = sorted(uniq - expected)
    ordered = nums == sorted(nums)

    print(
        f"  requests={len(glob.glob(f'{sink_dir}/req*.json')) - skip_files} "
        f"records={len(nums)} unique={len(uniq)} dups={dups} "
        f"missing={len(missing)} extra={len(extra)} ordered={ordered}"
    )

    if dups or missing or extra or not ordered:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
