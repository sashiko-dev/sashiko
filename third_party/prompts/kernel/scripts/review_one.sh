#!/bin/bash
#
# usage: review_one.sh <sha>
#
# Sets up a git worktree for the given SHA and runs claude review on it.
#
# Before running this, you need to have indexed the SHA range with semcode:
#   cd linux ; semcode-index -s . --git base..last_sha

set -e

usage() {
    echo "usage: review_one.sh [--linux <linux_dir>] [--prompt <prompt_file>] [--series <end_sha>] [--working-dir <dir>] [--model <model>] [--append <string>] <sha>"
    echo "  --linux: path to the base linux directory (default: \$PWD/linux)"
    echo "  --prompt: path to the review prompt file (default: <script_dir>/../review-core.md)"
    echo "  sha: the git commit SHA to review"
    echo "  --series: optional SHA of the last commit in the series"
    echo "  --range: optional git range base..last_sha"
    echo "  --working-dir: working directory (default: current directory or WORKING_DIR env)"
    echo "  --model: Claude model to use (default: sonnet or CLAUDE_MODEL env)"
    echo "  --append: string to append to the prompt (e.g., for enabling pedantic mode)"
    echo "  --cli: which CLI to use (default: claude)"
    echo "  --help: show this help message"
}

if [ $# -lt 1 ]; then
    usage
    exit 1
fi

# Get script directory early so we can use it for defaults
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"

# Parse arguments
SERIES_SHA=""
RANGE_SHA=""
ARG_WORKING_DIR=""
ARG_MODEL=""
REVIEW_PROMPT=""
BASE_LINUX=""
APPEND_STRING=""
CLI="claude"
while [[ $# -gt 1 ]]; do
    case "$1" in
        --help)
            usage
            exit 0
            ;;
        --series)
            SERIES_SHA="$2"
            shift 2
            ;;
        --range)
            RANGE_SHA="$2"
            shift 2
            ;;
        --working-dir)
            ARG_WORKING_DIR="$2"
            shift 2
            ;;
        --model)
            ARG_MODEL="$2"
            shift 2
            ;;
        --cli)
            CLI="$2"
            shift 2
            ;;
        --prompt)
            REVIEW_PROMPT="$2"
            shift 2
            ;;
        --linux)
            BASE_LINUX="$2"
            shift 2
            ;;
        --append)
            APPEND_STRING="$2"
            shift 2
            ;;
        *)
            break
            ;;
    esac
done

SHA="$1"

# Set defaults for optional arguments
if [ -z "$REVIEW_PROMPT" ]; then
    REVIEW_PROMPT="$SCRIPT_DIR/../review-core.md"
fi

if [ -z "$BASE_LINUX" ]; then
    BASE_LINUX="$(pwd -P)/linux"
fi

# Validate paths exist
if [ ! -f "$REVIEW_PROMPT" ]; then
    echo "Error: prompt file does not exist: $REVIEW_PROMPT" >&2
    exit 1
fi

if [ ! -d "$BASE_LINUX" ]; then
    echo "Error: linux directory does not exist: $BASE_LINUX" >&2
    exit 1
fi

# Use command line args first, then environment variables, then defaults
if [ -n "$ARG_WORKING_DIR" ]; then
    WORKING_DIR="$ARG_WORKING_DIR"
elif [ -z "$WORKING_DIR" ]; then
    WORKING_DIR="$(pwd -P)"
fi

if [ -n "$ARG_MODEL" ]; then
    CLAUDE_MODEL="$ARG_MODEL"
fi

export WORKING_DIR
export CLAUDE_MODEL

DIR="$BASE_LINUX.$SHA"

export TERM=xterm
export FORCE_COLOR=0

echo "Linux directory: $BASE_LINUX" >&2
echo "Working directory: $WORKING_DIR" >&2
echo "Prompt file: $REVIEW_PROMPT" >&2
echo "Processing $SHA"

if [ ! -d "$DIR" ]; then
    (cd "$BASE_LINUX" && git worktree add -d "$DIR" "$SHA")
    while true; do
        if [ -d "$DIR" ]; then
            break
        fi
        echo "waiting for $DIR to exist"
        sleep 1
    done
    if [ -d "$BASE_LINUX/.semcode.db" ]; then
        cp -al "$BASE_LINUX/.semcode.db" "$DIR/.semcode.db"
	HAVE_MCP=1
    else
        echo "Warning: $BASE_LINUX/.semcode.db not found, skipping MCP configuration" >&2
    fi
fi

cd "$DIR"

nowstr=$(date +"%Y-%m-%d-%H:%M")

# Clean up old review files
rm -f review.json
rm -f check.md
rm -f check.json
rm -f review.duration.txt

if [ -f review-inline.txt ]; then
    mv review-inline.txt "review-inline.$nowstr.txt"
fi

echo "Worktree ready at $DIR"
echo "SHA: $SHA"

# Build the prompt, optionally including series info
PROMPT="read prompt $REVIEW_PROMPT and run regression analysis of commit $SHA"
if [ -n "$SERIES_SHA" ]; then
    PROMPT+=", which is part of a series ending with $SERIES_SHA"
elif [ -n "$RANGE_SHA" ]; then
    PROMPT+=", which is part of a series with git range $RANGE_SHA"
fi

# Append optional string to prompt
if [ -n "$APPEND_STRING" ]; then
    PROMPT="$PROMPT $APPEND_STRING"
fi

MCP_ARGS=""

set_claude_opts() {
	if [ -v HAVE_MCP ]; then
		MCP_JSON='{"mcpServers":{"semcode":{"command":"semcode-mcp"}}}'

		SC_PFX="mcp__semcode"

		MCP_ARGS="--mcp-config"
		MCP_ARGS+=" '$MCP_JSON'"
		MCP_ARGS+=" --allowedTools"
		MCP_ARGS+=" ${SC_PFX}__find_function"
		MCP_ARGS+=",${SC_PFX}__find_type"
		MCP_ARGS+=",${SC_PFX}__find_callers"
		MCP_ARGS+=",${SC_PFX}__find_calls"
		MCP_ARGS+=",${SC_PFX}__find_callchain"
		MCP_ARGS+=",${SC_PFX}__diff_functions"
		MCP_ARGS+=",${SC_PFX}__grep_functions"
		MCP_ARGS+=",${SC_PFX}__vgrep_functions"
		MCP_ARGS+=",${SC_PFX}__find_commit"
		MCP_ARGS+=",${SC_PFX}__vcommit_similar_commits"
		MCP_ARGS+=",${SC_PFX}__lore_search"
		MCP_ARGS+=",${SC_PFX}__dig"
		MCP_ARGS+=",${SC_PFX}__vlore_similar_emails"
		MCP_ARGS+=",${SC_PFX}__indexing_status"
		MCP_ARGS+=",${SC_PFX}__list_branches"
		MCP_ARGS+=",${SC_PFX}__compare_branches"
	fi

	if [ -z "$CLAUDE_MODEL" ]; then
		CLAUDE_MODEL="opus"
	fi

	JSONPROG="$SCRIPT_DIR/claude-json.py"
	OUTFILE="review.json"

	CLI_OPTS="--verbose"
	CLI_OUT="--output-format=stream-json | tee $OUTFILE | $JSONPROG"
}

set_copilot_opts() {
	if [ -v HAVE_MCP ]; then
		MCP_JSON='{"mcpServers":{"semcode":{"command":"semcode-mcp","args":[],"tools":["*"]}}}'

		MCP_ARGS="--additional-mcp-config"
		MCP_ARGS+=" '$MCP_JSON'"
		MCP_ARGS+=" --allow-tool 'semcode'"
	fi

	if [ -z "$CLAUDE_MODEL" ]; then
		CLAUDE_MODEL="claude-opus-4.5"
	fi

	CLI_OPTS="--log-level all"
	CLI_OPTS+=" --add-dir /tmp"
	CLI_OPTS+=" --add-dir $WORKING_DIR"

	# Need this for output redirection. Can we reduce this?
	CLI_OPTS+=" --allow-all-tools"

	OUTFILE="review.out"
	CLI_OUT=" | tee $OUTFILE"
}

case "$CLI" in
    claude)
	    set_claude_opts
	    ;;
    copilot)
	    set_copilot_opts
	    ;;
    *)
	    echo "Error: Unknown CLI: $CLI" >&2
	    exit 1
	    ;;
esac

# Build the full command
FULL_CMD="$CLI"
FULL_CMD+=" -p '$PROMPT'"
FULL_CMD+=" $MCP_ARGS"
FULL_CMD+=" --model $CLAUDE_MODEL"
FULL_CMD+=" $CLI_OPTS"
FULL_CMD+=" $CLI_OUT"
#echo "Would run: $FULL_CMD"

start=$(date +%s)

for x in $(seq 1 5); do
    eval "$FULL_CMD"
    if [ -s "$OUTFILE" ]; then
        break
    fi
    echo "$CLI failed $SHA try $x"
    sleep 5
done

end=$(date +%s)
echo "Elapsed time: $((end - start)) seconds (sha $SHA)" | tee review.duration.txt

if [ -v JSONPROG ]; then
	$JSONPROG -i review.json -o review.md
fi

# Exit with failure if output file is empty after all retries
if [ -s "$OUTFILE" ]; then
    exit 0
else
    exit 1
fi
