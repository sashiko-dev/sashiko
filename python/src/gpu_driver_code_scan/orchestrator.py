"""End-to-end local scan orchestration and artifact contract."""

import json
import shutil
import threading
import time
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from pathlib import Path

from . import findings as finding_tools
from .grouping import plan_groups
from .inventory import copy_snapshot, enumerate_files
from .io import append_jsonl, git, write_json
from .models import (
    EXCLUDED_FINDINGS_FILE,
    FINDINGS_FILE,
    GROUP_PLAN_FILE,
    INVENTORY_FILE,
    MANIFEST_FILE,
    MANIFEST_SCHEMA,
    METRICS_FILE,
    PATCH_MAP_FILE,
    REPORT_FILE,
    RESULT_FILE,
    RESULT_SCHEMA,
)
from .report import render
from .sashiko import resolve_dependency, run_review
from .synthetic import build_patch, initialize_repository, public_patch, review_input


def ensure_output_directory(path, allow_existing=False):
    path = Path(path).expanduser().resolve()
    if path.exists() and not path.is_dir():
        raise RuntimeError("output path is not a directory: %s" % path)
    if path.exists() and any(path.iterdir()) and not allow_existing:
        raise RuntimeError("output directory must be empty: %s" % path)
    path.mkdir(parents=True, exist_ok=True)
    return path


def validate_source(path):
    path = Path(path).expanduser().resolve()
    if not path.exists():
        raise RuntimeError("source directory does not exist: %s" % path)
    if not path.is_dir():
        raise RuntimeError("source path is not a directory: %s" % path)
    return path


def sashiko_commit(sashiko_dir):
    try:
        return git(sashiko_dir, "rev-parse", "HEAD")
    except RuntimeError:
        return ""


def review_metric(patch, review):
    result = review.get("result") or {}
    raw_findings = ((result.get("review") or {}).get("findings") or [])
    return {
        "group_index": patch["index"],
        "group_id": patch["group_id"],
        "target_files": patch["target_files"],
        "status": "ok" if not review.get("error") else "failed",
        "wall_seconds": review.get("wall_seconds") or 0,
        "tokens_in": result.get("tokens_in") or 0,
        "tokens_out": result.get("tokens_out") or 0,
        "tokens_cached": result.get("tokens_cached") or 0,
        "finding_count": len(raw_findings),
        "error": review.get("error") or "",
        "stdout_path": review.get("stdout_path") or "",
        "stderr_path": review.get("stderr_path") or "",
    }


def raw_findings(results):
    values = []
    for result in results:
        review = result.get("review") or {}
        output = review.get("result") or {}
        for finding in ((output.get("review") or {}).get("findings") or []):
            if isinstance(finding, dict):
                item = dict(finding)
                item.setdefault("source_group", result["patch"]["group_id"])
                values.append(item)
    return values


def completion_reason(config, total_groups, reviewed_groups, failed_groups, stopped):
    if config.plan_only:
        return "plan_only"
    if failed_groups:
        return "failed_groups"
    if stopped == "finding_limit_reached":
        return stopped
    if stopped == "time_limit_reached":
        return stopped
    if reviewed_groups == total_groups:
        return "full_inventory_reviewed"
    return stopped or "incomplete"


def run_scans(config, repo_dir, output_dir, patch_iterator, dependency):
    sashiko_dir, review_bin, prompts_dir = dependency
    reviews_dir = output_dir / "reviews"
    inputs_dir = output_dir / "review-inputs"
    reviews_dir.mkdir(parents=True, exist_ok=True)
    inputs_dir.mkdir(parents=True, exist_ok=True)
    metrics_path = output_dir / METRICS_FILE
    if metrics_path.exists():
        metrics_path.unlink()

    state_lock = threading.Lock()
    results = []
    finding_count = 0
    stopped = ""
    started = time.time()

    def should_stop():
        if config.max_findings > 0 and finding_count >= config.max_findings:
            return "finding_limit_reached"
        if (
            config.max_review_seconds > 0
            and time.time() - started >= config.max_review_seconds
        ):
            return "time_limit_reached"
        return ""

    def review_one(patch):
        payload = review_input(config.project, patch)
        write_json(
            inputs_dir / ("group-%06d.json" % patch["index"]),
            payload,
        )
        review = run_review(
            repo_dir,
            patch,
            payload,
            sashiko_dir,
            review_bin,
            prompts_dir,
            output_dir,
            provider=config.provider,
            model=config.model,
            stages=config.stages,
            no_ai=config.no_ai,
            timeout_seconds=config.review_timeout_seconds,
        )
        write_json(
            reviews_dir / ("group-%06d.json" % patch["index"]),
            {"patch": public_patch(patch), "review": review},
        )
        return {"patch": public_patch(patch), "review": review}

    if config.plan_only:
        return [], "", 0

    executor = ThreadPoolExecutor(max_workers=max(1, config.concurrency))
    futures = {}

    def submit():
        nonlocal stopped
        while len(futures) < max(1, config.concurrency):
            reason = should_stop()
            if reason:
                stopped = reason
                return
            try:
                patch = next(patch_iterator)
            except StopIteration:
                return
            futures[executor.submit(review_one, patch)] = patch

    try:
        submit()
        while futures:
            done, _pending = wait(
                list(futures), timeout=1.0, return_when=FIRST_COMPLETED
            )
            if not done:
                reason = should_stop()
                if reason:
                    stopped = reason
                continue
            for future in done:
                patch = futures.pop(future)
                try:
                    result = future.result()
                except Exception as exception:
                    result = {
                        "patch": public_patch(patch),
                        "review": {
                            "returncode": 1,
                            "error": str(exception),
                            "result": None,
                        },
                    }
                metric = review_metric(patch, result["review"])
                with state_lock:
                    results.append(result)
                    finding_count += metric["finding_count"]
                    append_jsonl(metrics_path, metric)
                if metric["status"] == "failed":
                    stopped = "failed_groups"
            reason = should_stop()
            if reason:
                stopped = reason
            if not stopped:
                submit()
    finally:
        executor.shutdown(wait=True)

    results.sort(key=lambda item: item["patch"]["index"])
    return results, stopped, finding_count


def patch_iterator(repo_dir, output_dir, full_commit, groups):
    full_tree = git(repo_dir, "rev-parse", "%s^{tree}" % full_commit)
    for index, group in enumerate(groups, 1):
        yield build_patch(repo_dir, output_dir, full_commit, full_tree, group, index)


def build_result(config, project, source_dir, output_dir, group_plan, reviews, stopped):
    metrics = [review_metric(item["patch"], item["review"]) for item in reviews]
    failed_groups = sum(1 for item in metrics if item["status"] == "failed")
    reviewed_groups = sum(1 for item in metrics if item["status"] == "ok")
    total_groups = group_plan["group_count"]
    raw = raw_findings(reviews)
    findings, excluded = finding_tools.normalize(raw)
    findings = finding_tools.enrich_locations(findings, source_dir)
    reason = completion_reason(
        config, total_groups, reviewed_groups, failed_groups, stopped
    )
    ok = reason in {
        "full_inventory_reviewed",
        "finding_limit_reached",
        "plan_only",
    }
    result = {
        "schema": RESULT_SCHEMA,
        "ok": ok,
        "project": project,
        "source_dir": str(source_dir),
        "output_dir": str(output_dir),
        "finding_count": len(findings),
        "completion": {
            "reason": reason,
            "total_groups": total_groups,
            "reviewed_groups": reviewed_groups,
            "failed_groups": failed_groups,
            "remaining_groups": max(0, total_groups - reviewed_groups - failed_groups),
            "max_findings": config.max_findings,
        },
        "artifacts": {
            "manifest": MANIFEST_FILE,
            "inventory": INVENTORY_FILE,
            "group_plan": GROUP_PLAN_FILE,
            "patch_map": PATCH_MAP_FILE,
            "findings": FINDINGS_FILE,
            "excluded_findings": EXCLUDED_FINDINGS_FILE,
            "report": REPORT_FILE,
            "metrics": METRICS_FILE,
        },
    }
    return result, findings, excluded, metrics


def run_scan(config):
    source_dir = validate_source(config.source_dir)
    output_dir = ensure_output_directory(
        config.output_dir, allow_existing=config.allow_existing_output
    )
    project = config.project.strip() or source_dir.name or "GPU Driver"
    config.project = project
    files = enumerate_files(source_dir, config.include_globs)
    if not files:
        raise RuntimeError("no reviewable source files found under %s" % source_dir)

    plan = plan_groups(
        source_dir,
        files,
        config.max_files_per_group,
        config.max_lines_per_group,
        config.max_bytes_per_group,
    )
    coverage = plan["coverage"]
    if any(coverage.values()):
        write_json(output_dir / GROUP_PLAN_FILE, plan)
        raise RuntimeError("group coverage validation failed: %s" % coverage)

    snapshot_dir = output_dir / "snapshot-repository"
    copy_snapshot(source_dir, snapshot_dir)
    full_commit = initialize_repository(snapshot_dir)

    write_json(
        output_dir / INVENTORY_FILE,
        {
            "source_dir": str(source_dir),
            "file_count": len(files),
            "files": files,
        },
    )
    write_json(output_dir / GROUP_PLAN_FILE, plan)

    dependency = None
    if not config.plan_only:
        dependency = resolve_dependency(
            config.sashiko_dir, config.review_bin, config.prompts_dir
        )
    reviews, stopped, _raw_count = (
        run_scans(
            config,
            snapshot_dir,
            output_dir,
            patch_iterator(snapshot_dir, output_dir, full_commit, plan["groups"]),
            dependency,
        )
        if dependency
        else ([], "", 0)
    )
    reviewed_patches = [item["patch"] for item in reviews]
    write_json(
        output_dir / PATCH_MAP_FILE,
        {
            "mode": "grouped-snapshot",
            "full_snapshot_commit": full_commit,
            "patches": reviewed_patches,
            "reviewed_patch_count": len(reviewed_patches),
            "total_group_count": plan["group_count"],
        },
    )
    result, findings, excluded, metrics = build_result(
        config, project, source_dir, output_dir, plan, reviews, stopped
    )
    write_json(output_dir / FINDINGS_FILE, findings)
    write_json(output_dir / EXCLUDED_FINDINGS_FILE, excluded)

    manifest = {
        "schema": MANIFEST_SCHEMA,
        "project": project,
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "source": {
            "directory": str(source_dir),
            "url": config.driver_url,
            "file_count": len(files),
        },
        "kernel_context": {
            "url": config.kernel_url,
        },
        "policy": {
            "concurrency": config.concurrency,
            "max_findings": config.max_findings,
            "max_files_per_group": config.max_files_per_group,
            "max_lines_per_group": config.max_lines_per_group,
            "max_bytes_per_group": config.max_bytes_per_group,
            "max_review_seconds": config.max_review_seconds,
            "review_timeout_seconds": config.review_timeout_seconds,
            "stages": config.stages,
            "no_ai": config.no_ai,
        },
        "sashiko": {
            "directory": str(dependency[0]) if dependency else "",
            "commit": sashiko_commit(dependency[0]) if dependency else "",
            "review_binary": str(dependency[1]) if dependency else "",
            "prompts": str(dependency[2]) if dependency else "",
            "provider": config.provider,
            "model": config.model,
        },
        "snapshot": {
            "full_commit": full_commit,
            "group_count": plan["group_count"],
            "coverage": plan["coverage"],
        },
        "completion": result["completion"],
        "metrics": metrics,
    }
    write_json(output_dir / MANIFEST_FILE, manifest)
    report = render(
        project,
        source_dir,
        result,
        findings,
        driver_url=config.driver_url,
        kernel_url=config.kernel_url,
    )
    (output_dir / REPORT_FILE).write_text(report, encoding="utf-8")
    write_json(output_dir / RESULT_FILE, result)
    shutil.rmtree(str(output_dir / ".indexes"), ignore_errors=True)
    return result
