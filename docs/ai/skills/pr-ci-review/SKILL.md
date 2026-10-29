---
name: pr-ci-review
description: Use when creating or updating a Gitee Pull Request for X-Kernel, checking the latest CI status of a PR (Jenkins x-kernel-ci-test), checking a PR's xreview AI review, or when asked to request an AI review or respond to an AI finding. AI review state comes from xreview's status API; human opinions and CI reports from the Gitee PR comment area.
---

# PR CI and Review Tracking

Use this skill when an AI agent needs to:

- create or update a Pull Request against `openkylin/x-kernel`;
- find the latest Jenkins CI build for a given PR number;
- check whether the CI build is still running, what stage it is on, or why it failed;
- check a PR's AI review (xreview), request an AI review, or respond to an
  AI finding;
- read human review opinions and CI reports from the Gitee PR comment area.

## Key Context

- Code repository: `https://gitee.com/openkylin/x-kernel` (Gitee, PR-based workflow)
- CI job: Jenkins `x-kernel-ci-test` at `https://jenkins.openkylin.top/job/x-kernel-ci-test/`
- AI review: xreview at `https://review.x-kernel.com:57482` (the port is part
  of the URL; the bare host returns 403), triggered by the Gitee webhook on
  every push. Each PR has a review page at
  `https://review.x-kernel.com:57482/r/prs/<n>` and agent instructions at
  `https://review.x-kernel.com:57482/r/prs/<n>/agent.md` (generic version:
  `https://review.x-kernel.com:57482/r/agent.md`). agent.md is the source of
  truth for the review API; read it instead of copying calls from here.
- xreview posts one comment per PR as user `x-cibot` and edits it in place;
  its HTML markers start with `<!-- xreview:` (`summary`, `status`, `hint`).
  There are no inline comments.
- Older `x-cibot` comments marked `<!-- x-review:summary -->` /
  `<!-- x-review:review -->` are history from the retired `ai-review` job.
- CI status is also posted back to the PR as comments by user `openkylin-cibot`
  with an HTML comment marker `<!-- x-kernel-ci -->`.
- Jenkins and Gitee endpoints and curl snippets live only in
  [references/jenkins-gitee-api.md](references/jenkins-gitee-api.md)
  ("the reference"); the workflows below name the section to use.

## Core Rules

### Rule 1: AI review state comes from xreview's status call

The verdict, the blocking items and their ids come from xreview's status call
(section D). The `x-cibot` comment is only a summary that points to the review
page and agent.md. Human opinions and CI reports come from the Gitee PR
comment area.

### Rule 2: Anonymous access is enough for status, opinions and xreview calls

Both the Jenkins API and the Gitee API v5 are readable without credentials
for this project (state-changing actions need a Jenkins account, see below).
All xreview calls, POSTs included, are unauthenticated, but every POST must
carry the header `X-Xreview-Client: <name>[/<version>]`.

### Rule 3: Control actions require Jenkins auth; never do them passively

Stop / terminate / re-trigger / replay endpoints all return `403` (CSRF gate)
or `401` (auth) anonymously. They need a signed-in Jenkins user with Build
permission plus a CSRF crumb (reference: "Jenkins: state-changing"). Do not
probe control endpoints speculatively with real credentials; only act when the
user explicitly asks.

### Rule 4: Jenkins keeps only the most recent 31 builds

The `x-kernel-ci-test` job discards old builds. The API returns at most the
latest 31 builds (both `builds` and `allBuilds`). Older builds' console logs,
artifact paths, and `wfapi` stage data are gone (`404`), so failure details can
only be diagnosed for builds still inside the 31-build window.

### Rule 5: A PR number maps to builds via the Gitee webhook cause

Every build's `CauseAction` records `Triggered by (Debin) Gitee Pull Request
#<n>: <branch> => <base>`. Filter on that string to map PR number to build
numbers. The list is ordered newest first, so take the first match for "latest".

## Standard Workflow

### A. Create or update a PR

1. Branch from the latest `main`, make the change, and push to Gitee (needs
   a Gitee account with an SSH key or push credentials):

   ```bash
   git fetch origin
   git checkout -b fix/your-topic origin/main
   git add -A && git commit -m "fix(sub): meaningful summary"
   git push -u origin fix/your-topic
   ```

2. Open the PR with the `ge` CLI (Gitee v5 API wrapped; already authenticated
   in this environment as user `luodeb`):

   ```bash
   # from inside the repo (repo is auto-detected from git remote),
   # head branch = the branch you pushed, base = main
   ge pr create --title "fix(sub): meaningful summary" \
     --body "Describe what changed, why, and the expected behavior;
reference related issues with 'Fixes #IJSXXX' if any." \
     --head fix/your-topic --base main

   # body from a file is better for longer descriptions:
   ge pr create --title "fix(sub): meaningful summary" \
     --body-file ./pr-body.md --head fix/your-topic --base main

   # auto-fill title/body from the HEAD commit (subject + body):
   ge pr create --fill --head fix/your-topic --base main
   ```

   Verify creation with `ge pr view --repo openkylin/x-kernel` or the
   pull's `number` in the response. The raw API call behind `ge pr create`
   is in the reference ("Gitee API v5: create a PR").

3. After the PR is open, the Gitee webhook automatically triggers:
   - Jenkins `x-kernel-ci-test` (build pipeline);
   - an xreview round on the head commit (posts or updates the xreview comment).
     Large PRs above the approval threshold wait for a maintainer; the author
     (or the author's agent) can start a round from the review page or with
     the review call in section D.

4. Update the PR (new commits / `--amend`) pushes again; CI re-runs and
   xreview reviews the new head (a follow-up round against the previous one);
   then work through section D.

### B. Get the latest CI status for a PR

1. Map the PR number to its builds and take the first (newest) one
   (reference: "Map a PR number to its builds").
2. Read the build overview (`result` is `null` while running), the stage
   progress (the running stage is `IN_PROGRESS`) and, only when needed, the
   multi-MB console log (reference: "Job / build lookup").

### C. Diagnose why a CI build failed

Follow the reference's "Failure analysis":

1. Find the `FAILED` stage; later `ABORTED` stages are a cascade.
2. Grep the console log for the failing case.
3. Read the failing stage log, then the per-case and host-companion logs
   (reference: "Artifacts"); guess artifact paths from the stage log rather
   than loading the whole artifact tree.
4. Decide flake vs regression against earlier successful builds in the
   31-build window.

### D. Handle the AI review and read review opinions

Follow `https://review.x-kernel.com:57482/r/prs/<n>/agent.md`. In short:

1. GET the status: state, the latest round's verdict, and every item with its
   `item_id`.
2. For each blocking item, either fix it and push, or POST a response:
   `false_positive` with `file:line` evidence, `wont_fix` (still blocks the
   merge), or `recheck` for items of kind `inconclusive`.
3. After pushing fixes, POST a review request for the new head.
4. Re-read the status and check each response's `state`.

```bash
PR=759
XR="https://review.x-kernel.com:57482/api/v1/prs/$PR"
H='X-Xreview-Client: <agent>/<version>'
J='Content-Type: application/json'
curl -s "$XR/status"
curl -s -X POST "$XR/items/<item>/response" -H "$H" -H "$J" \
  -d '{"kind": "false_positive", "reason": "why, with file:line evidence"}'
curl -s -X POST "$XR/review" -H "$H" -H "$J" -d '{"note": "pushed fixes"}'
curl -s -X POST "$XR/recheck" -H "$H" -H "$J" -d '{}'  # all inconclusive items
```

For human opinions and CI reports, list and classify the PR comments
(reference: "Gitee API v5: PRs and comments").

The status call is authoritative for the AI review; human comments can
override it, so check them before reporting a verdict.

## Validation Expectations

At minimum, report:

- the PR number and its head commit SHA;
- the latest build number/result and which stage failed (or is running);
- for failures: the failing stage, failing case, and the key error line from
  the per-case or host-companion log;
- the latest review opinion: xreview's verdict for the current head (from the
  status call) and any human comments, citing their Gitee comment ids;
- the xreview item ids handled and the response or recheck sent for each.