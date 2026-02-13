#!/usr/bin/env python3

import subprocess
import sys

def run_git(cmd):
    """Run a git command and return output lines."""
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"Error running: {cmd}", file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        sys.exit(1)
    return result.stdout.strip().split('\n') if result.stdout.strip() else []

def get_current_head():
    """Get current HEAD commit."""
    result = subprocess.run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True)
    return result.stdout.strip()

def build_parent_child_map():
    """Build a map of commit -> leftmost parent and parent -> children."""
    print("Building commit graph...", file=sys.stderr)

    # Get all commits with their parents (including reflog)
    commits = run_git('git rev-list --all --reflog --parents')

    parent_map = {}  # commit -> leftmost parent
    children_map = {}  # parent -> list of children

    for line in commits:
        parts = line.split()
        if not parts:
            continue

        commit = parts[0]
        parents = parts[1:]  # All parents listed

        # Store leftmost parent (first parent)
        if parents:
            leftmost_parent = parents[0]
            parent_map[commit] = leftmost_parent

            # Build reverse map: parent -> children
            if leftmost_parent not in children_map:
                children_map[leftmost_parent] = []
            children_map[leftmost_parent].append(commit)

    print(f"Found {len(parent_map)} commits with parents", file=sys.stderr)
    return parent_map, children_map

def find_all_descendants(commit, children_map):
    """Find all descendants of a commit using BFS."""
    descendants = set()
    queue = [commit]

    while queue:
        current = queue.pop(0)

        # Get children of current commit
        if current in children_map:
            for child in children_map[current]:
                if child not in descendants:
                    descendants.add(child)
                    queue.append(child)

    return descendants

def main():
    # Get current HEAD
    head = get_current_head()
    head_short = subprocess.run(['git', 'rev-parse', '--short', 'HEAD'],
                                capture_output=True, text=True).stdout.strip()

    print(f"Finding descendants of HEAD: {head_short}", file=sys.stderr)
    print(f"Full SHA: {head}", file=sys.stderr)
    print(file=sys.stderr)

    # Build the parent-child relationship maps
    parent_map, children_map = build_parent_child_map()

    # Find all descendants
    descendants = find_all_descendants(head, children_map)

    print(f"Found {len(descendants)} descendant commits", file=sys.stderr)
    print(file=sys.stderr)

    if descendants:
        # Print descendants with their info
        for commit in sorted(descendants):
            commit_info = subprocess.run(
                ['git', 'log', '-1', '--format=%h %s', commit],
                capture_output=True, text=True
            ).stdout.strip()
            print(commit_info)
    else:
        print("No descendants found for HEAD", file=sys.stderr)

if __name__ == '__main__':
    main()
