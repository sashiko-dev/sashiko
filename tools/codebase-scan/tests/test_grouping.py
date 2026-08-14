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

import tempfile
import unittest
from pathlib import Path

from codebase_scan.grouping import plan_groups
from codebase_scan.inventory import copy_snapshot, enumerate_files
from codebase_scan.synthetic import build_patches, initialize_repository


class GroupingTest(unittest.TestCase):
    def test_group_limits_and_risk_order_are_deterministic(self):
        with tempfile.TemporaryDirectory(prefix="repository_scan_grouping_") as temporary:
            source = Path(temporary)
            (source / "uvm").mkdir()
            (source / "display").mkdir()
            (source / "uvm" / "fault.c").write_text(
                "void uvm_fault(void) { dma_map_page(); copy_from_user(); }\n",
                encoding="utf-8",
            )
            (source / "uvm" / "fault.h").write_text(
                "void uvm_fault(void);\n", encoding="utf-8"
            )
            (source / "display" / "panel.c").write_text(
                "void drm_panel_enable(void) {}\n", encoding="utf-8"
            )

            files = enumerate_files(source)
            plan = plan_groups(source, files, max_files=1, max_lines=100)

            self.assertEqual(3, plan["group_count"])
            self.assertEqual("uvm/fault.c", plan["groups"][0]["files"][0])
            self.assertEqual(
                sorted(files),
                sorted(
                    path for group in plan["groups"] for path in group["files"]
                ),
            )
            self.assertEqual([], plan["coverage"]["unassigned_files"])
            self.assertEqual([], plan["coverage"]["duplicate_primary_assignments"])

    def test_register_headers_do_not_dominate_implementation_priority(self):
        with tempfile.TemporaryDirectory(prefix="repository_scan_register_headers_") as temporary:
            source = Path(temporary)
            (source / "gpu" / "drm" / "amd" / "include" / "asic_reg" / "nbio").mkdir(parents=True)
            (source / "gpu" / "drm" / "amd" / "amdgpu").mkdir(parents=True)
            (source / "gpu" / "drm" / "amd" / "include" / "asic_reg" / "nbio" / "nbio_7_0_default.h").write_text(
                "\n".join("#define mmBIF_DOORBELL_FAULT_DMA_%05d 0x%x" % (index, index) for index in range(3000)),
                encoding="utf-8",
            )
            (source / "gpu" / "drm" / "amd" / "amdgpu" / "amdgpu_vm.c").write_text(
                "void amdgpu_vm_fault(void) { dma_map_page(); mutex_lock(0); }\n",
                encoding="utf-8",
            )

            files = enumerate_files(source)
            plan = plan_groups(source, files, max_files=1, max_lines=1000)

            self.assertEqual(
                "gpu/drm/amd/amdgpu/amdgpu_vm.c",
                plan["groups"][0]["files"][0],
            )
            generated_groups = [
                group
                for group in plan["groups"]
                if group["files"][0].endswith("nbio_7_0_default.h")
            ]
            self.assertTrue(generated_groups)
            self.assertEqual({"generated_header": 1}, generated_groups[0]["source_kinds"])

    def test_oversized_file_is_split_into_bounded_complete_regions(self):
        with tempfile.TemporaryDirectory(prefix="repository_scan_regions_") as temporary:
            root = Path(temporary)
            source = root / "source"
            repo = root / "repo"
            output = root / "output"
            source.mkdir()
            output.mkdir()
            content = "".join(
                "int value_%05d = %d; /* %s */\n" % (index, index, "x" * 40)
                for index in range(1, 13001)
            )
            (source / "large.c").write_text(content, encoding="utf-8")

            plan = plan_groups(
                source,
                ["large.c"],
                max_files=30,
                max_lines=2000,
                max_bytes=100000,
            )

            self.assertGreater(plan["group_count"], 1)
            regions = sorted(
                plan["region_assignments"]["large.c"],
                key=lambda item: item["start_line"],
            )
            self.assertEqual(1, regions[0]["start_line"])
            self.assertEqual(13000, regions[-1]["end_line"])
            for previous, current in zip(regions, regions[1:]):
                self.assertEqual(previous["end_line"] + 1, current["start_line"])
            for group in plan["groups"]:
                self.assertLessEqual(group["lines"], 2000)
                self.assertLessEqual(group["bytes"], 100000)
            self.assertEqual([], plan["coverage"]["incomplete_region_assignments"])
            self.assertEqual([], plan["coverage"]["duplicate_primary_assignments"])

            copy_snapshot(source, repo, ["large.c"])
            full_commit = initialize_repository(repo)
            patches = build_patches(repo, output, full_commit, plan["groups"])

            self.assertEqual(plan["group_count"], len(patches))
            self.assertEqual(
                [],
                list((output / ".indexes").glob("*.index")),
            )
            for patch in patches:
                self.assertIn("diff --git a/large.c b/large.c", patch["diff"])
                self.assertLess(len(patch["diff"].encode("utf-8")), 130000)

    def test_snapshot_contains_only_inventory_files(self):
        with tempfile.TemporaryDirectory(prefix="repository_scan_snapshot_") as temporary:
            root = Path(temporary)
            source = root / "source"
            destination = root / "snapshot"
            source.mkdir()
            (source / "driver.c").write_text("int probe(void) { return 0; }\n")
            (source / ".env").write_text("TOKEN=secret\n")
            (source / "notes.txt").write_text("private notes\n")

            copy_snapshot(source, destination, ["driver.c"])

            self.assertTrue((destination / "driver.c").is_file())
            self.assertFalse((destination / ".env").exists())
            self.assertFalse((destination / "notes.txt").exists())


if __name__ == "__main__":
    unittest.main()
