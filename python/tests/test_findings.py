import shutil
import tempfile
import unittest
from pathlib import Path

from gpu_driver_code_scan.findings import enrich_locations, normalize, one_line


class FindingNormalizationTest(unittest.TestCase):
    def setUp(self):
        self.temporary = Path(tempfile.mkdtemp(prefix="gpu_scan_findings_"))

    def tearDown(self):
        shutil.rmtree(self.temporary)

    def test_normalize_keeps_all_reportable_findings(self):
        reportable, excluded = normalize(
            [
                {
                    "title": "Finding %d" % index,
                    "severity": "High",
                }
                for index in range(1, 15)
            ]
        )

        self.assertEqual([], excluded)
        self.assertEqual(14, len(reportable))

    def test_normalize_preserves_native_suggested_fix(self):
        suggested_fix = (
            "In block_copy_pages(), pass src_page and dst_page to kunmap() instead "
            "of src_addr and dst_addr, then add a highmem CPU-to-CPU migration test."
        )

        reportable, excluded = normalize(
            [
                {
                    "problem": "kunmap receives virtual addresses.",
                    "severity": "Critical",
                    "suggested_fix": suggested_fix,
                }
            ]
        )

        self.assertEqual([], excluded)
        self.assertEqual(suggested_fix, reportable[0]["suggested_fix"])

    def test_normalize_fallback_fix_names_concrete_location(self):
        reportable, excluded = normalize(
            [
                {
                    "problem": "The allocation is leaked when map_pages() fails.",
                    "severity": "High",
                    "locations": [
                        {
                            "file": "driver/memory.c",
                            "function_or_symbol": "populate_pages",
                            "line": 42,
                        }
                    ],
                }
            ]
        )

        self.assertEqual([], excluded)
        suggested_fix = reportable[0]["suggested_fix"]
        self.assertIn("populate_pages", suggested_fix)
        self.assertIn("driver/memory.c", suggested_fix)
        self.assertIn("cleanup path", suggested_fix)
        self.assertNotIn("regression test", suggested_fix)
        self.assertNotIn(
            "Add explicit input, state, ownership, and error-path validation",
            suggested_fix,
        )

    def test_one_line_never_truncates_content(self):
        text = "summary " + ("detail " * 80)

        rendered = one_line(text)

        self.assertEqual(text.strip(), rendered)
        self.assertFalse(rendered.endswith("..."))

    def test_synthetic_patch_history_is_removed_from_snapshot_findings(self):
        reportable, excluded = normalize(
            [
                {
                    "problem": (
                        "count can overflow buffer. This file is newly added in "
                        "the reviewed patchset, and the baseline does not contain it."
                    ),
                    "severity": "High",
                    "severity_explanation": (
                        "The user controls count. There are no later patches in "
                        "the provided series that fix it."
                    ),
                    "preexisting": False,
                    "patch_index": 1,
                    "patch_subject": "snapshot: add family:uvm",
                }
            ]
        )

        self.assertEqual([], excluded)
        self.assertEqual(1, len(reportable))
        finding = reportable[0]
        self.assertEqual("count can overflow buffer.", finding["problem"])
        self.assertEqual("The user controls count.", finding["severity_explanation"])
        self.assertNotIn("preexisting", finding)
        self.assertNotIn("patch_index", finding)
        self.assertNotIn("patch_subject", finding)

    def test_preexisting_prefix_is_removed_without_dropping_problem(self):
        reportable, excluded = normalize(
            [
                {
                    "problem": (
                        "This problem wasn't introduced by this patch, but "
                        "block_populate_overlapping_cpu_chunks() can double-free "
                        "CPU chunk pointers."
                    ),
                    "severity": "Critical",
                }
            ]
        )

        self.assertEqual([], excluded)
        self.assertEqual(
            "block_populate_overlapping_cpu_chunks() can double-free CPU chunk pointers.",
            reportable[0]["problem"],
        )

    def test_newly_added_wording_is_removed_without_dropping_problem(self):
        reportable, excluded = normalize(
            [
                {
                    "problem": (
                        "block_zero_new_gpu_chunk() leaves an unused local in the "
                        "newly added helper."
                    ),
                    "severity": "Low",
                }
            ]
        )

        self.assertEqual([], excluded)
        self.assertEqual(
            "block_zero_new_gpu_chunk() leaves an unused local in the helper.",
            reportable[0]["problem"],
        )

    def test_enrichment_corrects_hint_line_to_the_sashiko_snippet(self):
        source = self.temporary / "driver.c"
        source.write_text(
            "int unrelated;\n"
            "int before;\n"
            "status = dangerous_call();\n"
            "if (status)\n"
            "    goto error;\n"
            "int after;\n",
            encoding="utf-8",
        )
        findings = [
            {
                "locations": [
                    {
                        "file": "driver.c",
                        "line": 1,
                        "code_snippet": (
                            "status = dangerous_call();\n"
                            "if (status)\n"
                            "    goto error;"
                        ),
                    }
                ]
            }
        ]

        enriched = enrich_locations(findings, self.temporary)
        location = enriched[0]["locations"][0]

        self.assertEqual(3, location["line"])
        self.assertIn("3: status = dangerous_call();", location["source_context"])


if __name__ == "__main__":
    unittest.main()
