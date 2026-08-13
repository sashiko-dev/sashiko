"""Directory-aware grouping with GPU-driver risk prioritization."""

import collections
import re
from pathlib import Path

from .models import PLAN_SCHEMA


RISK_PATTERNS = (
    (80, r"uvm|unified memory|hmm|mmu_interval|page.?fault|replayable|access.?counter", "memory/uvm/fault"),
    (70, r"nvlink|nvswitch|xgmi|p2p|peer.?mem|rdma|gpudirect|ats", "interconnect/p2p/rdma"),
    (65, r"mmap|vm_operations_struct|remap_pfn_range|vm_fault|vm_area_struct", "mmap/vm"),
    (60, r"\bdma_|\bdma\s|\bsg_table\b|scatterlist|iommu|pci_map|pci_unmap", "dma/iommu"),
    (55, r"unlocked_ioctl|compat_ioctl|\bioctl\b|copy_from_user|copy_to_user|get_user|put_user", "userspace api"),
    (50, r"channel|queue|runlist|pushbuffer|compute|cuda|gpfifo|fifo|fault|doorbell", "compute/channel/fault"),
    (40, r"spin_lock|mutex_|rwlock|atomic_|refcount_|kref_|rcu_", "locking/lifetime"),
    (35, r"workqueue|queue_work|init_work|timer_setup|kthread|notifier", "async/lifetime"),
    (-80, r"drm|modeset|display|connector|encoder|crtc|fbdev|backlight|headsurface", "display/graphics"),
)
GENERATED_HEADER = re.compile(
    r"(^|/)(cl[a-z0-9_]*|ctrl[0-9a-z_]*|g_[a-z0-9_]*|hwref/.*)\.h$",
    re.I,
)


def read_source(path):
    return Path(path).read_text(encoding="utf-8", errors="ignore")[: 1024 * 1024]


def source_kind(relative):
    if GENERATED_HEADER.search(relative):
        return "generated_header"
    suffix = Path(relative).suffix
    if suffix == ".c":
        return "implementation"
    if suffix == ".h":
        return "header"
    return "source"


def file_detail(source_dir, relative):
    path = Path(source_dir) / relative
    text = read_source(path)
    lines = path.read_bytes().splitlines(keepends=True)
    kind = source_kind(relative)
    score = {
        "implementation": 120,
        "source": 40,
        "header": -15,
        "generated_header": -180,
    }[kind]
    reasons = collections.Counter({"kind:%s" % kind: 1})
    scoring_text = (relative + "\n" + text).lower()
    for points, pattern, reason in RISK_PATTERNS:
        matches = re.findall(pattern, scoring_text, re.I)
        if matches:
            count = max(1, len(matches))
            score += points * count
            reasons[reason] += count
    return {
        "path": relative,
        "bytes": path.stat().st_size,
        "lines": max(1, len(lines)),
        "score": score,
        "reasons": reasons,
        "source_kind": kind,
    }


def split_file_regions(source_dir, detail, max_lines, max_bytes):
    path = Path(source_dir) / detail["path"]
    lines = path.read_bytes().splitlines(keepends=True)
    if not lines:
        lines = [b""]
    regions = []
    start = 1
    current_lines = 0
    current_bytes = 0
    for line_number, line in enumerate(lines, 1):
        if len(line) > max_bytes:
            raise RuntimeError(
                "%s line %s is %s bytes, larger than max_bytes_per_group=%s"
                % (detail["path"], line_number, len(line), max_bytes)
            )
        if current_lines and (
            current_lines >= max_lines or current_bytes + len(line) > max_bytes
        ):
            regions.append(
                dict(
                    detail,
                    start_line=start,
                    end_line=line_number - 1,
                    lines=current_lines,
                    bytes=current_bytes,
                )
            )
            start = line_number
            current_lines = 0
            current_bytes = 0
        current_lines += 1
        current_bytes += len(line)
    regions.append(
        dict(
            detail,
            start_line=start,
            end_line=len(lines),
            lines=current_lines,
            bytes=current_bytes,
        )
    )
    return regions


def split_details(source_dir, details, max_files, max_lines, max_bytes):
    chunks = []
    current = []
    current_lines = 0
    current_bytes = 0
    for detail in sorted(details, key=lambda item: item["path"]):
        for region in split_file_regions(
            source_dir, detail, max_lines, max_bytes
        ):
            next_file_count = len(
                {item["path"] for item in current} | {region["path"]}
            )
            if current and (
                next_file_count > max_files
                or current_lines + region["lines"] > max_lines
                or current_bytes + region["bytes"] > max_bytes
            ):
                chunks.append(current)
                current = []
                current_lines = 0
                current_bytes = 0
            current.append(region)
            current_lines += region["lines"]
            current_bytes += region["bytes"]
    if current:
        chunks.append(current)
    return chunks


def render_group(group_id, details):
    reasons = collections.Counter()
    source_kinds = collections.Counter()
    for detail in details:
        reasons.update(detail["reasons"])
        source_kinds[detail["source_kind"]] += 1
    files = list(dict.fromkeys(item["path"] for item in details))
    return {
        "group_id": group_id,
        "files": files,
        "file_count": len(files),
        "target_regions": [
            {
                "path": item["path"],
                "start_line": item["start_line"],
                "end_line": item["end_line"],
                "lines": item["lines"],
                "bytes": item["bytes"],
            }
            for item in details
        ],
        "lines": sum(item["lines"] for item in details),
        "bytes": sum(item["bytes"] for item in details),
        "score": sum(item["score"] for item in details) + min(len(details) * 2, 20),
        "reasons": [
            "%s x%s" % (name, count) for name, count in reasons.most_common()
        ],
        "source_kinds": dict(sorted(source_kinds.items())),
    }


def plan_groups(
    source_dir,
    files,
    max_files=30,
    max_lines=1000,
    max_bytes=100000,
):
    families = collections.defaultdict(list)
    details_by_path = {}
    for relative in files:
        directory = Path(relative).parent.as_posix()
        detail = file_detail(source_dir, relative)
        details_by_path[relative] = detail
        families["family:%s" % directory].append(detail)

    groups = []
    for family, details in sorted(families.items()):
        chunks = split_details(
            source_dir, details, max_files, max_lines, max_bytes
        )
        for index, chunk in enumerate(chunks, 1):
            group_id = family if len(chunks) == 1 else "%s#%03d" % (family, index)
            groups.append(render_group(group_id, chunk))

    groups.sort(key=lambda group: (-group["score"], group["group_id"]))
    region_assignments = collections.defaultdict(list)
    for rank, group in enumerate(groups, 1):
        group["priority_rank"] = rank
        for region in group["target_regions"]:
            region_assignments[region["path"]].append(
                {
                    "group_id": group["group_id"],
                    "start_line": region["start_line"],
                    "end_line": region["end_line"],
                }
            )

    inventory = set(files)
    assigned = set(region_assignments)
    duplicate_regions = []
    incomplete_regions = []
    for relative in sorted(inventory):
        expected_start = 1
        for region in sorted(
            region_assignments.get(relative, []),
            key=lambda item: (item["start_line"], item["end_line"]),
        ):
            if region["start_line"] < expected_start:
                duplicate_regions.append(relative)
            if region["start_line"] > expected_start:
                incomplete_regions.append(relative)
            expected_start = max(expected_start, region["end_line"] + 1)
        total_lines = details_by_path[relative]["lines"]
        if expected_start != total_lines + 1:
            incomplete_regions.append(relative)
    assignments = {
        relative: regions[0]["group_id"]
        for relative, regions in sorted(region_assignments.items())
        if regions
    }
    return {
        "schema": PLAN_SCHEMA,
        "inventory_file_count": len(files),
        "group_count": len(groups),
        "groups": groups,
        "assignments": assignments,
        "region_assignments": dict(region_assignments),
        "coverage": {
            "unassigned_files": sorted(inventory - assigned),
            "duplicate_primary_assignments": sorted(set(duplicate_regions)),
            "extra_assignments": sorted(assigned - inventory),
            "incomplete_region_assignments": sorted(set(incomplete_regions)),
        },
    }
