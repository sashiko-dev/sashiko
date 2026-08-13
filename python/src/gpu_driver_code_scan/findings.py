"""Finding normalization, quality filtering, and source location enrichment."""

import copy
import re
from pathlib import Path


COSMETIC_TERMS = (
    "comment typo",
    "documentation typo",
    "spelling",
    "misspell",
    "cosmetic",
    "grammar",
)
SYNTHETIC_HISTORY_TERMS = (
    "baseline does not contain",
    "not present in the baseline",
    "newly added",
    "newly introduced",
    "introduced by this patch",
    "reviewed patch",
    "reviewed patchset",
    "later patches",
    "provided series",
)


def one_line(value):
    return re.sub(r"\s+", " ", str(value or "")).strip()


def is_reportable(finding):
    severity = str(finding.get("severity") or "").lower()
    body = " ".join(
        str(finding.get(key) or "") for key in ("title", "problem", "evidence")
    ).lower()
    return not (severity in {"", "info", "informational", "low"} and any(term in body for term in COSMETIC_TERMS))


def fallback_fix(finding):
    locations = [
        location
        for location in finding.get("locations") or []
        if isinstance(location, dict)
    ]
    location = locations[0] if locations else {}
    symbol = (
        location.get("function_or_symbol")
        or location.get("symbol")
        or location.get("function")
        or "the affected function"
    )
    path = location_file(location, finding) or "the affected source file"
    prefix = "In %s (%s), " % (symbol, path)
    body = " ".join(
        str(finding.get(key) or "")
        for key in ("title", "problem", "path", "severity_explanation")
    ).lower()
    if "overflow" in body or "wrap" in body:
        action = (
            "replace the reported size or offset expression with checked arithmetic, "
            "return an error before the allocation, mapping, or copy when it overflows"
        )
    elif "use-after-free" in body or "lifetime" in body:
        action = (
            "keep the named object owned or referenced until every asynchronous user "
            "has completed, and cancel or flush the reported work before releasing it"
        )
    elif "race" in body or "toctou" in body or "without holding" in body:
        action = (
            "protect the reported state read and its use with the same named lock or "
            "a stable snapshot, and revalidate that snapshot before the operation"
        )
    elif "double-free" in body or "double free" in body:
        action = (
            "clear each ownership pointer immediately after ownership is transferred or "
            "released, and make the shared error unwind free only still-owned entries"
        )
    elif "leak" in body or "not cleaned up" in body:
        action = (
            "route the reported failure through one cleanup path that releases the exact "
            "allocation or reference acquired by this operation"
        )
    elif "copy_from_user" in body or "out-of-bounds" in body or "buffer" in body:
        action = (
            "validate the reported userspace-controlled index or length against the exact "
            "destination bound before dereferencing or copying"
        )
    else:
        action = (
            "change the reported operation so its stated ownership, ordering, bounds, "
            "or error-unwind invariant is preserved on the documented trigger path"
        )
    return prefix + action + "."


def strip_synthetic_history(value):
    text = str(value or "").strip()
    if not text:
        return text
    text = re.sub(
        r"(?i)^this problem (?:wasn't|was not) introduced by "
        r"(?:this|the reviewed) patch(?:set)?, but\s*",
        "",
        text,
    )
    text = re.sub(r"(?i)\bnewly (?:added|introduced)\b\s*", "", text)
    sentences = re.split(r"(?<=[.!?])\s+", text)
    retained = [
        sentence
        for sentence in sentences
        if not any(term in sentence.lower() for term in SYNTHETIC_HISTORY_TERMS)
    ]
    return " ".join(retained).strip()


def normalize_snapshot_finding(finding):
    for key in (
        "title",
        "problem",
        "evidence",
        "bad_behavior",
        "severity_explanation",
        "path",
        "trigger_path",
    ):
        if finding.get(key):
            finding[key] = strip_synthetic_history(finding[key])
    finding.pop("preexisting", None)
    finding.pop("patch_index", None)
    finding.pop("patch_subject", None)
    return finding


def normalize(raw_findings):
    reportable = []
    excluded = []
    for raw in raw_findings:
        if not isinstance(raw, dict):
            continue
        finding = normalize_snapshot_finding(copy.deepcopy(raw))
        if not is_reportable(finding):
            excluded.append(finding)
            continue
        if not finding.get("suggested_fix"):
            finding["suggested_fix"] = fallback_fix(finding)
        reportable.append(finding)
    return reportable, excluded


def location_file(location, finding):
    return (
        location.get("file")
        or location.get("path")
        or finding.get("file")
        or finding.get("path")
        or ""
    )


def source_path(source_dir, relative):
    if not relative:
        return None
    direct = Path(source_dir) / relative.lstrip("/")
    if direct.is_file():
        return direct
    basename = Path(relative).name
    for candidate in Path(source_dir).rglob(basename):
        if candidate.is_file() and candidate.as_posix().endswith(relative):
            return candidate
    return None


def snippet_candidates(snippet):
    candidates = []
    for line in str(snippet or "").splitlines():
        line = re.sub(r"^\d+\s*:\s*", "", line.strip())
        if line and line != "...":
            candidates.append(re.sub(r"\s+", " ", line))
    return candidates


def resolve_line(path, snippet, hint=0):
    if not path or not snippet:
        return ""
    source_lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    candidates = snippet_candidates(snippet)
    matches = []
    for candidate in candidates:
        if len(candidate) < 8:
            continue
        for index, line in enumerate(source_lines, 1):
            comparable = re.sub(r"\s+", " ", line.strip())
            if candidate == comparable or candidate in comparable:
                matches.append(index)
        if matches:
            break
    if not matches:
        return ""
    if hint:
        return str(min(matches, key=lambda index: abs(index - hint)))
    return str(matches[0])


def enrich_locations(findings, source_dir):
    enriched = copy.deepcopy(findings)
    for finding in enriched:
        locations = finding.get("locations")
        if not isinstance(locations, list):
            locations = []
            finding["locations"] = locations
        for location in locations:
            if not isinstance(location, dict):
                continue
            path = source_path(source_dir, location_file(location, finding))
            line = location.get("line") or ""
            try:
                hint = int(str(line).split("-", 1)[0]) if line else 0
            except ValueError:
                hint = 0
            resolved_line = resolve_line(
                path,
                location.get("code_snippet") or finding.get("code_snippet") or "",
                hint=hint,
            )
            if resolved_line:
                line = resolved_line
                location["line"] = int(line)
            if path and line:
                try:
                    number = int(str(line).split("-", 1)[0])
                except ValueError:
                    continue
                lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
                begin = max(1, number - 3)
                end = min(len(lines), number + 3)
                location["source_context"] = "\n".join(
                    "%d: %s" % (current, lines[current - 1])
                    for current in range(begin, end + 1)
                )
    return enriched


def title(finding):
    return one_line(
        finding.get("title")
        or finding.get("problem")
        or finding.get("evidence")
        or "Untitled finding"
    )
