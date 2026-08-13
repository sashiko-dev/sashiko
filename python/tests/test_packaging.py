import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class PackagingTest(unittest.TestCase):
    def test_report_template_is_tracked_source(self):
        template = (
            ROOT
            / "src"
            / "gpu_driver_code_scan"
            / "templates"
            / "report.md"
        )
        self.assertTrue(template.is_file())
        text = template.read_text(encoding="utf-8")
        self.assertIn("## Executive Summary", text)
        self.assertIn("## Risk Overview", text)
        self.assertIn("## Detailed Findings", text)


if __name__ == "__main__":
    unittest.main()
