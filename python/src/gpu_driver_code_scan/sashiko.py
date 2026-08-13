"""Sashiko dependency discovery and bounded review execution."""

import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path


ACTIVE_PROCESSES = set()
ACTIVE_PROCESSES_LOCK = threading.Lock()
SHUTDOWN_REQUESTED = threading.Event()


REVIEW_PROMPT = (
    "This is a synthetic grouped source-snapshot review. The target source regions "
    "are represented as restored lines to adapt an existing source tree to Sashiko's "
    "patch interface. Do not report file addition, commit-message, mailing-list, "
    "Fixes-tag, or submission-format issues. Review concrete implementation logic, "
    "security, resource lifetime, arithmetic, locking, memory management, DMA/IOMMU, "
    "and hardware behavior. Use the full target snapshot as context. Do not call a "
    "finding a regression unless the source itself proves it."
)


def reset_shutdown_state():
    SHUTDOWN_REQUESTED.clear()


def register_process(process):
    with ACTIVE_PROCESSES_LOCK:
        ACTIVE_PROCESSES.add(process)
    if SHUTDOWN_REQUESTED.is_set():
        terminate(process)
        return False
    return True


def unregister_process(process):
    with ACTIVE_PROCESSES_LOCK:
        ACTIVE_PROCESSES.discard(process)


def terminate_active_reviews():
    SHUTDOWN_REQUESTED.set()
    with ACTIVE_PROCESSES_LOCK:
        processes = list(ACTIVE_PROCESSES)
    for process in processes:
        terminate(process)


def repository_root():
    return Path(__file__).resolve().parents[2]


def default_sashiko_dir():
    root = repository_root()
    candidates = []
    if root.name == "python":
        candidates.append(root.parent)
    candidates.append(root / "sashiko")
    for candidate in candidates:
        if (
            (candidate / "Cargo.toml").is_file()
            or (candidate / "Settings.toml").is_file()
            or (candidate / "third_party" / "prompts" / "kernel").is_dir()
        ):
            return candidate
    return candidates[0]


def review_binary_candidates(sashiko_dir):
    root = repository_root()
    return [
        sashiko_dir / "target" / "release" / "review",
        sashiko_dir / "bin" / "review",
        root / "bin" / "review",
    ]


def resolve_dependency(sashiko_dir=None, review_bin=None, prompts_dir=None):
    sashiko_dir = Path(sashiko_dir or default_sashiko_dir()).expanduser().resolve()
    if review_bin:
        review_bin = Path(review_bin).expanduser().resolve()
    else:
        review_bin = next(
            (candidate for candidate in review_binary_candidates(sashiko_dir)
             if candidate.is_file()),
            review_binary_candidates(sashiko_dir)[-1],
        ).expanduser().resolve()
    prompts_dir = Path(
        prompts_dir or sashiko_dir / "third_party" / "prompts" / "kernel"
    ).expanduser().resolve()
    if not review_bin.is_file():
        raise RuntimeError(
            "Sashiko review binary not found: %s. Build it with "
            "`cargo build --release --bin review` in %s." % (review_bin, sashiko_dir)
        )
    if not os.access(str(review_bin), os.X_OK):
        raise RuntimeError("Sashiko review binary is not executable: %s" % review_bin)
    if not prompts_dir.is_dir():
        raise RuntimeError("Sashiko prompt directory not found: %s" % prompts_dir)
    return sashiko_dir, review_bin, prompts_dir


def parse_json_output(stdout):
    for line in reversed([item.strip() for item in stdout.splitlines() if item.strip()]):
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    raise RuntimeError("Sashiko output did not contain a JSON object")


def terminate(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.time() + 5
    while time.time() < deadline:
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if process.poll() is None:
        process.wait()


def run_review(
    repo_dir,
    patch,
    payload,
    sashiko_dir,
    review_bin,
    prompts_dir,
    output_dir,
    provider="",
    model="",
    stages="3,4,5,6,7",
    no_ai=False,
    timeout_seconds=3600,
):
    log_dir = Path(output_dir) / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    stdout_path = log_dir / ("group-%06d.stdout.log" % patch["index"])
    stderr_path = log_dir / ("group-%06d.stderr.log" % patch["index"])
    command = [
        str(review_bin),
        "--repo",
        str(repo_dir),
        "--baseline",
        patch["baseline_commit"],
        "--review-commit",
        patch["target_commit"],
        "--review-patch-index",
        "1",
        "--prompts",
        str(prompts_dir),
        "--custom-prompt",
        REVIEW_PROMPT,
        "--skip-report-stage",
    ]
    if provider:
        command.extend(["--ai-provider", provider])
    if stages:
        command.extend(["--stages", stages])
    if no_ai:
        command.append("--no-ai")

    environment = os.environ.copy()
    environment.setdefault("NO_COLOR", "1")
    if provider:
        environment["SASHIKO__AI__PROVIDER"] = provider
    if model:
        environment["SASHIKO__AI__MODEL"] = model
    started = time.time()
    if SHUTDOWN_REQUESTED.is_set():
        return {
            "returncode": 143,
            "wall_seconds": 0,
            "result": None,
            "error": "Sashiko review cancelled",
            "stdout_path": str(stdout_path),
            "stderr_path": str(stderr_path),
            "stdout_tail": "",
            "stderr_tail": "",
        }
    with stdout_path.open("wb") as stdout_handle, stderr_path.open("wb") as stderr_handle:
        process = subprocess.Popen(
            command,
            cwd=str(sashiko_dir),
            env=environment,
            stdin=subprocess.PIPE,
            stdout=stdout_handle,
            stderr=stderr_handle,
            start_new_session=True,
        )
        try:
            registered = register_process(process)
            if not registered:
                timed_out = False
            else:
                process.communicate(
                    (json.dumps(payload) + "\n").encode("utf-8"),
                    timeout=timeout_seconds if timeout_seconds > 0 else None,
                )
                timed_out = False
        except subprocess.TimeoutExpired:
            timed_out = True
            terminate(process)
        finally:
            unregister_process(process)
        if not registered:
            process.wait()
        elif process.returncode != 0:
            terminate(process)

    stdout = stdout_path.read_text(encoding="utf-8", errors="replace")
    stderr = stderr_path.read_text(encoding="utf-8", errors="replace")
    error = ""
    result = None
    if not registered:
        error = "Sashiko review cancelled"
    elif timed_out:
        error = "Sashiko review timed out after %s seconds" % timeout_seconds
    elif process.returncode != 0:
        error = stderr[-4000:] or stdout[-4000:] or "Sashiko review failed"
    else:
        try:
            result = parse_json_output(stdout)
            if result.get("error"):
                error = str(result["error"])
        except RuntimeError as exception:
            error = str(exception)
    return {
        "returncode": process.returncode,
        "wall_seconds": time.time() - started,
        "result": result,
        "error": error,
        "stdout_path": str(stdout_path),
        "stderr_path": str(stderr_path),
        "stdout_tail": stdout[-4000:],
        "stderr_tail": stderr[-4000:],
    }
