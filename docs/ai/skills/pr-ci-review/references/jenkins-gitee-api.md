# Jenkins & Gitee API Reference (verified against the live instance)

All endpoints below were verified with anonymous `curl` against

- Jenkins: `https://jenkins.openkylin.top` (X-Jenkins: 2.568.3)
- Gitee:   `https://gitee.com` (API v5)

## Jenkins: read-only (no auth needed)

### Job / build lookup

| Endpoint | Purpose |
|---|---|
| `GET /job/x-kernel-ci-test/api/json` | job info: `buildable`, `inQueue`, `lastBuild`, `lastCompletedBuild`, `nextBuildNumber` |
| `GET /job/x-kernel-ci-test/api/json?tree=builds[number,result,url,actions[causes[shortDescription]]]` | recent builds (newest first, **max 31**), each with webhook cause like `Triggered by Gitee Pull Request #759: ...` |
| `GET /job/x-kernel-ci-test/<n>/api/json` | build overview: `building`, `result` (`null` = running), `duration`, `estimatedDuration`, `timestamp`, `url`, `actions[].parameters[]` |
| `GET /job/x-kernel-ci-test/<n>/wfapi/describe` | stage-level progress: overall `status`, each stage `name/status/startTimeMillis/durationMillis`; running stage is `IN_PROGRESS` |
| `GET /job/x-kernel-ci-test/<n>/consoleText` | full console log (plain text; large builds are MBs — save to file, then `grep`) |
| `GET /job/x-kernel-ci-test/<n>/execution/node/<id>/wfapi/describe` | single stage detail (from `wfapi/describe` links) |

Notes:

- The 31-build window is a server-side retention policy, not an API limit;
  `allBuilds` and `tree=...{0,200}` still return at most 31. Builds outside the
  window return `404` for `api/json`, `consoleText`, `wfapi/*`, and artifacts.
- `search/suggest?query=` does **not** index build causes; do the filtering client-side.

### Artifacts (deployed under `artifact/.ci/`, anonymous GET)

The pipeline archives test logs as Jenkins artifacts under `.ci/`:

- `GET /job/x-kernel-ci-test/<n>/artifact/.ci/stage-logs/<Stage_Name>.log`
  stage names use underscores, e.g. `Runtime_Test_kplat-x86_64.log`.
- `GET /job/x-kernel-ci-test/<n>/artifact/.ci/work/runtime-<arch>/test-harness/logs/ci/<YYYYMMDD-HHMMSS>/cases/<case>.log`
- `GET /job/x-kernel-ci-test/<n>/artifact/.ci/work/runtime-<arch>/test-harness/logs/ci/<ts>/artifacts/<case>/host-ca.log`
- `GET /job/x-kernel-ci-test/<n>/artifact/.ci/*zip*/.ci.zip` — download everything.

Useful patterns for failure analysis:

```bash
# stage log → find the failing case and its exact log path
curl -sk ".../artifact/.ci/stage-logs/Runtime_Test_kplat-x86_64.log" | grep -n "FAILED\|✗"
# the stage log prints:  │ [16/30] xtee-regression logs/ci/<ts>/cases/xtee-regression.log
```

Note: the artifact tree API returns *all* artifacts (tens of thousands); avoid
dumping it wholesale — guess paths from the stage log instead.

### CI job list (view `xkernel`)

`GET /view/xkernel/api/json?tree=jobs[name,url]` → jobs including
`x-kernel-ci-test`, `ai-review`, `daily-test`, `x-kernel-longevity`, etc.

## Jenkins: state-changing (needs auth + CSRF crumb)

Form login (`j_spring_security_check`), CSRF protection enabled by default.

| Endpoint | Effect |
|---|---|
| `POST /job/x-kernel-ci-test/<n>/stop` | abort a running build gracefully |
| `POST /job/x-kernel-ci-test/<n>/terminate` | hard-kill the build agent step |
| `POST /job/x-kernel-ci-test/buildWithParameters` | trigger new build with form params (below) |
| `POST /job/x-kernel-ci-test/build` | trigger new build (non-parameterized equivalent) |
| `POST /job/x-kernel-ci-test/replay` | replay pipeline edit (advanced; prefer UI) |

`x-kernel-ci-test` parameters: `PROJECT_REPO`, `DEFAULT_BRANCH`, `LIBUTEE_REPO`,
`TEST_HARNESS_REPO`, `TEST_HARNESS_BRANCH`, `ENABLE_DOC_CHECK`.

Crumb flow (verified: anonymous POST is 403/401, with fake creds it is 401):

```bash
J="https://jenkins.openkylin.top"; U=user; T=token
CRUMB=$(curl -sk -u "$U:$T" "$J/crumbIssuer/api/json" | python3 -c "import json,sys;print(json.load(sys.stdin)['crumb'])")
curl -sk -u "$U:$T" -H "Jenkins-Crumb: $CRUMB" -X POST \
  "$J/job/x-kernel-ci-test/buildWithParameters" \
  --data-urlencode "PROJECT_REPO=https://gitee.com/openkylin/x-kernel" \
  --data-urlencode "DEFAULT_BRANCH=main" \
  --data-urlencode "LIBUTEE_REPO=https://gitee.com/openkylin/rust-libutee" \
  --data-urlencode "TEST_HARNESS_REPO=https://gitee.com/openkylin/starry-test-harness" \
  --data-urlencode "TEST_HARNESS_BRANCH=master" \
  --data-urlencode "ENABLE_DOC_CHECK=false"
```

## Gitee API v5: PRs and comments (read-only, anonymous OK)

| Endpoint | Purpose |
|---|---|
| `GET /repos/openkylin/x-kernel/pulls/<n>` | PR meta: `title`, `state`, `head.sha`, `head.ref`, `base.ref`, `html_url`, `diff_url`, `patch_url` |
| `GET /repos/openkylin/x-kernel/pulls/<n>/comments?per_page=100&sort=id&order=desc` | **PR comments — the review source of truth** (see below) |
| `GET /repos/openkylin/x-kernel/pulls/<n>/commits` | commits in the PR; `statuses_url` is null here |

Comment classification (verified across PRs #751/#755/#757/#727/#759):

- AI verdict: user `x-cibot`, first line `<!-- xreview:summary arm=pipe -->`,
  one comment per PR edited in place by xreview (status table with the
  reviewed commit, the latest round's verdict, links to the review page and
  `agent.md`). Item-level details come from xreview's status call, described
  in `https://review.x-kernel.com/r/prs/<n>/agent.md`.
- Retired AI verdicts: user `x-cibot`, HTML marker `<!-- x-review: -->`,
  headings `## AI Re-review` / `## AI Code Review Summary`, from the former
  Jenkins `ai-review` job; history only.
- CI report: user `openkylin-cibot`, HTML marker `<!-- x-kernel-ci -->`,
  heading `## ❌ Jenkins CI 构建失败` or success text, plus per-stage table.
- Human comments/commands: any other user (e.g. maintainer `rerun`).

Important:

- `GET .../issues/<n>/comments` returns `404` for PRs on Gitee — use
  `pulls/<n>/comments` instead.
- Commit status endpoint `.../commits/<sha>/status` returned `Commit not found`
  for this repo/head SHA, so do not rely on Gitee-side commit status to locate
  Jenkins builds; the Jenkins webhook-cause filter is the reliable path.
- PR page HTML contains no Jenkins URL link either.

## Worked example (PR #759 → build → failure → opinions)

```bash
# 1. latest build for PR
curl -sk "https://jenkins.openkylin.top/job/x-kernel-ci-test/api/json?tree=builds%5Bnumber,result,url,actions%5Bcauses%5BshortDescription%5D%5D%5D" \
 | python3 -c "import json,sys; [print(b['number'],b.get('result') or 'RUNNING') for b in json.load(sys.stdin)['builds'] for a in b.get('actions',[]) if a.get('_class')=='hudson.model.CauseAction' for c in a.get('causes',[]) if '#759' in c.get('shortDescription','')]"

# 2. stage progress
curl -sk "https://jenkins.openkylin.top/job/x-kernel-ci-test/954/wfapi/describe" | python3 -m json.tool

# 3. failing stage artifacts
curl -sk "https://jenkins.openkylin.top/job/x-kernel-ci-test/954/artifact/.ci/stage-logs/Runtime_Test_kplat-x86_64.log" | grep -n "FAILED"

# 4. review opinions (read from the Gitee comment area)
curl -sk "https://gitee.com/api/v5/repos/openkylin/x-kernel/pulls/759/comments?per_page=100&sort=id&order=desc"
```