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

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install-skill"


class SkillInstallTest(unittest.TestCase):
    def setUp(self):
        self.temporary = Path(
            tempfile.mkdtemp(prefix="codebase_scan_skill_install_")
        )

    def tearDown(self):
        shutil.rmtree(self.temporary)

    def install(self, *extra):
        return subprocess.run(
            [sys.executable, str(INSTALLER), "--target", str(self.temporary), *extra],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )

    def test_installer_packages_public_skill_runtime(self):
        result = self.install()
        self.assertEqual(0, result.returncode, result.stderr)

        skill_root = self.temporary / "skills" / "codebase-scan"
        self.assertTrue((skill_root / "SKILL.md").is_file())
        self.assertTrue((skill_root / "agents" / "openai.yaml").is_file())
        self.assertTrue(
            (
                skill_root
                / "src"
                / "codebase_scan"
                / "templates"
                / "report.md"
            ).is_file()
        )
        self.assertTrue(
            (
                skill_root
                / "sashiko"
                / "Settings.toml"
            ).is_file()
        )
        self.assertTrue(
            (
                skill_root
                / "sashiko"
                / "third_party"
                / "prompts"
                / "kernel"
                / "review-core.md"
            ).is_file()
        )
        review_binary = skill_root / "bin" / "review"
        self.assertTrue(review_binary.is_file())
        self.assertTrue(os.access(str(review_binary), os.X_OK))
        self.assertFalse((skill_root / "sashiko" / "Cargo.toml").exists())
        self.assertFalse((skill_root / "sashiko" / "skills").exists())
        self.assertFalse(
            (skill_root / "sashiko" / "third_party" / "linux").exists()
        )

    def test_installed_skill_runs_plan_only_scan(self):
        result = self.install()
        self.assertEqual(0, result.returncode, result.stderr)

        skill_root = self.temporary / "skills" / "codebase-scan"
        source_root = self.temporary / "driver"
        output_root = self.temporary / "artifacts"
        source_root.mkdir()
        (source_root / "driver.c").write_text(
            "int gpu_driver_probe(void) { return 0; }\n", encoding="utf-8"
        )
        environment = os.environ.copy()
        environment["PYTHONPATH"] = str(skill_root / "src")
        scan = subprocess.run(
            [
                sys.executable,
                "-m",
                "codebase_scan.cli",
                "scan",
                str(source_root),
                "--output-dir",
                str(output_root),
                "--plan-only",
            ],
            cwd=skill_root,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertEqual(0, scan.returncode, scan.stderr or scan.stdout)
        self.assertTrue((output_root / "scan-result.json").is_file())
        self.assertTrue((output_root / "group-plan.json").is_file())

    def test_installed_skill_runs_sashiko_no_ai_scan(self):
        result = self.install()
        self.assertEqual(0, result.returncode, result.stderr)

        skill_root = self.temporary / "skills" / "codebase-scan"
        source_root = self.temporary / "driver-no-ai"
        output_root = self.temporary / "artifacts-no-ai"
        source_root.mkdir()
        (source_root / "driver.c").write_text(
            "int gpu_driver_probe(void) { return 0; }\n", encoding="utf-8"
        )
        environment = os.environ.copy()
        environment["PYTHONPATH"] = str(skill_root / "src")
        scan = subprocess.run(
            [
                sys.executable,
                "-m",
                "codebase_scan.cli",
                "scan",
                str(source_root),
                "--output-dir",
                str(output_root),
                "--no-ai",
            ],
            cwd=skill_root,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        self.assertEqual(0, scan.returncode, scan.stderr or scan.stdout)
        self.assertTrue((output_root / "scan-result.json").is_file())
        self.assertTrue((output_root / "reviews" / "group-000001.json").is_file())


if __name__ == "__main__":
    unittest.main()
