# Phase 4: Cross Review

## Owners

- Linux MM Expert reviews the X-Kernel draft
- X-Kernel Memory Designer reviews the Linux baseline usage

## Purpose

Force both sides to critique the other before design freeze.

## Required Output

Use `templates/review-findings-template.md`.

Produce two review sections:

1. `linux-review-of-xkernel`
2. `xkernel-review-of-linux-usage`

## Rules

- Findings first, no blended narrative.
- Each finding must have a severity.
- Findings must be specific enough to trigger an arbiter decision.

## Exit Criteria

Both directions of review are present and at least all critical issues are explicit.
