#!/usr/bin/env python3
"""Validate a complete timer baseline CSV and print batch-mean medians, not p50 latency."""
import csv
import statistics
import sys

cases = ("rearm", "cancel_arm", "dispatch")
sizes = (1, 32, 256, 4096)
groups = {}
with open(sys.argv[1], newline="", encoding="utf-8") as source:
    for row in csv.DictReader(source):
        key = (row["case"], int(row["n"]))
        assert key[0] in cases and key[1] in sizes
        assert int(row["allocs"]) == int(row["frees"]) == 0
        ops, elapsed = int(row["operations"]), int(row["elapsed_ns"])
        assert ops > 0 and elapsed > 0
        assert int(row["callbacks"]) == (ops if key[0] == "dispatch" else 0)
        group = groups.setdefault(key, {})
        round_id = int(row["round"])
        assert round_id not in group
        group[round_id] = elapsed / ops

assert set(groups) == {(case, n) for case in cases for n in sizes}
rounds = set(next(iter(groups.values())))
assert rounds == set(range(len(rounds)))
assert all(set(group) == rounds for group in groups.values())
print("| Timers | Rearm ns | Cancel+arm ns/cycle | Dispatch ns/event |")
print("| ---: | ---: | ---: | ---: |")
for n in sizes:
    values = [statistics.median(groups[(case, n)].values()) for case in cases]
    print(f"| {n} | " + " | ".join(f"{v:.1f}" for v in values) + " |")
print(f"\nMedian of {len(rounds)} batch means; not single-operation latency percentiles.")
print("All measured operation phases: 0 allocation calls, 0 free calls.")
print("\n| Case | Timers | Min batch mean ns | Max batch mean ns |")
print("| --- | ---: | ---: | ---: |")
for (case, n), samples in sorted(groups.items()):
    print(f"| {case} | {n} | {min(samples.values()):.1f} | {max(samples.values()):.1f} |")
