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

"""Render the scanner-owned local Markdown report."""

from pathlib import Path
from urllib.parse import urlparse

from . import findings as finding_tools


REQUIRED_SECTIONS = (
    "## Executive Summary",
    "## Risk Overview",
    "## Detailed Findings",
)


def template_text():
    return (Path(__file__).resolve().parent / "templates" / "report.md").read_text(
        encoding="utf-8"
    )


def severity_counts(findings):
    counts = {"critical": 0, "high": 0, "medium": 0, "low": 0}
    for finding in findings:
        severity = str(finding.get("severity") or "").lower()
        if severity in counts:
            counts[severity] += 1
    return counts


def location_label(finding):
    labels = []
    for location in finding.get("locations") or []:
        if not isinstance(location, dict):
            continue
        path = finding_tools.location_file(location, finding)
        if not path:
            continue
        line = location.get("line") or ""
        label = path + (":%s" % line if line else "")
        symbol = (
            location.get("function_or_symbol")
            or location.get("symbol")
            or location.get("function")
            or ""
        )
        if symbol:
            label += " (%s)" % symbol
        if label not in labels:
            labels.append(label)
    return ", ".join(labels) or "-"


def location_code(location):
    return str(
        location.get("code_snippet")
        or location.get("source_context")
        or ""
    ).strip()


def comment_text(value):
    return finding_tools.one_line(value).replace("*/", "* /")


def key_code_snippet(finding):
    locations = [
        location
        for location in finding.get("locations") or []
        if isinstance(location, dict)
    ]
    if not locations:
        body = str(finding.get("code_snippet") or "").strip() or "-"
        return "```c\n%s\n```" % body.replace("```", "'''")

    sections = []
    for location in locations:
        label = location_label({"locations": [location]})
        why = str(location.get("why_this_location_matters") or "").strip()
        body = location_code(location)
        section = ["/* %s */" % comment_text(label)]
        if why:
            section.append("/* %s */" % comment_text(why))
        section.append(body.replace("```", "'''") if body else "/* No code snippet available. */")
        sections.append("\n".join(section))
    return "```c\n%s\n```" % "\n\n".join(sections)


def risk_rows(findings):
    if not findings:
        return "| - | - | No reportable findings. |"
    rows = []
    for index, finding in enumerate(findings, 1):
        rows.append(
            "| %d | %s | %s |"
            % (
                index,
                finding.get("severity") or "-",
                finding_tools.title(finding).replace("|", "\\|"),
            )
        )
    return "\n".join(rows)


def detail_sections(findings):
    if not findings:
        return "No reportable findings."
    sections = []
    for index, finding in enumerate(findings, 1):
        sections.append(
            """### {index}. [{severity}] {title}

- Problem: {problem}
- Key code:

{code}

- Suggested fix: {fix}
""".format(
                index=index,
                severity=finding.get("severity") or "-",
                title=finding_tools.title(finding),
                problem=(
                    finding.get("problem")
                    or finding.get("evidence")
                    or finding.get("bad_behavior")
                    or "-"
                ),
                code=key_code_snippet(finding),
                fix=finding.get("suggested_fix") or finding_tools.fallback_fix(finding),
            )
        )
    return "\n".join(sections).strip()


def report_link(value, fallback="-"):
    value = str(value or "").strip()
    if not value:
        return fallback
    parsed = urlparse(value)
    if parsed.scheme in {"http", "https"} and parsed.netloc:
        escaped = value.replace("(", "\\(").replace(")", "\\)")
        return "[%s](%s)" % (value, escaped)
    return value


def render(project, source_dir, result, findings, source_url="", reference_url=""):
    counts = severity_counts(findings)
    values = {
        "{{project}}": project,
        "{{source_link}}": report_link(source_url, str(source_dir)),
        "{{reference_context_link}}": report_link(reference_url),
        "{{finding_count}}": str(len(findings)),
        "{{critical_count}}": str(counts["critical"]),
        "{{high_count}}": str(counts["high"]),
        "{{medium_count}}": str(counts["medium"]),
        "{{low_count}}": str(counts["low"]),
        "{{risk_rows}}": risk_rows(findings),
        "{{details}}": detail_sections(findings),
    }
    report = template_text()
    for marker, value in values.items():
        report = report.replace(marker, value)
    missing = [section for section in REQUIRED_SECTIONS if section not in report]
    if missing:
        raise RuntimeError("report template missing required sections: %s" % missing)
    return report.rstrip() + "\n"
