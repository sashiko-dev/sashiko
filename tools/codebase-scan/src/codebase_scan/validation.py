# Copyright 2026 The Sashiko Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Validate the public artifact contract without running a scan."""

from pathlib import Path

from .io import read_json
from .models import (
    FINDINGS_FILE,
    GROUP_PLAN_FILE,
    MANIFEST_FILE,
    MANIFEST_SCHEMA,
    REPORT_FILE,
    RESULT_FILE,
    RESULT_SCHEMA,
)
from .report import REQUIRED_SECTIONS


def validate_output(output_dir):
    output_dir = Path(output_dir).expanduser().resolve()
    errors = []
    result = read_json(output_dir / RESULT_FILE, {}) or {}
    manifest = read_json(output_dir / MANIFEST_FILE, {}) or {}
    plan = read_json(output_dir / GROUP_PLAN_FILE, {}) or {}
    findings = read_json(output_dir / FINDINGS_FILE, None)
    report_path = output_dir / REPORT_FILE

    if result.get("schema") != RESULT_SCHEMA:
        errors.append("invalid or missing result schema")
    if manifest.get("schema") != MANIFEST_SCHEMA:
        errors.append("invalid or missing manifest schema")
    coverage = plan.get("coverage") or {}
    for key in (
        "unassigned_files",
        "duplicate_primary_assignments",
        "extra_assignments",
        "incomplete_region_assignments",
    ):
        if coverage.get(key):
            errors.append("group coverage contains %s" % key)
    if not isinstance(findings, list):
        errors.append("findings.json is missing or is not an array")
    if not report_path.is_file():
        errors.append("report.md is missing")
    else:
        report = report_path.read_text(encoding="utf-8", errors="replace")
        for section in REQUIRED_SECTIONS:
            if section not in report:
                errors.append("report.md is missing %s" % section)

    completion = result.get("completion") or {}
    reason = completion.get("reason") or ""
    if result.get("ok") and reason not in {
        "full_inventory_reviewed",
        "finding_limit_reached",
        "plan_only",
        "no_ai_smoke_test",
    }:
        errors.append("successful result has invalid completion reason: %s" % reason)
    if completion.get("failed_groups"):
        errors.append("scan contains failed groups")

    return {
        "schema": "codebase-scan/validation/v1",
        "ok": not errors,
        "output_dir": str(output_dir),
        "errors": errors,
        "finding_count": len(findings) if isinstance(findings, list) else 0,
        "completion": completion,
    }
