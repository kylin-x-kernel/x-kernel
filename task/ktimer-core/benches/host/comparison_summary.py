#!/usr/bin/env python3
"""Validate host old/new pairs; report medians of batch means, not IRQ latency."""
import csv
import statistics
import sys

cases = ("start_cancel", "dispatch", "lifecycle")
sizes = (1, 32, 256, 4096)
timings = {}
allocations = {}
with open(sys.argv[1], newline="", encoding="utf-8") as source:
    for row in csv.DictReader(source):
        key = (row["case"], int(row["n"]), row["version"])
        assert key[0] in cases and key[1] in sizes
        assert key[2] in ("legacy", "prepared")
        ops, elapsed = int(row["operations"]), int(row["elapsed_ns"])
        assert ops >= 4096 and ops % 4096 == 0 and elapsed > 0
        if row["mode"] == "timing":
            samples = timings.setdefault(key, {})
            samples.setdefault(int(row["round"]), []).append(elapsed / ops)
        else:
            assert row["mode"] == "allocation" and key not in allocations
            counts = (int(row["allocs"]), int(row["frees"]))
            if key[2] == "prepared" and key[0] != "lifecycle":
                assert counts == (0, 0)
            allocations[key] = tuple(value / ops for value in counts)

expected = {(case, n, version) for case in cases for n in sizes
            for version in ("legacy", "prepared")}
assert set(timings) == set(allocations) == expected
for rounds in timings.values():
    assert set(rounds) == {0, 1, 2}
    assert all(len(values) == 2 for values in rounds.values())

print("| Case | Timers | Legacy ns/op | Prepared ns/op | Change | Legacy alloc/free per op | Prepared alloc/free per op |")
print("| --- | ---: | ---: | ---: | ---: | ---: | ---: |")
for case in cases:
    for n in sizes:
        medians = [statistics.median(value for values in timings[(case, n, version)].values()
                                    for value in values)
                   for version in ("legacy", "prepared")]
        old, new = medians
        old_alloc = allocations[(case, n, "legacy")]
        new_alloc = allocations[(case, n, "prepared")]
        print(f"| {case} | {n} | {old:.1f} | {new:.1f} | {(new / old - 1) * 100:+.1f}% | "
              f"{old_alloc[0]:.3f}/{old_alloc[1]:.3f} | {new_alloc[0]:.3f}/{new_alloc[1]:.3f} |")
print("\nThree ABBA blocks; median of six batch means per version. Lower is better.")
print("Allocation probes are separate; timing-row zeros mean counting disabled, not no allocation.")
print("Legacy is a source-derived host model, not a mainline kernel measurement. No IRQ/IPI or scheduler cost.")
