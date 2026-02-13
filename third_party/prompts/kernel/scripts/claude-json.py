#!/usr/bin/env python3
"""
claude-json: Parse Claude stream-json output and convert to plain text

Based on patterns from:
- claude-code-log (MIT License) - https://github.com/daaain/claude-code-log  
- claude-code-sdk-python (MIT License) - https://github.com/anthropics/claude-code-sdk-python

Usage:
    claude -p "your prompt" --output-format=stream-json | python scripts/claude-json.py
    python scripts/claude-json.py -i input.json -o output.txt
    python scripts/claude-json.py -d < input.json  # debug mode
"""

import json
import sys
import argparse


def extract_text_from_stream(stream, debug=False):
    """Extract plain text from Claude's stream-json format."""
    text_parts = []
    line_count = 0
    parsed_count = 0

    for line in stream:
        line_count += 1
        line = line.strip()

        if not line:
            continue

        if debug:
            print(f"Processing line {line_count}: {line[:100]}...", file=sys.stderr)

        try:
            data = json.loads(line)

            # Extract text from assistant messages
            if (data.get('type') == 'assistant' and
                'message' in data and
                'content' in data['message']):

                for content_item in data['message']['content']:
                    if content_item.get('type') == 'text':
                        text = content_item.get('text', '')
                        if text:
                            parsed_count += 1
                            text_parts.append(text)
                            if debug:
                                print(f"Extracted text {parsed_count}: {len(text)} chars", file=sys.stderr)

            # Handle streaming deltas (if Claude uses them)
            elif data.get('type') == 'content_block_delta':
                delta_text = data.get('delta', {}).get('text', '')
                if delta_text:
                    parsed_count += 1
                    text_parts.append(delta_text)
                    if debug:
                        print(f"Extracted delta {parsed_count}: {len(delta_text)} chars", file=sys.stderr)

        except json.JSONDecodeError as e:
            if debug:
                print(f"JSON decode error on line {line_count}: {e}", file=sys.stderr)
                print(f"Raw line: {line}", file=sys.stderr)
            continue
        except Exception as e:
            if debug:
                print(f"Unexpected error on line {line_count}: {e}", file=sys.stderr)
            continue

    if debug:
        print(f"Processed {line_count} lines, extracted {parsed_count} text parts, total {len(''.join(text_parts))} chars", file=sys.stderr)

    return ''.join(text_parts)


def main():
    parser = argparse.ArgumentParser(
        description='Parse Claude stream-json output and convert to plain text',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__
    )
    
    parser.add_argument('-i', '--input', type=str,
                        help='Input file (default: stdin)')
    parser.add_argument('-o', '--output', type=str,
                        help='Output file (default: stdout)')
    parser.add_argument('-d', '--debug', action='store_true',
                        help='Enable debug output to stderr')
    
    args = parser.parse_args()
    
    # Handle input
    if args.input:
        try:
            with open(args.input, 'r') as f:
                text = extract_text_from_stream(f, debug=args.debug)
        except FileNotFoundError:
            print(f"Error: Input file '{args.input}' not found", file=sys.stderr)
            return 1
        except Exception as e:
            print(f"Error reading input file: {e}", file=sys.stderr)
            return 1
    else:
        text = extract_text_from_stream(sys.stdin, debug=args.debug)
    # Handle output
    print("\n")
    if args.output:
        try:
            with open(args.output, 'w') as f:
                f.write(text)
                f.write('\n')
        except Exception as e:
            print(f"Error writing output file: {e}", file=sys.stderr)
            return 1
    else:
        print(text, end='\n')

    print("\n\n=================\n")
    return 0


if __name__ == '__main__':
    sys.exit(main())
