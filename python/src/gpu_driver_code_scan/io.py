"""Small deterministic filesystem and process helpers."""

import hashlib
import json
import os
import subprocess
from pathlib import Path


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(
        "%s.%s.tmp" % (path.name, os.getpid())
    )
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(str(temporary), str(path))


def read_json(path, default=None):
    path = Path(path)
    if not path.is_file():
        return default
    return json.loads(path.read_text(encoding="utf-8"))


def append_jsonl(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(command, cwd=None, env=None, input_text=None, check=True):
    process = subprocess.Popen(
        [str(item) for item in command],
        cwd=str(cwd) if cwd else None,
        env=env,
        stdin=subprocess.PIPE if input_text is not None else None,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stdin_data = input_text.encode("utf-8") if isinstance(input_text, str) else input_text
    stdout, stderr = process.communicate(stdin_data)
    stdout_text = stdout.decode("utf-8", "replace")
    stderr_text = stderr.decode("utf-8", "replace")
    if check and process.returncode != 0:
        raise RuntimeError(
            "command failed rc=%s: %s\nstderr=%s\nstdout=%s"
            % (
                process.returncode,
                " ".join(str(item) for item in command),
                stderr_text[-4000:],
                stdout_text[-4000:],
            )
        )
    return process.returncode, stdout_text, stderr_text


def git(repo, *arguments):
    return run(["git"] + list(arguments), cwd=repo)[1].strip()
