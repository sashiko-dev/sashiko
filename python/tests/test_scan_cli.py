import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

from gpu_driver_code_scan.cli import build_parser


ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOT = ROOT / "src"


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


class ScanCliTest(unittest.TestCase):
    def setUp(self):
        self.temporary = Path(tempfile.mkdtemp(prefix="gpu_driver_code_scan_test_"))
        self.source = self.temporary / "driver"
        self.output = self.temporary / "artifacts"
        self.sashiko = self.temporary / "sashiko"
        self.review = self.sashiko / "target" / "release" / "review"
        self.prompts = self.sashiko / "third_party" / "prompts" / "kernel"
        self.prompts.mkdir(parents=True)
        write(self.prompts / "review-core.md", "# review\n")
        write(
            self.source / "uvm" / "driver.c",
            textwrap.dedent(
                """\
                long driver_ioctl(void __user *arg, unsigned long count)
                {
                    char buffer[16];
                    if (copy_from_user(buffer, arg, count))
                        return -EFAULT;
                    return 0;
                }
                """
            ),
        )
        write(
            self.source / "uvm" / "driver.h",
            "long driver_ioctl(void __user *arg, unsigned long count);\n",
        )
        write(
            self.source / "display" / "panel.c",
            "int panel_enable(void) { return 0; }\n",
        )
        write(
            self.source / "tests" / "driver_test.c",
            "int driver_test(void) { return 0; }\n",
        )
        self.write_fake_review()

    def tearDown(self):
        shutil.rmtree(self.temporary)

    def write_fake_review(self, fail=False):
        body = """\
            #!/usr/bin/env python3
            import json
            import os
            import sys

            if "--help" in sys.argv:
                raise SystemExit(0)
            assert "--skip-report-stage" in sys.argv
            payload = json.loads(sys.stdin.readline())
            patch = payload["patches"][0]
            assert "diff --git" in patch["diff"]
            if os.environ.get("FAKE_REVIEW_FAIL") == "1":
                print("forced failure", file=sys.stderr)
                raise SystemExit(7)
            findings_per_group = int(
                os.environ.get("FAKE_REVIEW_FINDINGS_PER_GROUP", "0")
            )
            findings = []
            if findings_per_group:
                source_file = (
                    "uvm/driver.c"
                    if "uvm/driver.c" in patch["diff"]
                    else "display/panel.c"
                )
                findings = [
                    {
                        "title": "Finding %d in %s" % (index, source_file),
                        "problem": "A reportable test finding.",
                        "severity": "High",
                        "locations": [{"file": source_file}],
                    }
                    for index in range(1, findings_per_group + 1)
                ]
            elif "uvm/driver.c" in patch["diff"]:
                findings = [{
                    "title": "Unchecked copy_from_user length can overflow buffer",
                    "problem": "count is not checked against the 16-byte buffer.",
                    "severity": "High",
                    "severity_explanation": "A userspace caller can overwrite stack memory.",
                    "path": "ioctl -> driver_ioctl -> copy_from_user",
                    "locations": [{
                        "file": "uvm/driver.c",
                        "function_or_symbol": "driver_ioctl",
                        "code_snippet": "if (copy_from_user(buffer, arg, count))",
                        "why_this_location_matters": "count controls the copy into buffer.",
                    }],
                }]
            print(json.dumps({
                "tokens_in": 30,
                "tokens_out": 4,
                "tokens_cached": 20,
                "review": {"findings": findings},
            }))
        """
        write(self.review, textwrap.dedent(body))
        self.review.chmod(self.review.stat().st_mode | stat.S_IXUSR)

    def test_public_provider_defaults_to_codex_cli(self):
        previous_provider = os.environ.pop("SASHIKO__AI__PROVIDER", None)
        previous_model = os.environ.pop("SASHIKO__AI__MODEL", None)
        try:
            args = build_parser().parse_args(
                ["scan", str(self.source), "--output-dir", str(self.output)]
            )
        finally:
            if previous_provider is not None:
                os.environ["SASHIKO__AI__PROVIDER"] = previous_provider
            if previous_model is not None:
                os.environ["SASHIKO__AI__MODEL"] = previous_model

        self.assertEqual("codex-cli", args.provider)
        self.assertEqual("gpt-5.5-2026-04-24", args.model)
        self.assertEqual(3, args.concurrency)

    def test_public_provider_can_be_overridden_by_environment(self):
        previous_provider = os.environ.get("SASHIKO__AI__PROVIDER")
        previous_model = os.environ.get("SASHIKO__AI__MODEL")
        os.environ["SASHIKO__AI__PROVIDER"] = "custom-provider"
        os.environ["SASHIKO__AI__MODEL"] = "custom-model"
        try:
            args = build_parser().parse_args(
                ["scan", str(self.source), "--output-dir", str(self.output)]
            )
        finally:
            if previous_provider is None:
                os.environ.pop("SASHIKO__AI__PROVIDER", None)
            else:
                os.environ["SASHIKO__AI__PROVIDER"] = previous_provider
            if previous_model is None:
                os.environ.pop("SASHIKO__AI__MODEL", None)
            else:
                os.environ["SASHIKO__AI__MODEL"] = previous_model

        self.assertEqual("custom-provider", args.provider)
        self.assertEqual("custom-model", args.model)

    def command(self, *extra):
        command = [
            sys.executable,
            "-m",
            "gpu_driver_code_scan.cli",
            "scan",
            str(self.source),
            "--output-dir",
            str(self.output),
            "--project",
            "Example GPU Driver",
            "--driver-url",
            "https://example.test/driver",
            "--kernel-url",
            "https://example.test/kernel",
            "--sashiko-dir",
            str(self.sashiko),
            "--review-bin",
            str(self.review),
            "--prompts-dir",
            str(self.prompts),
            "--concurrency",
            "2",
        ]
        command.extend(extra)
        environment = os.environ.copy()
        environment["PYTHONPATH"] = str(SOURCE_ROOT)
        return command, environment

    def run_scan(self, *extra, environment_updates=None):
        command, environment = self.command(*extra)
        environment.update(environment_updates or {})
        return subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )

    def validate(self):
        environment = os.environ.copy()
        environment["PYTHONPATH"] = str(SOURCE_ROOT)
        return subprocess.run(
            [
                sys.executable,
                "-m",
                "gpu_driver_code_scan.cli",
                "validate",
                str(self.output),
            ],
            cwd=ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )

    def test_end_to_end_scan_writes_stable_contract(self):
        before = {
            path.relative_to(self.source).as_posix(): path.read_bytes()
            for path in self.source.rglob("*")
            if path.is_file()
        }

        process = self.run_scan()

        self.assertEqual(0, process.returncode, process.stdout + process.stderr)
        response = json.loads(process.stdout)
        self.assertTrue(response["ok"])
        self.assertEqual("gpu-driver-code-scan/result/v1", response["schema"])
        self.assertEqual("full_inventory_reviewed", response["completion"]["reason"])
        self.assertEqual(1, response["finding_count"])
        self.assertEqual(2, response["completion"]["total_groups"])

        result = json.loads((self.output / "scan-result.json").read_text())
        self.assertEqual(response, result)
        inventory = json.loads((self.output / "inventory.json").read_text())
        self.assertEqual(
            ["display/panel.c", "uvm/driver.c", "uvm/driver.h"],
            inventory["files"],
        )
        plan = json.loads((self.output / "group-plan.json").read_text())
        assigned = [
            relative
            for group in plan["groups"]
            for relative in group["files"]
        ]
        self.assertEqual(sorted(inventory["files"]), sorted(assigned))
        self.assertEqual(len(assigned), len(set(assigned)))
        self.assertEqual([], plan["coverage"]["unassigned_files"])
        self.assertEqual("family:uvm", plan["groups"][0]["group_id"])

        patch_map = json.loads((self.output / "patch-map.json").read_text())
        self.assertEqual(2, len(patch_map["patches"]))
        first_input = json.loads(
            (self.output / "review-inputs" / "group-000001.json").read_text()
        )
        diff = first_input["patches"][0]["diff"]
        self.assertIn("uvm/driver.c", diff)
        self.assertIn("uvm/driver.h", diff)
        self.assertNotIn("display/panel.c", diff)

        report = (self.output / "report.md").read_text()
        self.assertIn("# Example GPU Driver Scan Report", report)
        self.assertIn("## Detailed Findings", report)
        self.assertIn("| Driver | [https://example.test/driver]", report)
        self.assertIn("| Kernel context | [https://example.test/kernel]", report)
        self.assertIn(
            "| Finding count | 1 total; critical 0, high 1, medium 0, low 0 |",
            report,
        )
        self.assertNotIn("Source directory", report)
        self.assertNotIn("Scan coverage", report)
        self.assertIn("uvm/driver.c:4 (driver_ioctl)", report)
        self.assertIn("```c", report)
        self.assertIn("if (copy_from_user(buffer, arg, count))", report)
        self.assertIn("count controls the copy into buffer.", report)

        after = {
            path.relative_to(self.source).as_posix(): path.read_bytes()
            for path in self.source.rglob("*")
            if path.is_file()
        }
        self.assertEqual(before, after)
        self.assertFalse((self.source / ".git").exists())

        validation = self.validate()
        self.assertEqual(0, validation.returncode, validation.stdout + validation.stderr)
        self.assertTrue(json.loads(validation.stdout)["ok"])

    def test_plan_only_does_not_require_sashiko(self):
        self.review.unlink()
        process = self.run_scan("--plan-only")

        self.assertEqual(0, process.returncode, process.stdout + process.stderr)
        result = json.loads(process.stdout)
        self.assertTrue(result["ok"])
        self.assertEqual("plan_only", result["completion"]["reason"])
        self.assertFalse((self.output / "reviews").exists())
        self.assertTrue((self.output / "report.md").is_file())

    def test_failed_group_produces_non_success_result(self):
        process = self.run_scan(
            "--max-files-per-group",
            "1",
            environment_updates={"FAKE_REVIEW_FAIL": "1"},
        )

        self.assertEqual(1, process.returncode, process.stdout + process.stderr)
        result = json.loads(process.stdout)
        self.assertFalse(result["ok"])
        self.assertEqual("failed_groups", result["completion"]["reason"])
        self.assertEqual(3, result["completion"]["total_groups"])
        self.assertEqual(2, result["completion"]["failed_groups"])
        self.assertEqual(
            2,
            len((self.output / "metrics.jsonl").read_text().splitlines()),
        )
        validation = self.validate()
        self.assertEqual(1, validation.returncode)

    def test_finding_limit_stops_scheduling_without_truncating_inflight_results(self):
        process = self.run_scan(
            "--max-findings",
            "1",
            environment_updates={"FAKE_REVIEW_FINDINGS_PER_GROUP": "2"},
        )

        self.assertEqual(0, process.returncode, process.stdout + process.stderr)
        result = json.loads(process.stdout)
        self.assertEqual("finding_limit_reached", result["completion"]["reason"])
        self.assertEqual(4, result["finding_count"])
        self.assertEqual(1, result["completion"]["max_findings"])
        self.assertEqual(
            4, len(json.loads((self.output / "findings.json").read_text()))
        )
        self.assertEqual(
            [], json.loads((self.output / "excluded-findings.json").read_text())
        )
        self.assertIn("Finding 2 in display/panel.c", (self.output / "report.md").read_text())

    def test_nonempty_output_is_rejected(self):
        self.output.mkdir(parents=True)
        write(self.output / "old.json", "{}\n")

        process = self.run_scan()

        self.assertEqual(2, process.returncode)
        self.assertIn("must be empty", json.loads(process.stdout)["error"])


if __name__ == "__main__":
    unittest.main()
