"""Configuration and stable artifact names for one local scan."""

from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Optional


RESULT_SCHEMA = "gpu-driver-code-scan/result/v1"
MANIFEST_SCHEMA = "gpu-driver-code-scan/manifest/v1"
PLAN_SCHEMA = "gpu-driver-code-scan/group-plan/v1"

RESULT_FILE = "scan-result.json"
MANIFEST_FILE = "manifest.json"
INVENTORY_FILE = "inventory.json"
GROUP_PLAN_FILE = "group-plan.json"
PATCH_MAP_FILE = "patch-map.json"
FINDINGS_FILE = "findings.json"
EXCLUDED_FINDINGS_FILE = "excluded-findings.json"
REPORT_FILE = "report.md"
METRICS_FILE = "metrics.jsonl"


@dataclass
class ScanConfig:
    source_dir: Path
    output_dir: Path
    project: str = ""
    driver_url: str = ""
    kernel_url: str = ""
    provider: str = ""
    model: str = ""
    sashiko_dir: Optional[Path] = None
    review_bin: Optional[Path] = None
    prompts_dir: Optional[Path] = None
    concurrency: int = 3
    max_findings: int = 10
    max_files_per_group: int = 30
    max_lines_per_group: int = 1000
    max_bytes_per_group: int = 100000
    max_review_seconds: int = 7200
    review_timeout_seconds: int = 3600
    include_globs: List[str] = field(default_factory=list)
    stages: str = "3,4,5,6,7"
    plan_only: bool = False
    no_ai: bool = False
    allow_existing_output: bool = False
