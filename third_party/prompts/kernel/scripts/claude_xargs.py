#!/usr/bin/env python3
"""
Run multiple claude -p commands in parallel, optionally with SHAs from a file.
"""

import argparse
import os
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from threading import Event, Lock

# Global state for signal handling
shutdown_event = Event()
active_processes = []
processes_lock = Lock()
signal_received = False


def signal_handler(signum, frame):
    """Handle Ctrl-C by killing all processes immediately."""
    global signal_received
    if signal_received:
        return  # Avoid multiple invocations
    signal_received = True

    print("\n\nInterrupted! Shutting down processes...", file=sys.stderr)
    shutdown_event.set()
    kill_all_processes()


def kill_all_processes():
    """Kill all active processes, first with SIGTERM, then SIGKILL."""
    with processes_lock:
        procs = list(active_processes)

    if not procs:
        return

    # First, try SIGTERM on process groups
    print(f"Sending SIGTERM to {len(procs)} process group(s)...", file=sys.stderr)
    for proc in procs:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, OSError):
            pass

    # Wait up to 5 seconds for graceful termination
    deadline = time.time() + 5
    while time.time() < deadline:
        with processes_lock:
            still_running = [p for p in active_processes if p.poll() is None]
        if not still_running:
            break
        time.sleep(0.1)

    # Kill any remaining with SIGKILL
    with processes_lock:
        still_running = [p for p in active_processes if p.poll() is None]

    if still_running:
        print(f"Sending SIGKILL to {len(still_running)} process group(s)...", file=sys.stderr)
        for proc in still_running:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, OSError):
                pass

    # Wait for all to actually exit
    print("Waiting for all processes to exit...", file=sys.stderr)
    for proc in procs:
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            print(f"Process {proc.pid} did not exit, forcing...", file=sys.stderr)
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait(timeout=5)
            except (ProcessLookupError, OSError, subprocess.TimeoutExpired):
                pass

    print("All processes terminated.", file=sys.stderr)


def run_claude_command(cmd_template: str, sha: str, timeout: int | None) -> tuple[str, int, str, str]:
    """
    Run a single claude command with a SHA appended to the prompt.

    Returns: (sha, return_code, stdout, stderr)
    """
    if shutdown_event.is_set():
        return (sha, -1, "", "Shutdown requested before start")

    # Build the command with SHA appended
    cmd = f"{cmd_template} {sha}"

    try:
        proc = subprocess.Popen(
            cmd,
            shell=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            preexec_fn=os.setsid  # Create new process group for clean termination
        )

        with processes_lock:
            active_processes.append(proc)

        try:
            stdout, stderr = proc.communicate(timeout=timeout)
            return (sha, proc.returncode, stdout, stderr)
        except subprocess.TimeoutExpired:
            # Kill the process group
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                time.sleep(1)
                if proc.poll() is None:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait()
            except (ProcessLookupError, OSError):
                pass
            return (sha, -1, "", f"Timeout after {timeout} seconds")

    except Exception as e:
        return (sha, -1, "", str(e))
    finally:
        with processes_lock:
            if proc in active_processes:
                active_processes.remove(proc)


def main():
    parser = argparse.ArgumentParser(
        description="Run claude -p commands in parallel",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Run with SHAs from a file
  %(prog)s -n 4 -c 'claude -p "Review commit"' -f shas.txt

  # With timeout
  %(prog)s -n 2 -c 'claude -p "Analyze"' -f shas.txt --timeout 300

  # With pedantic mode enabled
  %(prog)s -n 4 -c './review_one.sh' -f shas.txt --append "Enable pedantic mode."
        """
    )
    parser.add_argument(
        "-c", "--command",
        required=True,
        help="The claude command template to run (SHA will be appended if -f is used)"
    )
    parser.add_argument(
        "-n", "--parallel",
        type=int,
        default=24,
        help="Number of parallel instances to run (default: 24)"
    )
    parser.add_argument(
        "-f", "--sha-file",
        required=True,
        help="File containing list of SHAs (one per line)"
    )
    parser.add_argument(
        "--series",
        help="Git SHA of the last commit in the series"
    )
    parser.add_argument(
        "--append",
        help="String to append to the prompt (e.g., for enabling pedantic mode)"
    )
    parser.add_argument(
        "--timeout",
        type=int,
        help="Timeout in seconds for each command"
    )
    parser.add_argument(
        "-v", "--verbose",
        action="store_true",
        help="Print stderr output from commands"
    )

    args = parser.parse_args()

    # Build list of SHAs from file
    with open(args.sha_file, "r") as f:
        shas = [line.strip() for line in f if line.strip() and not line.startswith("#")]
    if not shas:
        print(f"Error: No SHAs found in {args.sha_file}", file=sys.stderr)
        sys.exit(1)
    print(f"Loaded {len(shas)} SHAs from {args.sha_file}", file=sys.stderr)

    # Build command template with optional series and append arguments
    cmd_template = args.command
    if args.series:
        cmd_template = f"{cmd_template} --series {args.series}"
    if args.append:
        cmd_template = f"{cmd_template} --append '{args.append}'"

    # Set up signal handler
    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    # Run commands in parallel
    results = []
    completed = 0
    failed = 0

    try:
        with ThreadPoolExecutor(max_workers=args.parallel) as executor:
            futures = {
                executor.submit(run_claude_command, cmd_template, sha, args.timeout): sha
                for sha in shas
            }

            for future in as_completed(futures):
                if shutdown_event.is_set():
                    break

                sha, returncode, stdout, stderr = future.result()
                completed += 1

                if returncode == 0:
                    print(f"\n{'='*60}")
                    print(f"Completed: {sha}")
                    print(f"{'='*60}")
                    print(stdout)
                else:
                    failed += 1
                    print(f"\n{'='*60}", file=sys.stderr)
                    print(f"FAILED: {sha} (exit code: {returncode})", file=sys.stderr)
                    print(f"{'='*60}", file=sys.stderr)
                    if args.verbose and stderr:
                        print(stderr, file=sys.stderr)

                print(f"Progress: {completed}/{len(shas)} (failed: {failed})", file=sys.stderr)

    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
    finally:
        # Ensure cleanup even if signal handler didn't run
        if shutdown_event.is_set() and not signal_received:
            kill_all_processes()

    print(f"\nCompleted: {completed}/{len(shas)}, Failed: {failed}", file=sys.stderr)
    sys.exit(1 if failed > 0 or shutdown_event.is_set() else 0)


if __name__ == "__main__":
    main()
