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

import json
import os
import stat
import tempfile
import threading
import time
import unittest
from pathlib import Path

from codebase_scan.sashiko import (
    reset_shutdown_state,
    run_review,
    terminate_active_reviews,
)


def process_running(pid):
    try:
        stat_text = Path("/proc/%d/stat" % pid).read_text()
    except OSError:
        return False
    return stat_text.rsplit(") ", 1)[1].split()[0] != "Z"


class SashikoProcessTest(unittest.TestCase):
    def test_failed_review_terminates_orphaned_process_group(self):
        with tempfile.TemporaryDirectory(prefix="repository_scan_failure_") as temporary:
            root = Path(temporary)
            review_bin = root / "review"
            pid_file = root / "pids.json"
            review_bin.write_text(
                """#!/usr/bin/env python3
import json
import os
import subprocess
import sys

sys.stdin.readline()
child = subprocess.Popen([
    sys.executable,
    "-c",
    "import time; time.sleep(300)",
])
with open(os.environ["REPOSITORY_SCAN_TEST_PID_FILE"], "w") as handle:
    json.dump({"parent": os.getpid(), "child": child.pid}, handle)
raise SystemExit(7)
""",
                encoding="utf-8",
            )
            review_bin.chmod(review_bin.stat().st_mode | stat.S_IXUSR)
            prompts = root / "prompts"
            prompts.mkdir()
            output = root / "output"
            output.mkdir()
            previous = os.environ.get("REPOSITORY_SCAN_TEST_PID_FILE")
            os.environ["REPOSITORY_SCAN_TEST_PID_FILE"] = str(pid_file)
            reset_shutdown_state()

            try:
                result = run_review(
                    root,
                    {
                        "index": 1,
                        "baseline_commit": "baseline",
                        "target_commit": "target",
                    },
                    {"patches": []},
                    root,
                    review_bin,
                    prompts,
                    output,
                    timeout_seconds=300,
                )
            finally:
                if previous is None:
                    os.environ.pop("REPOSITORY_SCAN_TEST_PID_FILE", None)
                else:
                    os.environ["REPOSITORY_SCAN_TEST_PID_FILE"] = previous

            self.assertEqual(7, result["returncode"])
            pids = json.loads(pid_file.read_text())
            deadline = time.time() + 5
            while any(process_running(pid) for pid in pids.values()) and time.time() < deadline:
                time.sleep(0.05)
            self.assertFalse(process_running(pids["parent"]))
            self.assertFalse(process_running(pids["child"]))

    def test_cancel_terminates_review_process_group(self):
        with tempfile.TemporaryDirectory(prefix="repository_scan_cancel_") as temporary:
            root = Path(temporary)
            review_bin = root / "review"
            pid_file = root / "pids.json"
            review_bin.write_text(
                """#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time

sys.stdin.readline()
child = subprocess.Popen([
    sys.executable,
    "-c",
    "import time; time.sleep(300)",
])
with open(os.environ["REPOSITORY_SCAN_TEST_PID_FILE"], "w") as handle:
    json.dump({"parent": os.getpid(), "child": child.pid}, handle)
time.sleep(300)
""",
                encoding="utf-8",
            )
            review_bin.chmod(review_bin.stat().st_mode | stat.S_IXUSR)
            prompts = root / "prompts"
            prompts.mkdir()
            output = root / "output"
            output.mkdir()
            previous = os.environ.get("REPOSITORY_SCAN_TEST_PID_FILE")
            os.environ["REPOSITORY_SCAN_TEST_PID_FILE"] = str(pid_file)
            reset_shutdown_state()
            result = {}

            def invoke():
                result.update(
                    run_review(
                        root,
                        {
                            "index": 1,
                            "baseline_commit": "baseline",
                            "target_commit": "target",
                        },
                        {"patches": []},
                        root,
                        review_bin,
                        prompts,
                        output,
                        timeout_seconds=300,
                    )
                )

            thread = threading.Thread(target=invoke)
            thread.start()
            deadline = time.time() + 10
            while not pid_file.is_file() and time.time() < deadline:
                time.sleep(0.05)
            self.assertTrue(pid_file.is_file())
            pids = json.loads(pid_file.read_text())

            terminate_active_reviews()
            thread.join(timeout=10)

            self.assertFalse(thread.is_alive())
            self.assertNotEqual(0, result["returncode"])
            deadline = time.time() + 5
            while any(process_running(pid) for pid in pids.values()) and time.time() < deadline:
                time.sleep(0.05)
            self.assertFalse(process_running(pids["parent"]))
            self.assertFalse(process_running(pids["child"]))
            reset_shutdown_state()
            if previous is None:
                os.environ.pop("REPOSITORY_SCAN_TEST_PID_FILE", None)
            else:
                os.environ["REPOSITORY_SCAN_TEST_PID_FILE"] = previous


if __name__ == "__main__":
    unittest.main()
