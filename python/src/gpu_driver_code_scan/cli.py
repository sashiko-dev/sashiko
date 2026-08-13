"""Command-line interface for scanning and validating local directories."""

import argparse
import json
import os
import signal
import sys
from pathlib import Path

from .models import ScanConfig
from .orchestrator import run_scan
from .sashiko import reset_shutdown_state, terminate_active_reviews
from .validation import validate_output


def positive_integer(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return number


def nonnegative_integer(value):
    number = int(value)
    if number < 0:
        raise argparse.ArgumentTypeError("must be zero or greater")
    return number


def add_scan_arguments(parser):
    parser.add_argument("source_dir", help="existing local driver source directory")
    parser.add_argument(
        "--output-dir", required=True, help="empty directory for scan artifacts"
    )
    parser.add_argument("--project", default="")
    parser.add_argument(
        "--driver-url",
        default="",
        help="optional source locator shown as Driver in the report",
    )
    parser.add_argument(
        "--kernel-url",
        default="",
        help="optional kernel context locator shown in the report",
    )
    parser.add_argument(
        "--provider", default=os.environ.get("SASHIKO__AI__PROVIDER", "codex-cli")
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("SASHIKO__AI__MODEL", "gpt-5.5-2026-04-24"),
    )
    parser.add_argument("--sashiko-dir", default="")
    parser.add_argument("--review-bin", default="")
    parser.add_argument("--prompts-dir", default="")
    parser.add_argument("--concurrency", type=positive_integer, default=3)
    parser.add_argument(
        "--max-findings",
        type=nonnegative_integer,
        default=10,
        help=(
            "stop scheduling new groups after completed reviews reach this finding "
            "count; findings from all already-started groups remain in the report"
        ),
    )
    parser.add_argument("--max-files-per-group", type=positive_integer, default=30)
    parser.add_argument("--max-lines-per-group", type=positive_integer, default=1000)
    parser.add_argument("--max-bytes-per-group", type=positive_integer, default=100000)
    parser.add_argument("--max-review-seconds", type=nonnegative_integer, default=7200)
    parser.add_argument("--review-timeout-seconds", type=positive_integer, default=3600)
    parser.add_argument("--include", action="append", default=[])
    parser.add_argument("--stages", default="3,4,5,6,7")
    parser.add_argument("--plan-only", action="store_true")
    parser.add_argument(
        "--no-ai",
        action="store_true",
        help="exercise Sashiko patch validation without model review",
    )
    parser.add_argument("--allow-existing-output", action="store_true")


def build_parser():
    parser = argparse.ArgumentParser(
        prog="gpu-driver-code-scan",
        description="Scan an existing local Linux GPU driver source directory with Sashiko.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    scan_parser = subparsers.add_parser("scan", help="run one local source scan")
    add_scan_arguments(scan_parser)
    validate_parser = subparsers.add_parser(
        "validate", help="validate a completed artifact directory"
    )
    validate_parser.add_argument("output_dir")
    return parser


def scan_config(args):
    return ScanConfig(
        source_dir=Path(args.source_dir),
        output_dir=Path(args.output_dir),
        project=args.project,
        driver_url=args.driver_url,
        kernel_url=args.kernel_url,
        provider=args.provider,
        model=args.model,
        sashiko_dir=Path(args.sashiko_dir) if args.sashiko_dir else None,
        review_bin=Path(args.review_bin) if args.review_bin else None,
        prompts_dir=Path(args.prompts_dir) if args.prompts_dir else None,
        concurrency=args.concurrency,
        max_findings=args.max_findings,
        max_files_per_group=args.max_files_per_group,
        max_lines_per_group=args.max_lines_per_group,
        max_bytes_per_group=args.max_bytes_per_group,
        max_review_seconds=args.max_review_seconds,
        review_timeout_seconds=args.review_timeout_seconds,
        include_globs=args.include,
        stages=args.stages,
        plan_only=args.plan_only,
        no_ai=args.no_ai,
        allow_existing_output=args.allow_existing_output,
    )


def main(argv=None):
    args = build_parser().parse_args(argv)
    previous_handlers = {}

    def stop_scan(signum, _frame):
        terminate_active_reviews()
        raise RuntimeError("scan cancelled by signal %s" % signum)

    try:
        if args.command == "validate":
            result = validate_output(args.output_dir)
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0 if result["ok"] else 1
        reset_shutdown_state()
        for signum in (signal.SIGINT, signal.SIGTERM):
            previous_handlers[signum] = signal.getsignal(signum)
            signal.signal(signum, stop_scan)
        result = run_scan(scan_config(args))
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0 if result["ok"] else 1
    except Exception as exception:
        print(
            json.dumps(
                {
                    "schema": "gpu-driver-code-scan/error/v1",
                    "ok": False,
                    "error": str(exception),
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 2
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)


if __name__ == "__main__":
    sys.exit(main())
