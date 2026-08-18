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

import unittest
from pathlib import Path

from codebase_scan.report import key_code_snippet, render


class ReportRenderingTest(unittest.TestCase):
    def test_report_matches_legacy_external_template(self):
        findings = [
            {
                "problem": "count can overflow the destination buffer.",
                "severity": "High",
                "locations": [
                    {
                        "file": "uvm/driver.c",
                        "line": 42,
                        "function_or_symbol": "driver_ioctl",
                        "source_context": "42: copy_from_user(buffer, arg, count);",
                    }
                ],
                "severity_explanation": "A userspace caller controls count.",
                "suggested_fix": "Reject count values larger than the buffer.",
            }
        ]

        report = render(
            "Example Repository",
            Path("/tmp/local-source"),
            {},
            findings,
            source_url="https://example.test/source",
            reference_url="https://example.test/reference",
        )

        self.assertIn("| Source | [https://example.test/source]", report)
        self.assertIn("| Reference context | [https://example.test/reference]", report)
        self.assertIn(
            "| Finding count | 1 total; critical 0, high 1, medium 0, low 0 |",
            report,
        )
        self.assertIn("| # | Severity | Summary |", report)
        self.assertIn(
            "| 1 | High | count can overflow the destination buffer. |",
            report,
        )
        risk_section = report.split("## Risk Overview", 1)[1].split(
            "## Detailed Findings", 1
        )[0]
        self.assertNotIn("Location", risk_section)
        self.assertNotIn("uvm/driver.c:42", risk_section)
        self.assertNotIn("Source directory", report)
        self.assertNotIn("Scan coverage", report)
        self.assertIn("### 1. [High] count can overflow the destination buffer.", report)
        self.assertIn("- Problem: count can overflow the destination buffer.", report)
        self.assertNotIn("- Location:", report)
        self.assertNotIn("- Trigger path:", report)
        self.assertIn("/* uvm/driver.c:42 (driver_ioctl) */", report)
        self.assertNotIn("Location:", report)
        self.assertNotIn("Why:", report)
        self.assertIn("```c", report)
        self.assertIn("42: copy_from_user", report)
        self.assertNotIn("Evidence 1", report)
        self.assertNotIn("Untitled finding", report)

    def test_key_code_preserves_all_locations_in_one_code_block(self):
        finding = {
            "locations": [
                {
                    "file": "driver.c",
                    "line": 10,
                    "function_or_symbol": "allocate",
                    "code_snippet": "resource = allocate();",
                    "source_context": "7: unrelated();\n8: unrelated();",
                    "why_this_location_matters": "This acquires the resource.",
                },
                {
                    "file": "driver.c",
                    "line": 30,
                    "function_or_symbol": "fail",
                    "code_snippet": "if (error)\n    return error;",
                    "why_this_location_matters": "This returns without releasing it.",
                },
            ]
        }

        snippet = key_code_snippet(finding)

        self.assertEqual(1, snippet.count("```c"))
        self.assertEqual(1, snippet.count("\n```"))
        self.assertIn("/* driver.c:10 (allocate) */", snippet)
        self.assertIn("/* This acquires the resource. */", snippet)
        self.assertIn("resource = allocate();", snippet)
        self.assertNotIn("unrelated();", snippet)
        self.assertIn("/* driver.c:30 (fail) */", snippet)
        self.assertIn("/* This returns without releasing it. */", snippet)
        self.assertNotIn("Location:", snippet)
        self.assertNotIn("Why:", snippet)
        self.assertLess(snippet.index("resource = allocate();"), snippet.index("if (error)"))
        self.assertNotIn("Evidence 1", snippet)

    def test_key_code_does_not_limit_or_filter_location_chain(self):
        finding = {
            "locations": [
                {
                    "file": "driver.c",
                    "line": index * 10,
                    "code_snippet": "evidence_%d();" % index,
                }
                for index in range(1, 7)
            ]
        }

        snippet = key_code_snippet(finding)

        for index in range(1, 7):
            self.assertIn("evidence_%d();" % index, snippet)
        offsets = [snippet.index("evidence_%d();" % index) for index in range(1, 7)]
        self.assertEqual(sorted(offsets), offsets)
        self.assertEqual(6, snippet.count("/* driver.c:"))
        self.assertNotIn("Showing ", snippet)

    def test_risk_overview_keeps_full_summary(self):
        problem = (
            "A very long concrete finding summary names the exact function, variable, "
            "triggering input, failed operation, resulting ownership violation, and "
            "observable consequence without being shortened by the overview renderer."
        )

        report = render(
            "Example",
            Path("/tmp/source"),
            {},
            [{"problem": problem, "severity": "High", "locations": []}],
        )

        risk_section = report.split("## Risk Overview", 1)[1].split(
            "## Detailed Findings", 1
        )[0]
        self.assertIn(problem, risk_section)
        self.assertNotIn("...", risk_section)


if __name__ == "__main__":
    unittest.main()
