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

"""Source inventory and immutable snapshot creation."""

import fnmatch
import os
import shutil
from pathlib import Path


SOURCE_EXTENSIONS = {".c", ".h", ".rs", ".S", ".s", ".lds"}
EXCLUDED_DIRECTORIES = {
    ".git",
    ".hg",
    ".svn",
    "__pycache__",
    "artifacts",
    "build",
    "dist",
    "out",
    "output",
    "target",
}
EXCLUDED_SUFFIXES = (".o", ".ko", ".a", ".cmd", ".mod.c")


def is_test_path(relative):
    parts = relative.lower().split("/")
    if any(part in {"test", "tests", "selftest", "selftests"} for part in parts[:-1]):
        return True
    stem = Path(parts[-1]).stem
    return (
        stem.startswith("test_")
        or stem.startswith("selftest_")
        or stem.endswith("_test")
        or stem.endswith("_selftest")
    )


def is_reviewable(relative):
    path = Path(relative)
    return path.suffix in SOURCE_EXTENSIONS and not is_test_path(relative)


def enumerate_files(source_dir, include_globs=None):
    source_dir = Path(source_dir)
    include_globs = include_globs or []
    files = []
    for current, directories, names in os.walk(str(source_dir)):
        directories[:] = sorted(
            name
            for name in directories
            if name not in EXCLUDED_DIRECTORIES
            and not (Path(current) / name).is_symlink()
        )
        for name in sorted(names):
            path = Path(current) / name
            if path.is_symlink() or not path.is_file():
                continue
            if any(name.endswith(suffix) for suffix in EXCLUDED_SUFFIXES):
                continue
            relative = path.relative_to(source_dir).as_posix()
            if not is_reviewable(relative):
                continue
            if include_globs and not any(
                fnmatch.fnmatch(relative, pattern) for pattern in include_globs
            ):
                continue
            files.append(relative)
    return sorted(files)


def copy_snapshot(source_dir, destination, files):
    source_dir = Path(source_dir)
    destination = Path(destination)
    if destination.exists():
        shutil.rmtree(str(destination))
    destination.mkdir(parents=True)
    for relative in files:
        source = source_dir / relative
        if source.is_symlink() or not source.is_file():
            raise RuntimeError("inventory file is not a regular file: %s" % relative)
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(str(source), str(target))
