#!/usr/bin/env python3
"""
Mechanical FILE-N-CHANGE-N categorization for kernel patch review.

Creates ./review-context/ directory with:
- change.diff - Full commit message and diff
- commit-message.json - Parsed commit metadata
- index.json - Index of all changes grouped by file
- FILE-N-CHANGE-M.json - Changes grouped by source file
"""

import argparse
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


@dataclass
class Hunk:
    """Represents a single diff hunk."""
    header: str  # The @@ line
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    function_context: str  # Function name from @@ header
    lines: list[str] = field(default_factory=list)
    # Diffinfo from semcode
    diffinfo: Optional[dict] = None


@dataclass
class FileChange:
    """Represents changes to a single file."""
    old_path: str
    new_path: str
    hunks: list[Hunk] = field(default_factory=list)


@dataclass
class Change:
    """Represents a single FILE-N-CHANGE-M entry."""
    file_num: int
    change_num: int
    file: str
    function: str
    hunk_header: str
    hunk_content: str
    total_lines: int
    # Aggregated diffinfo from semcode (combined from all hunks in this change)
    diffinfo: Optional[dict] = None
    # Function/type definition extracted from source (when semcode unavailable)
    definition: Optional[str] = None

    @property
    def id(self) -> str:
        return f"FILE-{self.file_num}-CHANGE-{self.change_num}"


# Maximum added lines when combining hunks from the same function
MAX_COMBINED_ADDED_LINES = 500

# Maximum total lines when combining new functions in the same file
MAX_NEW_FUNCTION_COMBINED_LINES = 500

# Maximum total lines per FILE-N before creating a new FILE-N+1
MAX_LINES_PER_FILE = 500

# Maximum combined lines for merging small FILE-N groups
# If consecutive FILE-N groups together have fewer lines than this, combine them
MAX_COMBINED_FILE_GROUP_LINES = 500

# Maximum combined lines when merging FILE-N groups based on function similarity
MAX_SIMILARITY_COMBINED_LINES = 500

# Minimum function overlap ratio to consider merging two FILE-N groups
MIN_FUNCTION_OVERLAP_RATIO = 0.8


def count_added_lines(hunk_content: str) -> int:
    """Count the number of added lines (starting with +, excluding ++) in hunk content."""
    count = 0
    for line in hunk_content.split("\n"):
        if line.startswith("+") and not line.startswith("++"):
            count += 1
    return count


def count_total_lines(hunk_content: str) -> int:
    """Count total lines in hunk content (excluding empty lines at end)."""
    lines = hunk_content.rstrip().split("\n")
    return len(lines)


def run_git(args: list[str], cwd: Optional[str] = None) -> str:
    """Run a git command and return output."""
    result = subprocess.run(
        ["git"] + args,
        capture_output=True,
        text=True,
        cwd=cwd,
    )
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr}")
    return result.stdout


@dataclass
class DiffParseResult:
    """Result of parsing a unified diff for modified symbols."""
    modified_functions: set[str] = field(default_factory=set)
    called_functions: set[str] = field(default_factory=set)
    modified_types: set[str] = field(default_factory=set)
    modified_macros: set[str] = field(default_factory=set)
    function_calls: dict[str, set[str]] = field(default_factory=dict)  # Per-function calls


def parse_unified_diff_python(diff_content: str) -> DiffParseResult:
    """
    Pure Python implementation of parse_unified_diff.
    Parses the diff content to extract modified functions, types, macros, and per-function calls.
    """
    result = DiffParseResult()
    lines = diff_content.split("\n")
    i = 0

    while i < len(lines):
        line = lines[i]

        # Look for file headers for C/C++ files
        if line.startswith("+++") and any(ext in line for ext in [".c", ".h", ".cpp", ".cc", ".cxx"]):
            # Process hunks for this file
            i += 1
            while i < len(lines):
                hunk_line = lines[i]

                if hunk_line.startswith("@@"):
                    # Parse the hunk
                    hunk_result = parse_hunk_python(lines, i)
                    result.modified_functions.update(hunk_result.modified_functions)
                    result.called_functions.update(hunk_result.called_functions)
                    result.modified_types.update(hunk_result.modified_types)
                    result.modified_macros.update(hunk_result.modified_macros)
                    # Merge per-function calls
                    for func_name, calls in hunk_result.function_calls.items():
                        if func_name not in result.function_calls:
                            result.function_calls[func_name] = set()
                        result.function_calls[func_name].update(calls)
                    # Find end of hunk
                    i += 1
                    while i < len(lines) and not lines[i].startswith(("@@", "---", "+++")):
                        i += 1
                elif hunk_line.startswith("---") or hunk_line.startswith("+++"):
                    break
                else:
                    i += 1
        else:
            i += 1

    return result


def parse_hunk_python(lines: list[str], hunk_start: int) -> DiffParseResult:
    """Parse a single hunk and extract modified symbols using walk-back algorithm."""
    result = DiffParseResult()

    # Extract function from hunk header
    hunk_header = lines[hunk_start]
    header_func_name = extract_function_from_hunk_header(hunk_header)
    if header_func_name:
        result.modified_functions.add(header_func_name)

    # Collect hunk content
    hunk_lines = []  # Reconstructed code (context + added)
    modified_line_indices = set()  # Lines that were modified
    line_to_calls: dict[int, set[str]] = {}  # Track calls per line
    current_line = 0
    i = hunk_start + 1

    while i < len(lines):
        line = lines[i]

        if line.startswith("@@") or line.startswith("---") or line.startswith("+++"):
            break

        if line.startswith("+") and not line.startswith("+++"):
            # Added line
            content = line[1:]  # Remove + prefix
            hunk_lines.append(content)
            modified_line_indices.add(current_line)
            # Extract function calls and track by line
            line_calls = extract_function_calls_python(content)
            result.called_functions.update(line_calls)
            if line_calls:
                line_to_calls[current_line] = line_calls
            current_line += 1
        elif line.startswith("-") and not line.startswith("---"):
            # Removed line - track as modified but don't include in reconstructed code
            content = line[1:]
            modified_line_indices.add(current_line)
            result.called_functions.update(extract_function_calls_python(content))
            # Don't increment current_line for removed lines
        elif line.startswith(" ") or line == "":
            # Context line
            hunk_lines.append(line[1:] if line.startswith(" ") else "")
            current_line += 1

        i += 1

    # Use walk-back algorithm to find symbols for modified lines
    if hunk_lines:
        symbols = extract_symbols_by_walkback_python(hunk_lines, modified_line_indices)
        for symbol in symbols:
            if symbol.startswith("#"):
                # Macro
                result.modified_macros.add(symbol[1:])
            elif symbol.endswith("()"):
                # Function
                result.modified_functions.add(symbol[:-2])
            elif symbol.startswith(("struct ", "union ", "enum ", "typedef ")):
                # Type
                result.modified_types.add(symbol)

        # Attribute calls to functions using walk-back
        for line_idx, calls in line_to_calls.items():
            containing_func = find_symbol_for_line_python(hunk_lines, line_idx)
            if containing_func:
                func_name = extract_function_name_from_symbol_python(containing_func)
                if func_name:
                    # Filter out self-references
                    filtered_calls = {c for c in calls if c != func_name}
                    if filtered_calls:
                        if func_name not in result.function_calls:
                            result.function_calls[func_name] = set()
                        result.function_calls[func_name].update(filtered_calls)
            elif header_func_name:
                # Fall back to hunk header function
                filtered_calls = {c for c in calls if c != header_func_name}
                if filtered_calls:
                    if header_func_name not in result.function_calls:
                        result.function_calls[header_func_name] = set()
                    result.function_calls[header_func_name].update(filtered_calls)

    return result


def extract_function_calls_python(line: str) -> set[str]:
    """Extract function calls from a line of code."""
    calls = set()
    line = line.strip()

    # Skip empty lines, comments, preprocessor
    if not line or line.startswith("//") or line.startswith("/*") or line.startswith("#"):
        return calls

    # Simple regex to find function calls: identifier followed by (
    # This is a simplified version - the Rust code is more sophisticated
    import re
    pattern = r'\b([a-zA-Z_][a-zA-Z0-9_]*)\s*\('
    for match in re.finditer(pattern, line):
        name = match.group(1)
        if name not in C_KEYWORDS:
            calls.add(name)

    return calls


def extract_symbols_by_walkback_python(lines: list[str], modified_lines: set[int]) -> list[str]:
    """Walk back from modified lines to find enclosing symbol definitions."""
    symbols = set()

    for modified_line in modified_lines:
        decl_line = find_symbol_for_line_python(lines, modified_line)
        if decl_line:
            symbol_name = extract_symbol_name_from_declaration_python(decl_line)
            if symbol_name:
                symbols.add(symbol_name)

    return list(symbols)


def find_symbol_for_line_python(lines: list[str], line_idx: int) -> Optional[str]:
    """Walk back from a line to find the enclosing function, struct, or macro definition."""
    if line_idx >= len(lines):
        return None

    current_line = lines[line_idx].strip() if line_idx < len(lines) else ""

    # Check for single-line typedef
    if current_line.startswith("typedef ") and current_line.endswith(";"):
        return current_line

    # Check for single-line macro
    if current_line.startswith("#define "):
        return current_line

    # Walk backwards looking for a definition
    for i in range(line_idx, max(-1, line_idx - 51), -1):
        if i < 0 or i >= len(lines):
            continue

        line = lines[i]
        trimmed = line.strip()

        # Skip empty lines and comments
        if not trimmed or trimmed.startswith("//") or trimmed.startswith("/*"):
            continue

        # Skip false positives (labels, case statements)
        if is_false_positive_python(line):
            continue

        # Check for function definition
        if is_function_definition_python(line, lines, i):
            return trimmed

        # Check for struct/union/enum definition
        if is_type_definition_python(line):
            return trimmed

        # Check for macro definition
        if trimmed.startswith("#define "):
            return trimmed

        # If it starts with whitespace, keep walking back (inside function body)
        if line and line[0].isspace():
            continue

    return None


def is_false_positive_python(line: str) -> bool:
    """Check if a line is a false positive (goto label, case label, etc.)."""
    trimmed = line.strip()

    if not line or line[0].isspace():
        return False

    if trimmed.endswith(":"):
        if trimmed.startswith("case ") or trimmed.startswith("default:"):
            return True
        if trimmed in ("public:", "private:", "protected:"):
            return True
        # Simple goto label
        label = trimmed.rstrip(":")
        if label and all(c.isalnum() or c == "_" for c in label):
            return True

    return False


def is_function_definition_python(line: str, lines: list[str], line_idx: int) -> bool:
    """Check if a line looks like a function definition."""
    # Must not start with significant indentation (allow minor whitespace)
    stripped = line.lstrip()
    if line and len(line) - len(stripped) > 1:
        return False

    trimmed = line.strip()
    if "(" not in trimmed:
        return False

    # Skip preprocessor, control flow, etc.
    if trimmed.startswith("#") or trimmed.startswith("//") or trimmed.startswith("/*"):
        return False

    # Skip common non-function patterns
    skip_keywords = ["if", "while", "for", "switch", "return", "sizeof", "typeof", "case"]
    for kw in skip_keywords:
        if trimmed.startswith(kw + "(") or trimmed.startswith(kw + " ("):
            return False

    # Check for opening brace on this line or following lines
    # Handle multi-line function signatures like:
    #   static void func(int a,
    #                    int b)
    #   {
    has_brace = "{" in trimmed
    if not has_brace:
        for j in range(line_idx + 1, min(len(lines), line_idx + 20)):
            if j >= len(lines):
                break
            next_line = lines[j]
            next_stripped = next_line.strip()

            if "{" in next_line:
                has_brace = True
                break

            # Empty line might be between signature and brace
            if not next_stripped:
                continue

            # Check if this is a continuation of the function signature
            # Continuation lines typically:
            # - Are indented (parameter continuation)
            # - End with , or ( (more params coming)
            # - End with ) (end of params, brace should follow)
            is_continuation = (
                next_line[0:1].isspace() or  # Indented
                next_stripped.endswith(",") or
                next_stripped.endswith("(") or
                next_stripped.endswith(")")  # End of multi-line params
            )

            if not is_continuation:
                # Non-continuation line without brace means not a function def
                break

    return has_brace


def is_type_definition_python(line: str) -> bool:
    """Check if a line is a struct/union/enum definition."""
    trimmed = line.strip()

    type_keywords = ["struct ", "union ", "enum ", "typedef struct ", "typedef union ", "typedef enum "]
    if not any(trimmed.startswith(kw) for kw in type_keywords):
        return False

    return "{" in trimmed


def extract_symbol_name_from_declaration_python(decl_line: str) -> Optional[str]:
    """Extract a formatted symbol name from a declaration line."""
    trimmed = decl_line.strip()

    # Check for macro
    if trimmed.startswith("#define "):
        after_define = trimmed[8:].strip()
        # Get macro name (before whitespace or paren)
        match = re.match(r"(\w+)", after_define)
        if match:
            return f"#{match.group(1)}"
        return None

    # Check for struct/union/enum DEFINITION (not a function returning a struct pointer)
    # A struct/union/enum definition has '{' but no '(' before it
    # e.g., "struct foo {" is a definition, but "struct foo *func(" is a function
    type_prefixes = ["typedef struct ", "typedef union ", "typedef enum ", "struct ", "union ", "enum "]
    for prefix in type_prefixes:
        if trimmed.startswith(prefix):
            # Check if this is a function returning a struct/union/enum pointer
            # Functions have '(' before '{' or at end of line (multi-line signature)
            has_paren = "(" in trimmed
            has_brace = "{" in trimmed

            # If it has '(' but no '{', or '(' comes before '{', it's likely a function
            if has_paren:
                if not has_brace:
                    # No brace on this line, but has paren - it's a function
                    paren_pos = trimmed.find("(")
                    if paren_pos > 0:
                        before_paren = trimmed[:paren_pos]
                        tokens = before_paren.split()
                        if tokens:
                            name = tokens[-1].lstrip("*&")
                            if name and all(c.isalnum() or c == "_" for c in name):
                                return f"{name}()"
                    return None
                # Has both - check which comes first
                paren_pos = trimmed.find("(")
                brace_pos = trimmed.find("{")
                if paren_pos < brace_pos:
                    # Paren before brace - it's a function
                    before_paren = trimmed[:paren_pos]
                    tokens = before_paren.split()
                    if tokens:
                        name = tokens[-1].lstrip("*&")
                        if name and all(c.isalnum() or c == "_" for c in name):
                            return f"{name}()"
                    return None

            # No paren, or brace before paren - treat as type definition
            after_kw = trimmed[len(prefix):].strip()
            # Extract name before { or whitespace
            match = re.match(r"(\w+)", after_kw)
            if match:
                kw = prefix.replace("typedef ", "").strip()
                return f"{kw} {match.group(1)}"
            return None

    # Check for typedef
    if trimmed.startswith("typedef ") and trimmed.endswith(";"):
        # Get last word before ;
        without_semi = trimmed[:-1].strip()
        tokens = without_semi.split()
        if tokens:
            return f"typedef {tokens[-1]}"
        return None

    # Must be a function - extract the name
    paren_pos = trimmed.find("(")
    if paren_pos > 0:
        before_paren = trimmed[:paren_pos]
        tokens = before_paren.split()
        if tokens:
            name = tokens[-1].lstrip("*&")
            if name and all(c.isalnum() or c == "_" for c in name):
                return f"{name}()"

    return None


def extract_function_name_from_symbol_python(symbol: str) -> Optional[str]:
    """Extract function name from a symbol/declaration line."""
    trimmed = symbol.strip()

    # If it's already a formatted function symbol like "func_name()"
    if trimmed.endswith("()"):
        return trimmed[:-2]

    # Try to extract function name from a declaration line
    paren_pos = trimmed.find("(")
    if paren_pos > 0:
        before_paren = trimmed[:paren_pos]
        tokens = before_paren.split()
        if tokens:
            name = tokens[-1].lstrip("*&")
            if name and all(c.isalnum() or c == "_" for c in name):
                return name

    return None


def extract_function_from_hunk_header(hunk_header: str) -> Optional[str]:
    """Extract function name from the @@ context line."""
    if not hunk_header.startswith("@@"):
        return None

    # Find the second @@ to get the function context
    parts = hunk_header.split("@@", 2)
    if len(parts) < 3:
        return None

    function_context = parts[2].strip()
    if not function_context:
        return None

    # Look for function definition pattern
    paren_pos = function_context.find("(")
    if paren_pos > 0:
        before_paren = function_context[:paren_pos]
        tokens = before_paren.split()
        if tokens:
            name = tokens[-1].lstrip("*")
            if name and all(c.isalnum() or c == "_" for c in name) and name not in C_KEYWORDS:
                return name

    return None


def run_semcode_diffinfo(diff_text: str, cwd: Optional[str] = None, skip_semcode: bool = False) -> Optional[dict]:
    """
    Run semcode --diffinfo with diff piped to stdin.
    Returns a dict with modified_functions (with per-function calls), modified_types, modified_macros.

    Output format:
    {
        "modified_functions": [{"name": "...", "types": [...], "callers": [...], "calls": [...]}, ...],
        "modified_types": [...],
        "modified_macros": [...]
    }
    """
    def build_result_from_python(py_result: DiffParseResult) -> dict:
        """Build result dict from Python parse result with per-function calls."""
        functions = []
        for name in sorted(py_result.modified_functions):
            # Get calls for this function, excluding self-references
            calls = sorted(c for c in py_result.function_calls.get(name, set()) if c != name)
            functions.append({
                "name": name,
                "types": [],
                "callers": [],
                "calls": calls,
            })
        return {
            "modified_functions": functions,
            "modified_types": sorted(py_result.modified_types),
            "modified_macros": sorted(py_result.modified_macros),
        }

    if skip_semcode:
        # Use pure Python implementation
        result = parse_unified_diff_python(diff_text)
        return build_result_from_python(result)

    # Check if .semcode.db exists
    db_path = Path(cwd or ".") / ".semcode.db"
    if not db_path.exists():
        # Fall back to pure Python implementation
        result = parse_unified_diff_python(diff_text)
        return build_result_from_python(result)

    try:
        result = subprocess.run(
            ["semcode", "--diffinfo"],
            input=diff_text,
            capture_output=True,
            text=True,
            cwd=cwd,
        )
        if result.returncode != 0:
            print(f"Warning: semcode --diffinfo failed: {result.stderr}", file=sys.stderr)
            # Fall back to pure Python
            py_result = parse_unified_diff_python(diff_text)
            return build_result_from_python(py_result)
    except FileNotFoundError:
        print("Warning: semcode not found, using Python diff parser", file=sys.stderr)
        result = parse_unified_diff_python(diff_text)
        return build_result_from_python(result)

    try:
        return json.loads(result.stdout.strip())
    except json.JSONDecodeError as e:
        print(f"Warning: failed to parse semcode output: {e}", file=sys.stderr)
        # Fall back to pure Python
        py_result = parse_unified_diff_python(diff_text)
        return build_result_from_python(py_result)


def is_semcode_available(cwd: Optional[str] = None, skip_semcode: bool = False) -> bool:
    """Check if semcode is available and usable."""
    if skip_semcode:
        return False

    # Check if .semcode.db exists
    db_path = Path(cwd or ".") / ".semcode.db"
    if not db_path.exists():
        return False

    # Check if semcode binary is available
    try:
        result = subprocess.run(
            ["semcode", "--help"],
            capture_output=True,
            text=True,
            cwd=cwd,
        )
        return result.returncode == 0
    except FileNotFoundError:
        return False


# C keywords that should not be treated as function names
C_KEYWORDS = {
    "if", "else", "for", "while", "do", "switch", "case", "default",
    "return", "break", "continue", "goto", "sizeof", "typeof",
    "int", "char", "float", "double", "void", "long", "short",
    "signed", "unsigned", "const", "static", "extern", "inline",
    "struct", "union", "enum", "typedef", "auto", "register", "volatile",
    "__attribute__", "__always_inline", "noinline", "bool", "_Bool",
}


def find_function_start(lines: list[str], target_line: int) -> Optional[int]:
    """
    Walk backwards from target_line to find the start of a function definition.
    Returns the line index of the function start, or None if not found.
    """
    if target_line >= len(lines):
        return None

    # Walk backwards looking for function definition start
    for i in range(target_line, max(-1, target_line - 100), -1):
        line = lines[i]
        stripped = line.strip()

        # Skip empty lines and comments
        if not stripped or stripped.startswith("//") or stripped.startswith("/*"):
            continue

        # Skip preprocessor directives (except #define which we handle separately)
        if stripped.startswith("#") and not stripped.startswith("#define"):
            continue

        # Check if this line starts at column 0 and looks like a function definition
        if not line[0].isspace() if line else False:
            # Must have parentheses (function parameters)
            if "(" in stripped:
                # Look ahead to see if there's a { within next 20 lines
                for j in range(i, min(len(lines), i + 20)):
                    if "{" in lines[j]:
                        return i
                    # Stop if we hit another non-whitespace non-continuation line
                    if j > i and lines[j].strip() and not lines[j].strip().startswith("{"):
                        if not lines[j - 1].strip().endswith(",") and not lines[j - 1].strip().endswith("("):
                            break

            # Check for struct/union/enum definition
            if stripped.startswith(("struct ", "union ", "enum ", "typedef struct ", "typedef union ", "typedef enum ")):
                if "{" in stripped:
                    return i
                # Look for { on next lines
                for j in range(i + 1, min(len(lines), i + 10)):
                    if "{" in lines[j]:
                        return i
                    if lines[j].strip() and not lines[j].strip().startswith("{"):
                        break

    return None


def find_function_end(lines: list[str], start_line: int) -> Optional[int]:
    """
    Find the end of a function or type definition starting at start_line.
    Tracks brace depth to find the matching closing brace.
    Returns the line index of the closing brace, or None if not found.
    """
    brace_depth = 0
    found_opening = False
    in_string = False
    in_char = False
    in_line_comment = False
    in_block_comment = False

    for i in range(start_line, min(len(lines), start_line + 2000)):
        line = lines[i]
        j = 0
        while j < len(line):
            char = line[j]
            next_char = line[j + 1] if j + 1 < len(line) else ""

            # Handle comments
            if in_line_comment:
                break  # Rest of line is comment
            if in_block_comment:
                if char == "*" and next_char == "/":
                    in_block_comment = False
                    j += 2
                    continue
                j += 1
                continue

            # Check for comment start
            if char == "/" and next_char == "/":
                in_line_comment = True
                break
            if char == "/" and next_char == "*":
                in_block_comment = True
                j += 2
                continue

            # Handle strings
            if char == '"' and not in_char:
                # Check for escape
                if j > 0 and line[j - 1] == "\\":
                    j += 1
                    continue
                in_string = not in_string
                j += 1
                continue

            # Handle char literals
            if char == "'" and not in_string:
                if j > 0 and line[j - 1] == "\\":
                    j += 1
                    continue
                in_char = not in_char
                j += 1
                continue

            # Skip content inside strings/chars
            if in_string or in_char:
                j += 1
                continue

            # Track braces
            if char == "{":
                brace_depth += 1
                found_opening = True
            elif char == "}":
                brace_depth -= 1
                if found_opening and brace_depth == 0:
                    return i

            j += 1

        in_line_comment = False  # Reset for next line

    return None


def extract_function_name_from_line(line: str) -> Optional[str]:
    """Extract function name from a function definition line."""
    # Find the opening parenthesis
    paren_pos = line.find("(")
    if paren_pos == -1:
        return None

    before_paren = line[:paren_pos].strip()

    # Split by whitespace and get the last token before the paren
    tokens = before_paren.split()
    if not tokens:
        return None

    name = tokens[-1]

    # Remove pointer/reference markers
    name = name.lstrip("*&")

    # Validate as identifier
    if name and name not in C_KEYWORDS:
        if name[0].isalpha() or name[0] == "_":
            if all(c.isalnum() or c == "_" for c in name):
                return name

    return None


def extract_type_name_from_line(line: str) -> Optional[tuple[str, str]]:
    """
    Extract type name from a struct/union/enum definition line.
    Returns (type_keyword, type_name) tuple, or None if not found.
    """
    stripped = line.strip()

    # Determine type keyword
    if stripped.startswith("typedef struct "):
        keyword = "struct"
        after = stripped[len("typedef struct "):].strip()
    elif stripped.startswith("typedef union "):
        keyword = "union"
        after = stripped[len("typedef union "):].strip()
    elif stripped.startswith("typedef enum "):
        keyword = "enum"
        after = stripped[len("typedef enum "):].strip()
    elif stripped.startswith("struct "):
        keyword = "struct"
        after = stripped[len("struct "):].strip()
    elif stripped.startswith("union "):
        keyword = "union"
        after = stripped[len("union "):].strip()
    elif stripped.startswith("enum "):
        keyword = "enum"
        after = stripped[len("enum "):].strip()
    else:
        return None

    # Extract name (first identifier before { or whitespace)
    match = re.match(r"(\w+)", after)
    if match:
        return (keyword, match.group(1))

    return None


def get_definition_for_change(
    file_path: str,
    function_name: str,
    hunk_new_start: int,
    git_dir: Optional[str] = None
) -> Optional[str]:
    """
    Extract the full function or type definition from the source file.
    Returns the definition as a string, or None if not found.
    """
    # Construct full path
    if git_dir:
        full_path = Path(git_dir) / file_path
    else:
        full_path = Path(file_path)

    if not full_path.exists():
        return None

    try:
        with open(full_path, "r", encoding="utf-8", errors="replace") as f:
            lines = f.readlines()
    except (IOError, OSError):
        return None

    # Convert to 0-indexed
    target_line = hunk_new_start - 1
    if target_line < 0 or target_line >= len(lines):
        return None

    # Find function start
    start_line = find_function_start(lines, target_line)
    if start_line is None:
        return None

    # Find function end
    end_line = find_function_end(lines, start_line)
    if end_line is None:
        return None

    # Extract definition
    definition_lines = lines[start_line:end_line + 1]
    definition = "".join(definition_lines)

    # Sanity check: don't return extremely long definitions
    if len(definition) > 10000:
        # Return truncated version with ellipsis
        truncated_lines = definition_lines[:100]
        return "".join(truncated_lines) + "\n... [truncated, definition too long]\n"

    return definition


def parse_hunk_line_numbers(hunk_header: str) -> Optional[tuple[int, int]]:
    """
    Parse new start line and count from a hunk header.
    Returns (new_start, new_count) or None if parsing fails.

    Handles formats like:
    - "@@ -10,5 +20,5 @@ function_name"
    - "@@ -10,5 +20,5 @@"
    - Combined headers like "-10,5 +20,5 + -30,5 +40,5"
    """
    # Try standard format first
    match = re.search(r"\+(\d+)(?:,(\d+))?", hunk_header)
    if match:
        new_start = int(match.group(1))
        new_count = int(match.group(2)) if match.group(2) else 1
        return (new_start, new_count)

    return None


def get_definitions_for_hunk(
    file_path: str,
    hunk: "Hunk",
    git_dir: Optional[str] = None
) -> dict[str, str]:
    """
    Extract definitions for all functions/types modified in a hunk.
    Returns a dict mapping function/type names to their definitions.
    """
    definitions = {}

    # Get the function from hunk context
    func_name = extract_function_from_context(hunk.function_context)
    if func_name and func_name != "unknown":
        defn = get_definition_for_change(file_path, func_name, hunk.new_start, git_dir)
        if defn:
            definitions[func_name] = defn

    return definitions


def parse_commit_message(git_show_output: str) -> dict:
    """Parse commit metadata from git show output."""
    lines = git_show_output.split("\n")

    # Parse header
    sha = ""
    author = ""
    date = ""
    subject = ""
    body_lines = []

    # Tags
    tags = {
        "fixes": None,
        "cc": [],
        "signed-off-by": [],
        "reviewed-by": [],
        "acked-by": [],
        "tested-by": [],
        "link": [],
    }

    in_body = False
    past_subject = False

    for line in lines:
        if line.startswith("commit "):
            sha = line.split()[1]
        elif line.startswith("Author: "):
            author = line[8:].strip()
        elif line.startswith("Date: "):
            date = line[6:].strip()
        elif line.startswith("diff --git"):
            break
        elif line.startswith("    "):
            content = line[4:]
            if not past_subject:
                subject = content
                past_subject = True
            elif content:
                body_lines.append(content)
                # Parse tags
                if content.startswith("Fixes: "):
                    tags["fixes"] = content[7:].strip()
                elif content.startswith("Cc: "):
                    tags["cc"].append(content[4:].strip())
                elif content.startswith("Signed-off-by: "):
                    tags["signed-off-by"].append(content[15:].strip())
                elif content.startswith("Reviewed-by: "):
                    tags["reviewed-by"].append(content[13:].strip())
                elif content.startswith("Acked-by: "):
                    tags["acked-by"].append(content[10:].strip())
                elif content.startswith("Tested-by: "):
                    tags["tested-by"].append(content[11:].strip())
                elif content.startswith("Link: "):
                    tags["link"].append(content[6:].strip())

    return {
        "sha": sha,
        "author": author,
        "date": date,
        "subject": subject,
        "body": "\n".join(body_lines),
        "tags": tags,
    }


def parse_diff(diff_text: str, diffinfo: Optional[dict] = None) -> list[FileChange]:
    """Parse unified diff into structured format.

    If diffinfo is provided (global diffinfo from semcode), it can be used to look up
    function information by name.
    """
    files = []
    current_file = None
    current_hunk = None

    lines = diff_text.split("\n")
    i = 0

    while i < len(lines):
        line = lines[i]

        # New file
        if line.startswith("diff --git "):
            if current_file and current_hunk:
                current_file.hunks.append(current_hunk)
            if current_file:
                files.append(current_file)

            # Parse file paths
            match = re.match(r"diff --git a/(.+) b/(.+)", line)
            if match:
                current_file = FileChange(
                    old_path=match.group(1),
                    new_path=match.group(2),
                )
            current_hunk = None

        # Hunk header
        elif line.startswith("@@ "):
            if current_file and current_hunk:
                current_file.hunks.append(current_hunk)

            # Parse @@ -old_start,old_count +new_start,new_count @@ function
            match = re.match(
                r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)",
                line,
            )
            if match:
                # Get diffinfo for this hunk by looking up function name
                hunk_diffinfo = None
                func_context = match.group(5).strip()
                if diffinfo and func_context:
                    # Try to find matching function info
                    func_name = extract_function_from_context(func_context)
                    if func_name and func_name != "unknown":
                        for func_info in diffinfo.get("modified_functions", []):
                            if func_info.get("name") == func_name:
                                hunk_diffinfo = {
                                    "modifies": func_name,
                                    "types": func_info.get("types", []),
                                    "callers": func_info.get("callers", []),
                                    "calls": func_info.get("calls", []),
                                }
                                break

                current_hunk = Hunk(
                    header=line,
                    old_start=int(match.group(1)),
                    old_count=int(match.group(2) or 1),
                    new_start=int(match.group(3)),
                    new_count=int(match.group(4) or 1),
                    function_context=func_context,
                    diffinfo=hunk_diffinfo,
                )

        # Hunk content
        elif current_hunk is not None and (
            line.startswith("+") or line.startswith("-") or
            line.startswith(" ") or line == ""
        ):
            current_hunk.lines.append(line)

        i += 1

    # Don't forget the last file/hunk
    if current_file and current_hunk:
        current_file.hunks.append(current_hunk)
    if current_file:
        files.append(current_file)

    return files


def extract_function_from_context(function_context: str) -> str:
    """Extract function name from the @@ context line."""
    if not function_context:
        return "unknown"

    # Try to match function definition patterns
    # static int foo_bar(...)
    # void *some_func(
    # SYSCALL_DEFINE3(read, ...
    patterns = [
        r"(\w+)\s*\([^)]*\)\s*$",  # func_name(...)
        r"(\w+)\s*\($",  # func_name(
        r"^(?:static\s+)?(?:\w+\s+\*?\s*)?(\w+)\s*\(",  # type func(
        r"SYSCALL_DEFINE\d+\((\w+)",  # SYSCALL_DEFINEn(name
        r"DEFINE_\w+\((\w+)",  # DEFINE_*(name
        r"^struct\s+(\w+)",  # struct name
    ]

    for pattern in patterns:
        match = re.search(pattern, function_context)
        if match:
            return match.group(1)

    # Fallback: just use the context as-is or first word
    words = function_context.split()
    if words:
        # Clean up any trailing punctuation
        return re.sub(r"[^a-zA-Z0-9_]", "", words[-1]) or "unknown"

    return "unknown"


def find_function_definitions_in_hunk(hunk_lines: list[str]) -> list[tuple[str, int, int]]:
    """
    Find new function definitions within a hunk.
    Returns list of (function_name, start_line_idx, end_line_idx).
    Only considers added lines (starting with +).
    """
    functions = []

    # Patterns for function definitions
    func_def_patterns = [
        # static type func_name(...)
        r"^\+\s*(?:static\s+)?(?:inline\s+)?(?:__always_inline\s+)?(?:noinline\s+)?(?:const\s+)?(?:\w+\s+\*?\s*)+(\w+)\s*\([^;]*$",
        # Just the function name with opening paren at end of line
        r"^\+\s*(?:static\s+)?(?:\w+\s+\*?\s*)*(\w+)\s*\(\s*$",
    ]

    # Track brace depth to find function boundaries
    i = 0
    while i < len(hunk_lines):
        line = hunk_lines[i]

        # Skip non-added lines and context for function detection
        if not line.startswith("+"):
            i += 1
            continue

        # Check if this looks like a function definition
        func_name = None
        for pattern in func_def_patterns:
            match = re.match(pattern, line)
            if match:
                func_name = match.group(1)
                break

        if not func_name:
            # Check for multi-line function signatures
            # e.g., static int\n+func_name(args)
            if i > 0 and re.match(r"^\+\s*(\w+)\s*\(", line):
                prev = hunk_lines[i-1] if i > 0 else ""
                if prev.startswith("+") and re.search(r"(?:static|int|void|bool|u\d+|s\d+|long|unsigned)\s*$", prev):
                    match = re.match(r"^\+\s*(\w+)\s*\(", line)
                    if match:
                        func_name = match.group(1)

        if func_name and func_name not in ("if", "for", "while", "switch", "return", "sizeof", "typeof"):
            # Found a function definition, find where it ends
            start_idx = i
            brace_depth = 0
            found_opening_brace = False
            end_idx = i

            for j in range(i, len(hunk_lines)):
                check_line = hunk_lines[j]
                if check_line.startswith("+") or check_line.startswith(" "):
                    content = check_line[1:] if check_line else ""
                    # Count braces (simple approach, doesn't handle strings/comments)
                    for char in content:
                        if char == "{":
                            brace_depth += 1
                            found_opening_brace = True
                        elif char == "}":
                            brace_depth -= 1

                    if found_opening_brace and brace_depth == 0:
                        end_idx = j
                        break

            if found_opening_brace:
                functions.append((func_name, start_idx, end_idx))
                i = end_idx + 1
                continue

        i += 1

    return functions


def find_changed_function_in_hunk(hunk_lines: list[str]) -> Optional[tuple[str, int]]:
    """
    Scan hunk content for a function definition on context or modified lines
    that contains the actual changes.

    The @@ header shows the function in scope at the start of the hunk, but
    if the hunk starts with trailing context from a preceding function and
    then enters a new function definition, the @@ header function is wrong.

    Returns (func_name, line_index) if a definition is found in the hunk body
    with modifications after it. Returns None if no such definition exists.
    """
    for i, line in enumerate(hunk_lines):
        # Get content without diff prefix
        if line.startswith((" ", "+", "-")):
            content = line[1:]
        elif line == "":
            continue
        else:
            content = line

        # Function definitions start at column 0 (no leading whitespace)
        if not content or content[0].isspace():
            continue

        stripped = content.strip()
        if not stripped:
            continue

        # Skip comments and preprocessor
        if stripped.startswith(("//", "/*", "*", "#")):
            continue

        # Skip closing braces and labels
        if stripped == "}" or (stripped.endswith(":") and "(" not in stripped):
            continue

        # Must have parens (function parameters)
        if "(" not in stripped:
            continue

        # Skip control flow keywords
        before_paren = stripped.split("(")[0].split()
        if before_paren and before_paren[-1] in (
            "if", "while", "for", "switch", "return", "sizeof", "typeof", "case", "else"
        ):
            continue

        # Check for opening brace on this or following lines
        has_brace = "{" in stripped
        if not has_brace:
            for j in range(i + 1, min(len(hunk_lines), i + 20)):
                jline = hunk_lines[j]
                if jline.startswith((" ", "+", "-")):
                    jcontent = jline[1:]
                elif jline == "":
                    continue
                else:
                    jcontent = jline

                if "{" in jcontent:
                    has_brace = True
                    break

                jstripped = jcontent.strip()
                if not jstripped:
                    continue

                # Continuation of function signature (indented or ends with , ( ) )
                is_continuation = (
                    (jcontent and jcontent[0].isspace()) or
                    jstripped.endswith((",", "(", ")"))
                )
                if not is_continuation:
                    break

        if not has_brace:
            continue

        func_name = extract_function_name_from_line(stripped)
        if not func_name:
            continue

        # Verify there are modifications after this definition
        has_mods_after = any(
            hunk_lines[j].startswith(("+", "-"))
            and not hunk_lines[j].startswith(("+++", "---"))
            for j in range(i + 1, len(hunk_lines))
        )
        if has_mods_after:
            return (func_name, i)

        return None

    return None


def split_hunk_by_functions(hunk: Hunk, file_path: str, global_diffinfo: Optional[dict] = None) -> list[tuple[str, str, str, bool, Optional[dict]]]:
    """
    Split a hunk into multiple segments if it contains multiple function definitions.
    Returns list of (function_name, hunk_header, hunk_content, is_new_function, diffinfo).
    is_new_function is True if this segment defines a new function.
    diffinfo is looked up from global_diffinfo by function name, or uses hunk.diffinfo as fallback.
    """
    def lookup_diffinfo(func_name: str) -> Optional[dict]:
        """Look up diffinfo for a function from global diffinfo."""
        if global_diffinfo:
            for func_info in global_diffinfo.get("modified_functions", []):
                if func_info.get("name") == func_name:
                    return {
                        "modifies": func_name,
                        "types": func_info.get("types", []),
                        "callers": func_info.get("callers", []),
                        "calls": func_info.get("calls", []),
                    }
        return hunk.diffinfo

    # Find function definitions in added lines
    functions = find_function_definitions_in_hunk(hunk.lines)

    if len(functions) == 0:
        # No new function definitions - this is a modification to existing function
        # Check if the hunk body contains a function definition that differs from
        # the @@ header (e.g., when the hunk starts with context from a preceding function)
        body_result = find_changed_function_in_hunk(hunk.lines)
        if body_result:
            body_func, body_func_line = body_result

            # Check if there are also modifications before the body function definition
            # (i.e., changes at the tail of the @@ header function)
            has_mods_before = any(
                hunk.lines[j].startswith(("+", "-"))
                and not hunk.lines[j].startswith(("+++", "---"))
                for j in range(0, body_func_line)
            )

            if has_mods_before:
                # Split into two segments: @@ header function + body function
                header_func = extract_function_from_context(hunk.function_context)
                before_lines = hunk.lines[:body_func_line]
                after_lines = hunk.lines[body_func_line:]

                before_content = hunk.header + "\n" + "\n".join(before_lines)
                after_content = hunk.header + "\n" + "\n".join(after_lines)

                before_diffinfo = lookup_diffinfo(header_func) if header_func != "unknown" else hunk.diffinfo
                after_diffinfo = lookup_diffinfo(body_func)

                return [
                    (header_func, hunk.header, before_content, False, before_diffinfo),
                    (body_func, hunk.header, after_content, False, after_diffinfo),
                ]
            else:
                # Only the body function is modified
                func_name = body_func
                diffinfo = lookup_diffinfo(func_name) if func_name != "unknown" else hunk.diffinfo
                return [(func_name, hunk.header, hunk.header + "\n" + "\n".join(hunk.lines), False, diffinfo)]
        else:
            func_name = extract_function_from_context(hunk.function_context)
            diffinfo = lookup_diffinfo(func_name) if func_name != "unknown" else hunk.diffinfo
            return [(func_name, hunk.header, hunk.header + "\n" + "\n".join(hunk.lines), False, diffinfo)]

    if len(functions) == 1:
        # Single new function definition
        func_name = functions[0][0]
        diffinfo = lookup_diffinfo(func_name)
        return [(func_name, hunk.header, hunk.header + "\n" + "\n".join(hunk.lines), True, diffinfo)]

    # Multiple functions found - create separate segments (all are new functions)
    segments = []

    for func_name, start_idx, end_idx in functions:
        # Extract just the lines for this function
        func_lines = hunk.lines[start_idx:end_idx + 1]

        # Create a modified hunk header (approximate line numbers)
        new_header = f"@@ (within {hunk.header.split('@@')[1].strip()}) @@ {func_name}"

        content = new_header + "\n" + "\n".join(func_lines)
        diffinfo = lookup_diffinfo(func_name)
        segments.append((func_name, new_header, content, True, diffinfo))

    return segments


def create_change_json(change: Change) -> dict:
    """Generate FILE-N-CHANGE-M.json content as a dict."""
    result = {
        "id": change.id,
        "file": change.file,
        "function": change.function,
        "hunk_header": change.hunk_header,
        "diff": change.hunk_content,
        "total_lines": change.total_lines,
    }

    # Add diffinfo fields if available
    if change.diffinfo:
        di = change.diffinfo
        if di.get("modifies"):
            result["modifies"] = di["modifies"]
        if di.get("types"):
            result["types"] = di["types"]
        if di.get("calls"):
            result["calls"] = di["calls"]
        if di.get("callers"):
            result["callers"] = di["callers"]

    # Add definition if available (when semcode unavailable)
    if change.definition:
        result["definition"] = change.definition

    return result


def extract_group_functions(group: dict) -> set[str]:
    """
    Extract all functions referenced by a FILE-N group.
    Includes: modified functions, callers, and callees from all changes.
    """
    functions = set()
    for change in group["changes"]:
        # Add the function name from the change (may be comma-separated for new functions)
        if change.function and change.function != "unknown":
            for func in change.function.split(", "):
                func = func.strip()
                if func:
                    functions.add(func)

        # Add functions from diffinfo
        if change.diffinfo:
            di = change.diffinfo
            if di.get("modifies"):
                functions.add(di["modifies"])
            for caller in di.get("callers", []):
                functions.add(caller)
            for callee in di.get("calls", []):
                functions.add(callee)

    return functions


def calculate_function_overlap(funcs1: set[str], funcs2: set[str]) -> float:
    """
    Calculate the overlap ratio between two sets of functions.
    Returns the ratio of intersection to the smaller set size.
    This ensures small groups can still be merged if they share most of their functions.
    """
    if not funcs1 or not funcs2:
        return 0.0

    intersection = funcs1 & funcs2
    # Use the smaller set as denominator to allow small groups to merge with larger ones
    smaller_size = min(len(funcs1), len(funcs2))
    return len(intersection) / smaller_size


def combine_groups_by_similarity(file_groups: list[dict]) -> list[dict]:
    """
    Combine FILE-N groups that share >50% of their functions.
    Respects MAX_SIMILARITY_COMBINED_LINES limit.
    Uses greedy approach: repeatedly merge the most similar pair until no more merges possible.
    """
    if len(file_groups) <= 1:
        return file_groups

    # Build function sets for each group
    group_functions: list[set[str]] = []
    for group in file_groups:
        group_functions.append(extract_group_functions(group))

    # Track which groups have been merged (by index)
    merged_into: dict[int, int] = {}  # maps original index -> merged group index
    working_groups = list(file_groups)  # will be modified during merging
    working_funcs = list(group_functions)

    changed = True
    while changed:
        changed = False
        best_overlap = 0.0
        best_pair = None

        # Find the best pair to merge
        for i in range(len(working_groups)):
            if working_groups[i] is None:
                continue
            for j in range(i + 1, len(working_groups)):
                if working_groups[j] is None:
                    continue

                # Check if combined size would exceed limit
                combined_lines = working_groups[i]["total_lines"] + working_groups[j]["total_lines"]
                if combined_lines > MAX_SIMILARITY_COMBINED_LINES:
                    continue

                overlap = calculate_function_overlap(working_funcs[i], working_funcs[j])
                if overlap >= MIN_FUNCTION_OVERLAP_RATIO and overlap > best_overlap:
                    best_overlap = overlap
                    best_pair = (i, j)

        # Merge the best pair if found
        if best_pair:
            i, j = best_pair
            # Merge j into i
            gi = working_groups[i]
            gj = working_groups[j]

            # Combine files lists
            files_i = gi.get("files", [gi["file"]] if isinstance(gi["file"], str) else gi["file"])
            files_j = gj.get("files", [gj["file"]] if isinstance(gj["file"], str) else gj["file"])
            combined_files = list(files_i) + [f for f in files_j if f not in files_i]

            # Create merged group
            merged = {
                "file_num": None,  # Will be reassigned later
                "file": combined_files if len(combined_files) > 1 else combined_files[0],
                "files": combined_files,
                "changes": list(gi["changes"]) + list(gj["changes"]),
                "total_lines": gi["total_lines"] + gj["total_lines"],
            }
            working_groups[i] = merged
            working_groups[j] = None
            working_funcs[i] = working_funcs[i] | working_funcs[j]
            working_funcs[j] = set()
            changed = True

    # Collect non-None groups and reassign file_num
    result = [g for g in working_groups if g is not None]
    for idx, group in enumerate(result, start=1):
        group["file_num"] = idx
        # Update file_num and change_num in each Change object
        change_num = 1
        for change in group["changes"]:
            change.file_num = idx
            change.change_num = change_num
            change_num += 1

    return result


def main():
    parser = argparse.ArgumentParser(
        description="Create CHANGE-N categorization for kernel patch review"
    )
    parser.add_argument(
        "commit",
        nargs="?",
        default="HEAD",
        help="Commit reference (SHA, HEAD, etc.) or path to patch file",
    )
    parser.add_argument(
        "-o", "--output-dir",
        default="./review-context",
        help="Output directory (default: ./review-context)",
    )
    parser.add_argument(
        "-C", "--git-dir",
        default=None,
        help="Git repository directory",
    )
    parser.add_argument(
        "--no-semcode",
        action="store_true",
        help="Skip semcode calls (extract definitions from source instead)",
    )

    args = parser.parse_args()

    # Determine if we should use semcode
    skip_semcode = args.no_semcode
    use_semcode = not skip_semcode and is_semcode_available(cwd=args.git_dir, skip_semcode=skip_semcode)

    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)

    # Get commit info
    if os.path.isfile(args.commit):
        # Patch file
        with open(args.commit) as f:
            git_show_output = f.read()
    else:
        # Git commit
        git_show_output = run_git(
            ["show", "--format=full", args.commit],
            cwd=args.git_dir,
        )

    # Write change.diff
    with open(output_dir / "change.diff", "w") as f:
        f.write(git_show_output)

    # Parse commit message
    commit_info = parse_commit_message(git_show_output)

    # Get list of modified files
    if os.path.isfile(args.commit):
        # Parse from diff
        files_changed = []
        for line in git_show_output.split("\n"):
            if line.startswith("diff --git "):
                match = re.match(r"diff --git a/(.+) b/(.+)", line)
                if match:
                    files_changed.append(match.group(2))
    else:
        files_output = run_git(
            ["diff-tree", "--no-commit-id", "--name-only", "-r", args.commit],
            cwd=args.git_dir,
        )
        files_changed = [f for f in files_output.strip().split("\n") if f]

    commit_info["files-changed"] = files_changed

    # Detect subsystems from file paths
    subsystems = set()
    for f in files_changed:
        if f.startswith("net/") or f.startswith("drivers/net/"):
            subsystems.add("networking")
        elif f.startswith("mm/"):
            subsystems.add("mm")
        elif f.startswith("fs/btrfs/"):
            subsystems.add("btrfs")
        elif f.startswith("fs/"):
            subsystems.add("vfs")
        elif f.startswith("kernel/sched/"):
            subsystems.add("scheduler")
        elif f.startswith("kernel/bpf/"):
            subsystems.add("bpf")
        elif f.startswith("block/") or f.startswith("drivers/nvme/"):
            subsystems.add("block")

    commit_info["subsystems"] = list(subsystems)

    # Write commit-message.json
    with open(output_dir / "commit-message.json", "w") as f:
        json.dump(commit_info, f, indent=2)

    # Parse diff
    # Extract just the diff portion
    diff_start = git_show_output.find("diff --git ")
    if diff_start == -1:
        print("No diff found in commit", file=sys.stderr)
        sys.exit(1)

    diff_text = git_show_output[diff_start:]

    # Run semcode --diffinfo to get static analysis info (or use Python fallback)
    diffinfo = run_semcode_diffinfo(diff_text, cwd=args.git_dir, skip_semcode=skip_semcode)

    file_changes = parse_diff(diff_text, diffinfo)

    # Create intermediate change entries (without file_num assigned yet)
    # These will be grouped by source file and then assigned FILE-N numbers
    #
    # Structure: changes_by_file[file_path] = [(function, hunk_header, hunk_content, total_lines, diffinfo), ...]

    # Collect segments, separating modifications from new function definitions
    # modifications_by_key[file][function] = [(hunk_header, hunk_content, added_lines, diffinfo), ...]
    modifications_by_key: dict[str, dict[str, list[tuple[str, str, int, Optional[dict]]]]] = defaultdict(lambda: defaultdict(list))
    # new_functions_by_file[file] = [(func_name, hunk_header, hunk_content, total_lines, diffinfo), ...]
    new_functions_by_file: dict[str, list[tuple[str, str, str, int, Optional[dict]]]] = defaultdict(list)
    total_hunks = 0

    for file_change in file_changes:
        for hunk in file_change.hunks:
            total_hunks += 1
            # Split hunk if it contains multiple function definitions
            segments = split_hunk_by_functions(hunk, file_change.new_path, diffinfo)

            for func_name, hunk_header, hunk_content, is_new_function, segment_diffinfo in segments:
                if is_new_function:
                    total_lines = count_total_lines(hunk_content)
                    new_functions_by_file[file_change.new_path].append(
                        (func_name, hunk_header, hunk_content, total_lines, segment_diffinfo)
                    )
                else:
                    added_lines = count_added_lines(hunk_content)
                    modifications_by_key[file_change.new_path][func_name].append(
                        (hunk_header, hunk_content, added_lines, segment_diffinfo)
                    )

    # Build intermediate changes grouped by file path
    # changes_by_file[file_path] = [(function, hunk_header, hunk_content, total_lines, diffinfo), ...]
    changes_by_file: dict[str, list[tuple[str, str, str, int, Optional[dict]]]] = defaultdict(list)

    def merge_diffinfo(info_list: list[Optional[dict]]) -> Optional[dict]:
        """Merge multiple diffinfo dicts into one, combining lists."""
        infos = [i for i in info_list if i is not None]
        if not infos:
            return None
        merged = {
            "modifies": None,
            "types": [],
            "calls": [],
            "callers": [],
        }
        for info in infos:
            if info.get("modifies") and not merged["modifies"]:
                merged["modifies"] = info["modifies"]
            for key in ("types", "calls", "callers"):
                for item in info.get(key, []):
                    if item not in merged[key]:
                        merged[key].append(item)
        return merged

    # Process modifications: combine by (file, function) up to MAX_COMBINED_ADDED_LINES
    for file_path in modifications_by_key:
        for func_name, segment_list in modifications_by_key[file_path].items():
            current_headers = []
            current_contents = []
            current_added = 0
            current_diffinfos = []

            for hunk_header, hunk_content, added_lines, diffinfo in segment_list:
                if current_added > 0 and current_added + added_lines > MAX_COMBINED_ADDED_LINES:
                    # Flush current group
                    combined_header = " + ".join(current_headers) if len(current_headers) > 1 else current_headers[0]
                    combined_content = "\n\n".join(current_contents)
                    changes_by_file[file_path].append(
                        (func_name, combined_header, combined_content, count_total_lines(combined_content), merge_diffinfo(current_diffinfos))
                    )
                    current_headers = []
                    current_contents = []
                    current_added = 0
                    current_diffinfos = []

                header_part = hunk_header.split("@@")[1].strip() if "@@" in hunk_header else hunk_header
                current_headers.append(header_part)
                current_contents.append(hunk_content)
                current_added += added_lines
                current_diffinfos.append(diffinfo)

            if current_contents:
                combined_header = " + ".join(current_headers) if len(current_headers) > 1 else current_headers[0]
                combined_content = "\n\n".join(current_contents)
                changes_by_file[file_path].append(
                    (func_name, combined_header, combined_content, count_total_lines(combined_content), merge_diffinfo(current_diffinfos))
                )

    # Process new functions: combine by file up to MAX_NEW_FUNCTION_COMBINED_LINES total lines
    for file_path, func_list in new_functions_by_file.items():
        current_func_names = []
        current_headers = []
        current_contents = []
        current_total_lines = 0
        current_diffinfos = []

        for func_name, hunk_header, hunk_content, total_lines, diffinfo in func_list:
            if current_total_lines > 0 and current_total_lines + total_lines > MAX_NEW_FUNCTION_COMBINED_LINES:
                # Flush current group
                combined_func_name = ", ".join(current_func_names)
                combined_header = " + ".join(current_headers) if len(current_headers) > 1 else current_headers[0]
                combined_content = "\n\n".join(current_contents)
                changes_by_file[file_path].append(
                    (combined_func_name, combined_header, combined_content, current_total_lines, merge_diffinfo(current_diffinfos))
                )
                current_func_names = []
                current_headers = []
                current_contents = []
                current_total_lines = 0
                current_diffinfos = []

            current_func_names.append(func_name)
            header_part = hunk_header.split("@@")[1].strip() if "@@" in hunk_header else hunk_header
            current_headers.append(header_part)
            current_contents.append(hunk_content)
            current_total_lines += total_lines
            current_diffinfos.append(diffinfo)

        if current_contents:
            combined_func_name = ", ".join(current_func_names)
            combined_header = " + ".join(current_headers) if len(current_headers) > 1 else current_headers[0]
            combined_content = "\n\n".join(current_contents)
            changes_by_file[file_path].append(
                (combined_func_name, combined_header, combined_content, current_total_lines, merge_diffinfo(current_diffinfos))
            )

    # Now assign FILE-N numbers, splitting files that exceed MAX_LINES_PER_FILE
    # file_groups[file_num] = {"file": path, "changes": [Change, ...], "total_lines": int}
    file_groups: list[dict] = []
    file_num = 1

    # Process files in a stable order (by path)
    for file_path in sorted(changes_by_file.keys()):
        file_changes_list = changes_by_file[file_path]

        current_group_changes = []
        current_group_lines = 0
        change_num = 1

        for func_name, hunk_header, hunk_content, total_lines, diffinfo in file_changes_list:
            # Check if adding this change would exceed the limit
            if current_group_lines > 0 and current_group_lines + total_lines > MAX_LINES_PER_FILE:
                # Flush current group as a FILE-N
                file_groups.append({
                    "file_num": file_num,
                    "file": file_path,
                    "changes": current_group_changes,
                    "total_lines": current_group_lines,
                })
                file_num += 1
                current_group_changes = []
                current_group_lines = 0
                change_num = 1

            # Extract definition when semcode is not available
            definition = None
            if not use_semcode and func_name != "unknown":
                # Parse line number from hunk header
                line_nums = parse_hunk_line_numbers(hunk_header)
                if line_nums:
                    new_start, _ = line_nums
                    definition = get_definition_for_change(
                        file_path, func_name, new_start, args.git_dir
                    )

            # Create Change object
            change = Change(
                file_num=file_num,
                change_num=change_num,
                file=file_path,
                function=func_name,
                hunk_header=hunk_header,
                hunk_content=hunk_content,
                total_lines=total_lines,
                diffinfo=diffinfo,
                definition=definition,
            )
            current_group_changes.append(change)
            current_group_lines += total_lines
            change_num += 1

        # Flush remaining changes for this file
        if current_group_changes:
            file_groups.append({
                "file_num": file_num,
                "file": file_path,
                "changes": current_group_changes,
                "total_lines": current_group_lines,
            })
            file_num += 1

    # Post-process: combine small FILE-N groups that together are under MAX_COMBINED_FILE_GROUP_LINES
    # This reduces the number of agents spawned in Phase 2
    if len(file_groups) > 1:
        combined_groups = []
        current_combined = None

        for group in file_groups:
            if current_combined is None:
                # Start a new combined group
                current_combined = {
                    "file_num": None,  # Will be assigned later
                    "files": [group["file"]],
                    "changes": list(group["changes"]),
                    "total_lines": group["total_lines"],
                }
            elif current_combined["total_lines"] + group["total_lines"] <= MAX_COMBINED_FILE_GROUP_LINES:
                # Merge this group into current_combined
                current_combined["files"].append(group["file"])
                current_combined["changes"].extend(group["changes"])
                current_combined["total_lines"] += group["total_lines"]
            else:
                # Current combined group is full, flush it and start new one
                combined_groups.append(current_combined)
                current_combined = {
                    "file_num": None,
                    "files": [group["file"]],
                    "changes": list(group["changes"]),
                    "total_lines": group["total_lines"],
                }

        # Flush last combined group
        if current_combined:
            combined_groups.append(current_combined)

        # Reassign file_num and update changes
        for idx, cgroup in enumerate(combined_groups, start=1):
            cgroup["file_num"] = idx
            # Update file_num and change_num in each Change object
            change_num = 1
            for change in cgroup["changes"]:
                change.file_num = idx
                change.change_num = change_num
                change_num += 1
            # Set "file" to combined representation for single or multiple files
            if len(cgroup["files"]) == 1:
                cgroup["file"] = cgroup["files"][0]
            else:
                cgroup["file"] = cgroup["files"]  # List of files

        file_groups = combined_groups

    # Track count before similarity merging for reporting
    groups_before_similarity = len(file_groups)

    # Post-process: combine FILE-N groups that share significant function overlap
    # This merges groups that work on related code even if they're in different files
    file_groups = combine_groups_by_similarity(file_groups)

    groups_after_similarity = len(file_groups)
    similarity_merges = groups_before_similarity - groups_after_similarity

    # Collect all changes for writing
    all_changes = []
    for group in file_groups:
        all_changes.extend(group["changes"])

    # Write FILE-N-CHANGE-M.json files
    for change in all_changes:
        content = create_change_json(change)
        with open(output_dir / f"{change.id}.json", "w") as f:
            json.dump(content, f, indent=2)

    # Create index.json with file-grouped structure
    index = {
        "version": "2.0",
        "commit": {
            "sha": commit_info["sha"],
            "subject": commit_info["subject"],
            "author": commit_info["author"],
        },
        "files": [
            {
                "file_num": group["file_num"],
                "file": group["file"],  # Can be string or list of strings
                "files": group.get("files", [group["file"]] if isinstance(group["file"], str) else group["file"]),
                "total_lines": group["total_lines"],
                "functions": sorted(extract_group_functions(group)),  # All functions in this group
                "changes": [
                    {
                        "id": c.id,
                        "function": c.function,
                        "file": c.file,  # Individual file for this change
                        "hunk": c.hunk_header,
                    }
                    for c in group["changes"]
                ],
            }
            for group in file_groups
        ],
        "files-modified": files_changed,
        "total-files": len(file_groups),
        "total-changes": len(all_changes),
    }

    with open(output_dir / "index.json", "w") as f:
        json.dump(index, f, indent=2)

    # Print summary
    print(f"CONTEXT ANALYSIS COMPLETE")
    print()
    print(f"Commit: {commit_info['sha'][:12]} {commit_info['subject']}")
    print(f"Author: {commit_info['author']}")
    print()
    if use_semcode:
        print("Using semcode for static analysis")
    else:
        if skip_semcode:
            print("Semcode disabled (--no-semcode), extracting definitions from source")
        else:
            print("Semcode unavailable, extracting definitions from source")
    print()
    print(f"Source files modified: {len(files_changed)}")
    print(f"Hunks in diff: {total_hunks}")
    print(f"FILE-N groups created: {len(file_groups)}")
    if similarity_merges > 0:
        print(f"  - Groups merged by function similarity: {similarity_merges} (from {groups_before_similarity} to {groups_after_similarity})")
    print(f"Total changes: {len(all_changes)}")
    print(f"  - Max lines per FILE-N: {MAX_LINES_PER_FILE}")
    print(f"  - Small file groups combined: max {MAX_COMBINED_FILE_GROUP_LINES} lines")
    print(f"  - Similarity merge: >{int(MIN_FUNCTION_OVERLAP_RATIO*100)}% function overlap, max {MAX_SIMILARITY_COMBINED_LINES} lines")
    print(f"  - Modifications: combined by function, max {MAX_COMBINED_ADDED_LINES} added lines")
    print(f"  - New functions: combined by file, max {MAX_NEW_FUNCTION_COMBINED_LINES} total lines")
    print()
    print("File breakdown:")
    for group in file_groups:
        files_list = group.get("files", [group["file"]] if isinstance(group["file"], str) else group["file"])
        if len(files_list) == 1:
            file_display = os.path.basename(files_list[0])
        else:
            file_display = ", ".join(os.path.basename(f) for f in files_list)
        print(f"- FILE-{group['file_num']}: {file_display} ({group['total_lines']} lines, {len(group['changes'])} changes)")
        for c in group["changes"]:
            print(f"    - {c.id}: {c.function} in {os.path.basename(c.file)}")

    print()
    print(f"Output directory: {output_dir.absolute()}")


if __name__ == "__main__":
    main()
