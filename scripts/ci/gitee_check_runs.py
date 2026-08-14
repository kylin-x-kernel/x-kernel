#!/usr/bin/env python3
"""Gitee Check-Runs API 工具（Jenkins 门禁检查 + 本地调试）

Jenkins 流水线请使用 --jenkins-manifest（由 Jenkinsfile giteeCheck() 生成；
阶段拓扑见 manifest 的 sequential_stages / parallel_stages，与 ci*StageOrder() 一致）。

本地调试示例:
  GITEE_TOKEN=xxx python3 gitee_check_runs.py --owner openkylin --repo x-kernel \\
    --jenkins-manifest gitee-ci-manifest.json --pr-db-id <id> --head-sha <sha>

  python3 gitee_check_runs.py --owner openkylin --repo x-kernel --list <commit_sha>
  python3 gitee_check_runs.py --owner openkylin --repo x-kernel --get <check_run_id>
  python3 gitee_check_runs.py --owner openkylin --repo x-kernel --update <id> --conclusion failure

  # 手工批量上报（需自备 JSON，无内置假数据）:
  GITEE_TOKEN=xxx python3 gitee_check_runs.py --owner openkylin --repo x-kernel \\
    --push-file checks.json --head-sha <sha> --pr-db-id <id>
"""

import argparse
import fcntl
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE_URL = "https://gitee.com/api/v5"
TOKEN_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".gitee_oauth_token")
DEFAULT_IDS_FILE = "gitee-check-ids.json"
MAX_TEXT_BYTES = 65535
SCOPE = "projects issues notes"

# ── CLI 状态图标 ──────────────────────────────────────────────

STATUS_ICON = {
    "queued": "⏳",
    "in_progress": "🔄",
    "completed": "✅",
}
CONCLUSION_ICON = {
    "success": "✅",
    "failure": "❌",
    "cancelled": "🚫",
    "timed_out": "⏱️",
    "action_required": "⚠️",
    "neutral": "➖",
    "skipped": "⏭️",
    "stale": "🕸️",
}
ANNOTATION_ICON = {
    "failure": "🔴",
    "warning": "🟡",
    "notice": "🔵",
}

def _load_cached_token():
    """从缓存文件读取 OAuth token"""
    if not os.path.exists(TOKEN_FILE):
        return None
    try:
        with open(TOKEN_FILE) as f:
            data = json.load(f)
        return data.get("access_token")
    except Exception:
        return None


def _resolve_token(args):
    """按优先级获取 token: --token > $GITEE_TOKEN > 缓存 OAuth token > 报错"""
    if args.token:
        return args.token
    env_token = os.environ.get("GITEE_TOKEN", "").strip()
    if env_token:
        return env_token
    cached = _load_cached_token()
    if cached:
        return cached
    print("未找到 token。请设置 GITEE_TOKEN、--token，或缓存 OAuth token。", file=sys.stderr)
    sys.exit(1)


# ── 底层请求 ──────────────────────────────────────────────────

def _request(method, path, fields=None):
    """发送 API 请求，返回 (parsed JSON, HTTP status)"""
    url = f"{BASE_URL}{path}"
    data = urllib.parse.urlencode(fields or {}).encode("utf-8") if fields else None
    req = urllib.request.Request(url, data=data, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode("utf-8")), resp.status
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        print(f"HTTP {e.code}: {body}", file=sys.stderr)
        return None, e.code


# ── API 封装 ──────────────────────────────────────────────────

def create_check_run(token, owner, repo, *, name, head_sha, pr_id=None,
                     status="queued", conclusion=None, details_url=None,
                     output=None, annotations=None):
    """创建检查任务 (POST /repos/{owner}/{repo}/check-runs)

    必须传 pr_id 才能在 PR 检查页面上展示。
    status="in_progress" 时自动记录 started_at，后续用 update_check_run 完成即可显示真实执行时长。
    """
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    fields = {
        "access_token": token,
        "name": name,
        "head_sha": head_sha,
        "status": status,
    }
    if pr_id:
        fields["pull_request_id"] = str(pr_id)
    if conclusion:
        fields["conclusion"] = conclusion
        fields["status"] = "completed"
        fields["started_at"] = now
        fields["completed_at"] = now
    elif status == "in_progress":
        fields["started_at"] = now
    if details_url:
        fields["details_url"] = details_url
    if output:
        fields["output[title]"] = output.get("title", "CI Check")
        fields["output[summary]"] = output.get("summary", "")
        text = output.get("text", "")
        fields["output[text]"] = prepare_output_text(
            text, log_artifact_url=output.get("log_artifact_url", ""))
    if annotations:
        for i, ann in enumerate(annotations):
            fields[f"output[annotations][{i}][path]"] = ann["path"]
            fields[f"output[annotations][{i}][start_line]"] = str(ann["start_line"])
            fields[f"output[annotations][{i}][end_line]"] = str(ann["end_line"])
            fields[f"output[annotations][{i}][annotation_level]"] = ann.get("annotation_level", "notice")
            fields[f"output[annotations][{i}][message]"] = ann["message"]

    data, code = _request("POST", f"/repos/{owner}/{repo}/check-runs", fields)
    if data:
        icon = CONCLUSION_ICON.get(data.get("conclusion", ""), "")
        print(f"  {icon} {name}: id={data['id']}, status={data['status']}, conclusion={data.get('conclusion', '-')}")
    return data, code


def fetch_check_run(token, owner, repo, check_run_id, *, quiet=False):
    """获取检查任务详情 (GET /repos/{owner}/{repo}/check-runs/{id})"""
    data, code = _request("GET", f"/repos/{owner}/{repo}/check-runs/{check_run_id}",
                          {"access_token": token})
    if data and not quiet:
        _print_check_run(data)
    return data, code


def get_check_run(token, owner, repo, check_run_id):
    return fetch_check_run(token, owner, repo, check_run_id)


def is_check_run_in_progress(token, owner, repo, check_run_id):
    data, code = fetch_check_run(token, owner, repo, check_run_id, quiet=True)
    return bool(data and 200 <= code < 300 and data.get("status") == "in_progress")


def ci_stage_status(ci_results, stage_name):
    ci_result = ci_results.get(stage_name)
    if isinstance(ci_result, dict):
        return ci_result.get("status") or "not_run"
    return "not_run"


def is_fail_fast_abort_detail(detail):
    """Jenkins fail-fast 中止并行分支时，catch 可能误标 failed；根据 detail 识别。"""
    if not detail:
        return False
    text = detail.lower()
    return "aborted due to" in text or "flowinterruptedexception" in text


def update_check_run(token, owner, repo, check_run_id, *, name=None,
                     status=None, conclusion=None, output=None,
                     annotations=None):
    """更新检查任务 (PATCH /repos/{owner}/{repo}/check-runs/{id})

    传 conclusion 时自动设 status=completed 并记录 completed_at，
    不覆盖 started_at，以保留真实执行时长。
    """
    fields = {"access_token": token}
    if name:
        fields["name"] = name
    if status:
        fields["status"] = status
    if conclusion:
        fields["conclusion"] = conclusion
        fields["status"] = "completed"
        fields["completed_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    if output:
        fields["output[title]"] = output.get("title", "")
        fields["output[summary]"] = output.get("summary", "")
        text = output.get("text", "")
        if text:
            fields["output[text]"] = prepare_output_text(
                text, log_artifact_url=output.get("log_artifact_url", ""))
    if annotations:
        for i, ann in enumerate(annotations):
            fields[f"output[annotations][{i}][path]"] = ann["path"]
            fields[f"output[annotations][{i}][start_line]"] = str(ann["start_line"])
            fields[f"output[annotations][{i}][end_line]"] = str(ann["end_line"])
            fields[f"output[annotations][{i}][annotation_level]"] = ann.get("annotation_level", "notice")
            fields[f"output[annotations][{i}][message]"] = ann["message"]

    data, code = _request("PATCH", f"/repos/{owner}/{repo}/check-runs/{check_run_id}", fields)
    if data:
        icon = CONCLUSION_ICON.get(data.get("conclusion", ""), "")
        print(f"  {icon} updated: id={data['id']}, status={data['status']}, conclusion={data.get('conclusion', '-')}")
    return data, code


def list_check_runs(token, owner, repo, ref, *, per_page=20, page=1):
    """获取某 commit 的所有检查任务 (GET /repos/{owner}/{repo}/commits/{ref}/check-runs)"""
    fields = {"access_token": token, "per_page": str(per_page), "page": str(page)}
    data, code = _request("GET", f"/repos/{owner}/{repo}/commits/{ref}/check-runs", fields)
    if data:
        runs = data.get("check_runs", []) if isinstance(data, dict) else data
        total = data.get("total_count", len(runs)) if isinstance(data, dict) else len(runs)
        print(f"共 {total} 个检查任务:")
        for r in runs:
            s = STATUS_ICON.get(r.get("status", ""), "")
            c = CONCLUSION_ICON.get(r.get("conclusion", ""), "") if r.get("conclusion") else ""
            print(f"  {s}{c} {r['name']} (id={r['id']}) — {r.get('conclusion', r['status'])}")
        return runs, code
    return None, code


def get_annotations(token, owner, repo, check_run_id):
    """获取检查任务的代码行注释 (GET /repos/{owner}/{repo}/check-runs/{id}/annotations)"""
    data, code = _request("GET", f"/repos/{owner}/{repo}/check-runs/{check_run_id}/annotations",
                          {"access_token": token})
    if data:
        if not data:
            print("  (无注释)")
        for ann in data:
            icon = ANNOTATION_ICON.get(ann.get("annotation_level", ""), "")
            print(f"  {icon} {ann['path']}:{ann['start_line']}-{ann['end_line']} {ann['message']}")
    return data, code


def push_checks_to_pr(token, owner, repo, pr_db_id, head_sha, checks, *,
                      default_details_url=None):
    """为 PR 批量推送检查结果（一次性 create completed，非 Jenkins 常用路径）。

    pr_db_id: PR 的数据库 ID (pull_request.id)，不是 PR 编号 (iid)。
    """
    if not checks:
        raise ValueError("checks 不能为空，请通过 --push-file 传入 JSON 列表")
    if default_details_url:
        fallback_details = default_details_url
    elif pr_db_id:
        fallback_details = f"https://gitee.com/{owner}/{repo}/pulls/{pr_db_id}"
    else:
        fallback_details = f"https://gitee.com/{owner}/{repo}"

    pr_label = f"db_id={pr_db_id}" if pr_db_id else "no pr_id"
    print(f"为 {owner}/{repo} PR ({pr_label}) @ {head_sha[:8]} 推送 {len(checks)} 个检查:")
    results = []
    for chk in checks:
        output = chk.get("output")
        if output and chk.get("log_artifact_url"):
            output = dict(output)
            output["log_artifact_url"] = chk["log_artifact_url"]
        data, code = create_check_run(
            token, owner, repo,
            name=chk["name"],
            head_sha=head_sha,
            pr_id=pr_db_id,
            conclusion=chk.get("conclusion", "success"),
            details_url=chk.get("details_url", fallback_details),
            output=output,
            annotations=chk.get("annotations"),
        )
        results.append((chk["name"], data, code))
    print()
    ok = sum(1 for _, _, c in results if 200 <= c < 300)
    print(f"完成: {ok}/{len(results)} 成功")
    if ok < len(results):
        for name, _, code in results:
            if not (200 <= code < 300):
                print(f"  WARNING: {name} HTTP {code}", file=sys.stderr)
    return results


# ── Jenkins: 按 stage 开始 / 结束 ─────────────────────────────

def _load_ids_map(ids_file):
    path = ids_file or DEFAULT_IDS_FILE
    if not os.path.exists(path):
        return path, {}
    with open(path, encoding="utf-8") as f:
        fcntl.flock(f.fileno(), fcntl.LOCK_SH)
        try:
            data = json.load(f)
        except json.JSONDecodeError:
            data = {}
        finally:
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    if not isinstance(data, dict):
        data = {}
    return path, data


def _save_ids_map(path, data):
    parent = os.path.dirname(os.path.abspath(path))
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            json.dump(data, f, indent=2, ensure_ascii=False)
            f.write("\n")
        finally:
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)


def _update_ids_map(ids_file, updater):
    """在独占锁内读-改-写 ids 文件，避免并行 stage 覆盖条目。"""
    path = ids_file or DEFAULT_IDS_FILE
    parent = os.path.dirname(os.path.abspath(path))
    if parent:
        os.makedirs(parent, exist_ok=True)
    data = {}
    if os.path.exists(path):
        with open(path, "r+", encoding="utf-8") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            try:
                if os.path.getsize(path):
                    f.seek(0)
                    data = json.load(f)
                if not isinstance(data, dict):
                    data = {}
                updater(data)
                f.seek(0)
                f.truncate()
                json.dump(data, f, indent=2, ensure_ascii=False)
                f.write("\n")
            finally:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    else:
        with open(path, "w", encoding="utf-8") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            try:
                updater(data)
                json.dump(data, f, indent=2, ensure_ascii=False)
                f.write("\n")
            finally:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    return path


def _merge_ids_record(ids_file, name, check_id):
    def _upd(data):
        data[name] = int(check_id)
    return _update_ids_map(ids_file, _upd)


def find_check_run_id_on_commit(token, owner, repo, head_sha, name):
    """按名称在 commit 上查找 check-run id（用于 ids 丢失时的补救）。"""
    matches = []
    page = 1
    while page <= 10:
        fields = {"access_token": token, "per_page": "100", "page": str(page)}
        data, code = _request(
            "GET", f"/repos/{owner}/{repo}/commits/{head_sha}/check-runs", fields,
        )
        if not data or not (200 <= code < 300):
            break
        runs = data.get("check_runs", []) if isinstance(data, dict) else []
        if not runs:
            break
        for run in runs:
            if run.get("name") == name and run.get("id") is not None:
                matches.append(run)
        total = data.get("total_count", len(runs)) if isinstance(data, dict) else len(runs)
        if page * 100 >= total:
            break
        page += 1
    if not matches:
        return None
    for run in matches:
        if run.get("status") == "in_progress":
            return int(run["id"])
    return int(max(matches, key=lambda r: int(r["id"]))["id"])


def resolve_check_run_id(token, owner, repo, head_sha, name, ids_file):
    _, id_map = _load_ids_map(ids_file)
    check_id = id_map.get(name)
    if check_id is not None:
        return int(check_id)
    if not (token and head_sha):
        return None
    found = find_check_run_id_on_commit(token, owner, repo, head_sha, name)
    if found is not None:
        print(f"  resolved {name!r} check-run id={found} from Gitee API", file=sys.stderr)
        _merge_ids_record(ids_file, name, found)
    return found


def start_checks_batch(token, owner, repo, names, *, head_sha, pr_id=None,
                       details_url=None, ids_file=None):
    """并行 stage 组：在单次文件锁内依次 create，避免与并行分支争用请求文件。"""
    path, id_map = _load_ids_map(ids_file)
    results = []
    for name in names:
        if name in id_map:
            print(f"  ⏭ {name}: already started (id={id_map[name]})")
            results.append((name, {"id": id_map[name]}, 200))
            continue
        output = {
            "title": name,
            "summary": f"## 🔄 {name} 进行中\n\nCI 正在执行此阶段…",
            "text": "",
        }
        data, code = create_check_run(
            token, owner, repo,
            name=name,
            head_sha=head_sha,
            pr_id=pr_id,
            status="in_progress",
            details_url=details_url,
            output=output,
        )
        if data and 200 <= code < 300 and data.get("id") is not None:
            id_map[name] = int(data["id"])
        results.append((name, data, code))
    _save_ids_map(path, id_map)
    ok = sum(1 for _, d, c in results if d and 200 <= c < 300)
    print(f"并行检查 batch start: {ok}/{len(names)} 成功")
    code = 200 if ok == len(names) else 207
    return results, code


def start_stage_check(token, owner, repo, *, name, head_sha, pr_id=None,
                      details_url=None, ids_file=None):
    """stage 开始时创建 in_progress 检查，并把 check-run id 记入 ids 文件。"""
    _, id_map = _load_ids_map(ids_file)
    if name in id_map:
        print(f"  ⏭ {name}: already started (id={id_map[name]})")
        return {"id": id_map[name]}, 200

    output = {
        "title": name,
        "summary": f"## 🔄 {name} 进行中\n\nCI 正在执行此阶段…",
        "text": "",
    }
    data, code = create_check_run(
        token, owner, repo,
        name=name,
        head_sha=head_sha,
        pr_id=pr_id,
        status="in_progress",
        details_url=details_url,
        output=output,
    )
    if data and 200 <= code < 300 and data.get("id") is not None:
        _merge_ids_record(ids_file, name, data["id"])
    return data, code


def finish_stage_check(token, owner, repo, *, name, conclusion, head_sha=None,
                       pr_id=None, details_url=None, output=None,
                       log_artifact_url=None, ids_file=None):
    """stage 结束时按 ids 文件（或 commit API 回查）更新检查结论。"""
    path, _ = _load_ids_map(ids_file)
    check_id = resolve_check_run_id(token, owner, repo, head_sha, name, ids_file)
    if check_id is None:
        print(f"ERROR: cannot resolve check-run id for stage {name!r} in {path}",
              file=sys.stderr)
        return None, 404

    output = output or {"title": name, "summary": "", "text": ""}
    if log_artifact_url:
        output = dict(output)
        output["log_artifact_url"] = log_artifact_url

    data, code = update_check_run(
        token, owner, repo, int(check_id),
        conclusion=conclusion,
        output=output,
    )
    return data, code


def handle_stage_request(token, owner, repo, request, *, pr_db_id=None,
                         default_details_url=None):
    """处理 Jenkins 写入的 JSON 请求 (action: start | start_batch | finish)。"""
    action = request.get("action")
    if not action:
        print("request missing action", file=sys.stderr)
        sys.exit(1)

    head_sha = request.get("head_sha")
    if not head_sha:
        print("request missing head_sha", file=sys.stderr)
        sys.exit(1)

    pr_id = request.get("pr_db_id", pr_db_id)
    details_url = request.get("details_url") or default_details_url
    ids_file = request.get("ids_file", DEFAULT_IDS_FILE)

    if action == "start_batch":
        names = request.get("names")
        if not names:
            print("start_batch missing names", file=sys.stderr)
            sys.exit(1)
        return start_checks_batch(
            token, owner, repo, names,
            head_sha=head_sha,
            pr_id=pr_id,
            details_url=details_url,
            ids_file=ids_file,
        )

    name = request.get("name")
    if not name:
        print("request missing name", file=sys.stderr)
        sys.exit(1)

    if action == "start":
        return start_stage_check(
            token, owner, repo,
            name=name,
            head_sha=head_sha,
            pr_id=pr_id,
            details_url=details_url,
            ids_file=ids_file,
        )

    if action == "finish":
        conclusion = request.get("conclusion", "success")
        output = request.get("output")
        log_artifact_url = request.get("log_artifact_url")
        return finish_stage_check(
            token, owner, repo,
            name=name,
            conclusion=conclusion,
            output=output,
            log_artifact_url=log_artifact_url,
            ids_file=ids_file,
        )

    print(f"unknown action: {action!r}", file=sys.stderr)
    sys.exit(1)


# ── 工具函数 ──────────────────────────────────────────────────

ANSI_CSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]", re.IGNORECASE)
ANSI_CSI_ALT_RE = re.compile(r"\x9b[0-9;?]*[ -/]*[@-~]", re.IGNORECASE)
ANSI_SGR_RE = re.compile(r"\[(?:\d{1,3};?)*[mK]")


def strip_ansi_escapes(content):
    if not content:
        return ""
    text = ANSI_CSI_RE.sub("", content)
    text = ANSI_CSI_ALT_RE.sub("", text)
    text = ANSI_SGR_RE.sub("", text)
    text = text.replace("\r", "")
    text = re.sub(r"\n{4,}", "\n\n\n", text)
    return text.strip()


def prepare_output_text(content, *, log_artifact_url=""):
    """去除 ANSI 转义并按 Gitee 上限截断（与 Jenkinsfile CI 一致）。"""
    content = strip_ansi_escapes(content or "")
    if not content:
        return ""
    encoded = content.encode("utf-8")
    if len(encoded) <= MAX_TEXT_BYTES:
        return content
    notice = ""
    if log_artifact_url:
        notice = (
            f"...(日志共 {len(encoded)} 字节，超过 Gitee 上限 {MAX_TEXT_BYTES} 字节，"
            f"以下为末尾片段。完整日志: {log_artifact_url})\n\n"
        )
    else:
        notice = f"...(已截断，共 {len(encoded)} 字节)\n\n"
    budget = MAX_TEXT_BYTES - len(notice.encode("utf-8"))
    if budget <= 0:
        return notice[:MAX_TEXT_BYTES]
    tail = encoded[-budget:]
    while tail:
        try:
            return notice + tail.decode("utf-8")
        except UnicodeDecodeError:
            tail = tail[1:]
    return notice


def _truncate(text):
    """截断 output text 到 Gitee 上限"""
    if not text:
        return ""
    encoded = text.encode("utf-8")
    if len(encoded) <= MAX_TEXT_BYTES:
        return text
    budget = MAX_TEXT_BYTES - 200
    tail = encoded[-budget:]
    while tail:
        try:
            return "...(已截断)\n\n" + tail.decode("utf-8")
        except UnicodeDecodeError:
            tail = tail[1:]
    return "...(已截断)"


def _print_check_run(r):
    """格式化打印单个 check-run"""
    print(f"  名称:   {r.get('name')}")
    print(f"  ID:     {r.get('id')}")
    print(f"  状态:   {r.get('status')}")
    print(f"  结论:   {r.get('conclusion', '-')}")
    print(f"  SHA:    {r.get('head_sha')}")
    print(f"  开始:   {r.get('started_at', '-')}")
    print(f"  完成:   {r.get('completed_at', '-')}")
    print(f"  链接:   {r.get('html_url', '-')}")
    output = r.get("output", {})
    if output:
        print(f"  报告:   {output.get('title', '-')}")
        print(f"  概要:   {output.get('summary', '-')}")


# ── Jenkins 流水线拓扑（由 Jenkinsfile 经 manifest 传入）────────────


def _require_stage_list(manifest, key):
    stages = manifest.get(key)
    if not isinstance(stages, list) or not stages:
        print(f"jenkins manifest missing or empty {key!r} "
              f"(set ciSequentialStageOrder/ciParallelStageOrder in Jenkinsfile)",
              file=sys.stderr)
        sys.exit(1)
    if not all(isinstance(s, str) and s.strip() for s in stages):
        print(f"jenkins manifest {key!r} must be a non-empty list of stage names",
              file=sys.stderr)
        sys.exit(1)
    return stages


def manifest_topology(manifest):
    """返回 (sequential_stages, parallel_stages, all_stages)。"""
    sequential = _require_stage_list(manifest, "sequential_stages")
    parallel = _require_stage_list(manifest, "parallel_stages")
    return sequential, parallel, sequential + parallel


def sanitize_stage_file_name(stage_name):
    safe = re.sub(r"[^A-Za-z0-9._-]+", "_", stage_name or "")
    return safe[:80]


def status_to_conclusion(status):
    if status == "passed":
        return "success"
    if status == "failed":
        return "failure"
    return "skipped"


def stage_log_path(root_ws, stage_name):
    root = (root_ws or ".").rstrip("/")
    return os.path.join(root, "stage-logs", f"{sanitize_stage_file_name(stage_name)}.log")


def read_stage_log_file(path):
    if not path or not os.path.isfile(path):
        return ""
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read().strip()
    except OSError:
        return ""


def resolve_failed_stage_log(stage_name, ci_result, failed_stage_logs, root_ws):
    """优先 failed_stage_logs，其次磁盘 stage-logs，最后 ci_result.detail。"""
    logs = failed_stage_logs if isinstance(failed_stage_logs, dict) else {}
    text = (logs.get(stage_name) or "").strip()
    if text:
        return strip_ansi_escapes(text)
    text = read_stage_log_file(stage_log_path(root_ws, stage_name))
    if text:
        return strip_ansi_escapes(text)
    detail = ""
    if isinstance(ci_result, dict):
        detail = (ci_result.get("detail") or "").strip()
    return strip_ansi_escapes(detail)


def _runtime_arch_from_stage(stage_name):
    """Parse arch from 'Runtime Test: kplat-<arch>' (arch is the suffix)."""
    prefix = "Runtime Test: kplat-"
    name = (stage_name or "").strip()
    if not name.startswith(prefix):
        return None
    arch = name[len(prefix):].strip()
    return arch or None


def _runtime_failed_cases_markdown(stage_name, details_url, root_ws, stage_log_url=""):
    """Best-effort failed-case links from harness last_run.json.

    ``root_ws`` follows the Jenkins manifest contract (``ciRootDir()``, i.e.
    ``<workspace>/.ci``). ``runtime_failed_cases`` also accepts a workspace
    root; both layouts are tried when locating last_run.json.
    """
    if not root_ws:
        return ""
    try:
        from runtime_failed_cases import collect_runtime_failed_case_markdown
    except ImportError:
        script_dir = os.path.dirname(os.path.abspath(__file__))
        if script_dir not in sys.path:
            sys.path.insert(0, script_dir)
        try:
            from runtime_failed_cases import collect_runtime_failed_case_markdown
        except ImportError:
            return ""
    try:
        return collect_runtime_failed_case_markdown(
            stage_name,
            root_ws=root_ws,
            build_url=details_url or "",
            stage_log_url=stage_log_url or "",
            arch=_runtime_arch_from_stage(stage_name),
        )
    except Exception as exc:  # noqa: BLE001 - best-effort enrichment only
        print(f"WARN: runtime failed-case links skipped: {exc}", file=sys.stderr)
        return ""



def build_stage_check_run_output(stage_name, ci_result, failed_stage_logs, details_url,
                                 root_ws=None):
    """生成 finish 时传给 Gitee 的 output 字段。"""
    status = "not_run"
    detail = "该阶段未执行，通常是前序阶段失败导致。请查看 Jenkins Stages 详情。"
    if isinstance(ci_result, dict):
        status = ci_result.get("status") or status
        detail = (ci_result.get("detail") or detail).strip()

    base = (details_url or "").rstrip("/")
    stages_url = f"{base}/stages/" if base else ""
    log_url = (
        f"{base}/artifact/.ci/stage-logs/"
        f"{sanitize_stage_file_name(stage_name)}.log"
        if base else ""
    )

    case_links = ""
    if status == "failed":
        case_links = _runtime_failed_cases_markdown(
            stage_name, details_url, root_ws, stage_log_url=log_url,
        )

    if status == "passed":
        summary = f"## ✅ {stage_name} 通过\n\n[查看 Jenkins Stages]({stages_url})"
    elif status == "failed":
        if case_links:
            summary = (
                f"## ❌ {stage_name} 失败\n\n{case_links}\n\n"
                f"[Jenkins Stages]({stages_url})"
            )
        else:
            summary = (
                f"## ❌ {stage_name} 失败\n\n{detail}\n\n"
                f"[阶段日志]({log_url}) | [Jenkins Stages]({stages_url})"
            )
    elif status == "skipped":
        summary = f"## ⏭ {stage_name} 已跳过\n\n{detail}\n\n[查看 Jenkins Stages]({stages_url})"
    else:
        summary = f"## ⏭ {stage_name} 未执行\n\n{detail}\n\n[查看 Jenkins Stages]({stages_url})"

    text = ""
    if status == "failed":
        log = resolve_failed_stage_log(stage_name, ci_result, failed_stage_logs, root_ws)
        if log:
            text = f"```\n{log}\n```"

    return {"title": stage_name, "summary": summary, "text": text}


def log_artifact_url(details_url, stage_name):
    base = (details_url or "").rstrip("/")
    if not base:
        return ""
    return (
        f"{base}/artifact/.ci/stage-logs/"
        f"{sanitize_stage_file_name(stage_name)}.log"
    )


def check_run_started(ids_file, stage_name):
    _, id_map = _load_ids_map(ids_file)
    return stage_name in id_map


def _resolve_pr_db_id(manifest, token, owner, repo, pr_db_id_fallback=None):
    pr_db_id = manifest.get("pr_db_id") or pr_db_id_fallback
    if pr_db_id:
        return pr_db_id
    pr_iid = manifest.get("pr_iid")
    if pr_iid and token and owner and repo:
        _, db_id = _resolve_pr(token, owner, repo, int(pr_iid))
        return db_id
    return None


def finish_stage_from_manifest(token, owner, repo, manifest, stage_name, *,
                               pr_db_id=None, override_ci_result=None):
    ci_results = manifest.get("ci_results") or {}
    ci_result = override_ci_result if override_ci_result is not None else ci_results.get(stage_name)
    status = "not_run"
    if isinstance(ci_result, dict):
        status = ci_result.get("status") or status

    head_sha = manifest.get("head_sha")
    details_url = manifest.get("details_url")
    ids_file = manifest.get("ids_file", DEFAULT_IDS_FILE)
    root_ws = manifest.get("root_ws")
    failed_logs = manifest.get("failed_stage_logs") or {}

    if status == "not_run":
        check_id = resolve_check_run_id(
            token, owner, repo, head_sha, stage_name, ids_file,
        )
        if check_id is None:
            return None, 0
        ci_result = {
            "status": "skipped",
            "detail": "CI 未上报该阶段结果，已自动关闭 Gitee 检查",
        }
        status = "skipped"
        print(f"  WARN: {stage_name!r} had no ci_results status; finishing as skipped",
              file=sys.stderr)

    output = build_stage_check_run_output(
        stage_name, ci_result, failed_logs, details_url, root_ws=root_ws,
    )
    log_url = ""
    if status == "failed":
        log_url = log_artifact_url(details_url, stage_name)

    return finish_stage_check(
        token, owner, repo,
        name=stage_name,
        conclusion=status_to_conclusion(status),
        output=output,
        log_artifact_url=log_url or None,
        ids_file=ids_file,
        head_sha=head_sha,
    )


def handle_jenkins_manifest(token, owner, repo, manifest, *, pr_db_id_fallback=None):
    """Jenkins gitee-ci-manifest.json 统一入口。"""
    action = manifest.get("action")
    if not action:
        print("jenkins manifest missing action", file=sys.stderr)
        sys.exit(1)

    head_sha = manifest.get("head_sha")
    if not head_sha:
        print("jenkins manifest missing head_sha", file=sys.stderr)
        sys.exit(1)

    pr_id = _resolve_pr_db_id(manifest, token, owner, repo, pr_db_id_fallback)
    details_url = manifest.get("details_url")
    ids_file = manifest.get("ids_file", DEFAULT_IDS_FILE)
    stage_name = manifest.get("stage_name")
    ci_results = manifest.get("ci_results") or {}
    failed_logs = manifest.get("failed_stage_logs") or {}

    if action == "start_parallel":
        _, parallel_stages, _ = manifest_topology(manifest)
        return start_checks_batch(
            token, owner, repo, parallel_stages,
            head_sha=head_sha,
            pr_id=pr_id,
            details_url=details_url,
            ids_file=ids_file,
        )

    if action == "start":
        if not stage_name:
            print("start requires stage_name", file=sys.stderr)
            sys.exit(1)
        return start_stage_check(
            token, owner, repo,
            name=stage_name,
            head_sha=head_sha,
            pr_id=pr_id,
            details_url=details_url,
            ids_file=ids_file,
        )

    if action == "ensure_start":
        if not stage_name:
            print("ensure_start requires stage_name", file=sys.stderr)
            sys.exit(1)
        if check_run_started(ids_file, stage_name):
            print(f"  ⏭ {stage_name}: check already started")
            return None, 200
        print(f"  creating missing check for {stage_name}")
        return start_stage_check(
            token, owner, repo,
            name=stage_name,
            head_sha=head_sha,
            pr_id=pr_id,
            details_url=details_url,
            ids_file=ids_file,
        )

    if action == "finish":
        if not stage_name:
            print("finish requires stage_name", file=sys.stderr)
            sys.exit(1)
        return finish_stage_from_manifest(
            token, owner, repo, manifest, stage_name, pr_db_id=pr_id,
        )

    if action == "post_finalize":
        _, _, all_stages = manifest_topology(manifest)
        _, id_map = _load_ids_map(ids_file)
        last_code = 200
        skip_detail = "阶段未执行或构建已中止（fail-fast / 手动停止）"
        for name in all_stages:
            check_id = id_map.get(name)
            if check_id is None:
                continue
            st = ci_stage_status(ci_results, name)
            in_progress = is_check_run_in_progress(
                token, owner, repo, int(check_id),
            )
            if st == "failed":
                detail = (ci_results.get(name) or {}).get("detail") or ""
                if is_fail_fast_abort_detail(detail):
                    override = {"status": "skipped", "detail": skip_detail}
                    _, code = finish_stage_from_manifest(
                        token, owner, repo, manifest, name,
                        pr_db_id=pr_id, override_ci_result=override,
                    )
                    if code and code >= 300:
                        last_code = code
                    continue
                if in_progress:
                    _, code = finish_stage_from_manifest(
                        token, owner, repo, manifest, name, pr_db_id=pr_id,
                    )
                    if code and code >= 300:
                        last_code = code
                continue
            if st == "passed":
                if not in_progress:
                    continue
                _, code = finish_stage_from_manifest(
                    token, owner, repo, manifest, name, pr_db_id=pr_id,
                )
            elif in_progress or st in ("not_run", "running", "skipped"):
                override = {"status": "skipped", "detail": skip_detail}
                _, code = finish_stage_from_manifest(
                    token, owner, repo, manifest, name,
                    pr_db_id=pr_id, override_ci_result=override,
                )
            else:
                continue
            if code and code >= 300:
                last_code = code
        return None, last_code

    if action == "refresh_failed":
        _, _, all_stages = manifest_topology(manifest)
        last_code = 200
        for name in all_stages:
            st = (ci_results.get(name) or {}).get("status")
            if st != "failed":
                continue
            if not check_run_started(ids_file, name):
                continue
            _, code = finish_stage_from_manifest(
                token, owner, repo, manifest, name, pr_db_id=pr_id,
            )
            if code and code >= 300:
                last_code = code
        return None, last_code

    if action == "reorder_sequential":
        sequential_stages, _, _ = manifest_topology(manifest)
        last_code = 200
        for name in reversed(sequential_stages):
            st = (ci_results.get(name) or {}).get("status")
            if not st or st == "not_run":
                continue
            if not check_run_started(ids_file, name):
                continue
            _, code = finish_stage_from_manifest(
                token, owner, repo, manifest, name, pr_db_id=pr_id,
            )
            if code and code >= 300:
                last_code = code
        return None, last_code

    print(f"unknown jenkins action: {action!r}", file=sys.stderr)
    sys.exit(1)


def _resolve_pr(token, owner, repo, pr_number):
    """通过 PR 编号 (iid) 查询 PR 详情，返回 (head_sha, pr_db_id)"""
    data, code = _request("GET", f"/repos/{owner}/{repo}/pulls/{pr_number}",
                          {"access_token": token})
    if code and 200 <= code < 300 and data:
        sha = data.get("head", {}).get("sha")
        db_id = data.get("id")
        return sha, db_id
    return None, None


# ── CLI 入口 ──────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser(
        description="Gitee Check-Runs CLI",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
示例:
  %(prog)s --owner O --repo R --jenkins-manifest manifest.json
  %(prog)s --owner O --repo R --list <commit_sha>
  %(prog)s --owner O --repo R --push-file checks.json --head-sha SHA --pr-db-id ID
        """,
    )
    p.add_argument("--owner", help="仓库所属空间 (用户名/组织)")
    p.add_argument("--repo", help="仓库名")

    p.add_argument("--token", help="个人令牌 (不推荐，check-run 可能不在 PR 检查页展示)")

    g = p.add_mutually_exclusive_group()
    g.add_argument("--list", metavar="SHA", help="查询某 commit 的所有检查")
    g.add_argument("--get", type=int, metavar="ID", help="查看某个检查详情")
    g.add_argument("--annotations", type=int, metavar="ID", help="查看某个检查的行级注释")
    g.add_argument("--update", type=int, metavar="ID", help="更新某个检查的状态")
    g.add_argument("--push-file", metavar="FILE",
                   help="从 JSON 一次性批量 create 检查 (需 --head-sha)")

    p.add_argument("--pr", type=int,
                   help="PR 编号 (iid)，配合 --push-file/--jenkins-manifest 解析 pr_db_id 或 head_sha")
    p.add_argument("--head-sha", help="commit SHA")
    p.add_argument("--pr-db-id", type=int, help="PR 数据库 ID (pull_request.id)")
    p.add_argument("--details-url", help="检查详情链接 (如 Jenkins BUILD_URL)")
    p.add_argument("--conclusion", default="success",
                   choices=["success", "failure", "cancelled", "timed_out",
                            "action_required", "neutral", "skipped", "stale"],
                   help="检查结论 (配合 --update)")
    p.add_argument("--request-file", metavar="FILE",
                   help="低层 API 请求 JSON (action: start|finish|start_batch)")
    p.add_argument("--jenkins-manifest", metavar="FILE",
                   help="Jenkins 门禁检查 manifest (action: start|finish|start_parallel|...)")

    args = p.parse_args()

    # ── 需要 owner/repo 的操作 ──
    needs_repo = any([
        args.list, args.get, args.annotations, args.update,
        args.push_file, args.request_file, args.jenkins_manifest,
    ])
    if needs_repo and not (args.owner and args.repo):
        p.error("this action requires --owner and --repo")

    if not needs_repo:
        p.error("no action specified; use --jenkins-manifest, --pr, --push-file, ...")

    token = _resolve_token(args)

    if args.jenkins_manifest:
        if args.list or args.get or args.annotations or args.update:
            p.error("--jenkins-manifest cannot combine with --list/--get/--update")
        with open(args.jenkins_manifest, encoding="utf-8") as f:
            manifest = json.load(f)
        owner = manifest.get("owner") or args.owner
        repo = manifest.get("repo") or args.repo
        if args.head_sha and not manifest.get("head_sha"):
            manifest["head_sha"] = args.head_sha
        if args.details_url and not manifest.get("details_url"):
            manifest["details_url"] = args.details_url
        pr_db_id = args.pr_db_id
        if not pr_db_id and args.pr:
            _, pr_db_id = _resolve_pr(token, owner, repo, args.pr)
        _, code = handle_jenkins_manifest(
            token, owner, repo, manifest, pr_db_id_fallback=pr_db_id,
        )
        action = manifest.get("action", "")
        if code and not (200 <= code < 300) and action not in (
            "finish", "post_finalize", "refresh_failed", "reorder_sequential",
        ):
            sys.exit(1)
        if action == "start_parallel":
            path, id_map = _load_ids_map(manifest.get("ids_file", DEFAULT_IDS_FILE))
            print(f"Gitee parallel check ids: {', '.join(sorted(id_map.keys()))}")
        return

    if args.request_file:
        if args.list or args.get or args.annotations or args.update:
            p.error("--request-file cannot be combined with --list, --get, --annotations, or --update")
        pr_db_id = args.pr_db_id
        if not pr_db_id and args.pr:
            _, pr_db_id = _resolve_pr(token, args.owner, args.repo, args.pr)
        with open(args.request_file, encoding="utf-8") as f:
            request = json.load(f)
        if args.head_sha and not request.get("head_sha"):
            request["head_sha"] = args.head_sha
        if args.pr_db_id and not request.get("pr_db_id"):
            request["pr_db_id"] = args.pr_db_id
        data, code = handle_stage_request(
            token, args.owner, args.repo, request,
            pr_db_id=pr_db_id,
            default_details_url=args.details_url,
        )
        # finish 缺 id 时 code=0；start_batch 部分失败返回 207，不令 Jenkins 构建失败
        if code and not (200 <= code < 300) and request.get("action") != "finish":
            sys.exit(1)
        return

    if args.push_file:
        if args.list or args.get or args.annotations or args.update:
            p.error("--push-file cannot be combined with --list, --get, --annotations, or --update")
        if not args.head_sha:
            p.error("--push-file requires --head-sha")
        pr_db_id = args.pr_db_id
        if not pr_db_id and args.pr:
            _, pr_db_id = _resolve_pr(token, args.owner, args.repo, args.pr)
        if not pr_db_id:
            print("WARNING: 未指定 --pr-db-id，check-run 可能不会显示在 PR 检查页", file=sys.stderr)
        with open(args.push_file, encoding="utf-8") as f:
            checks = json.load(f)
        push_checks_to_pr(
            token, args.owner, args.repo, pr_db_id, args.head_sha, checks,
            default_details_url=args.details_url,
        )
        return

    if args.list:
        list_check_runs(token, args.owner, args.repo, args.list)

    elif args.get:
        get_check_run(token, args.owner, args.repo, args.get)

    elif args.annotations:
        get_annotations(token, args.owner, args.repo, args.annotations)

    elif args.update:
        update_check_run(token, args.owner, args.repo, args.update,
                         conclusion=args.conclusion)


if __name__ == "__main__":
    main()
