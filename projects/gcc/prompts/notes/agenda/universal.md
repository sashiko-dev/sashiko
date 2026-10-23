# Universal Software Engineering Principles

This module covers foundational software invariants that apply across all components regardless of language or compiler domain, focusing on loop indexing, variable refactoring, input-proportional scaling, and structured text parsing.

Derived by `sashiko notes agenda` from 200 fixes reachable from 4990533b6a10301ff4681fbcc0bbe7c32ca819fe (115 code bugs, 23 themes).

## INDX: Loop Bounds, Index Tracking, and Scope Integrity

- INDX-1: How are exclusive loop upper bounds adjusted when iterating over a single-element range?
- INDX-2: When populating an array while filtering or skipping initial elements, how are source and destination indices tracked separately?
- INDX-3: What invariants ensure that renamed or refactored variables are updated across all conditional and fallback uses within a scope?
- INDX-4: How must conditional compilation directives be structured so they do not split brace-enclosed control-flow scopes?

## SCAL: Input-Proportional Scaling and Early Bounds Checking

- SCAL-1: What length or magnitude bounds are checked on numeric literals before performing per-digit arbitrary-precision arithmetic?
- SCAL-2: How do file and line-offset caches scale their capacity in proportion to input file size to avoid superlinear lookup times?
- SCAL-3: Where must fixed capacity limits be replaced with input-proportional bounds when buffering or indexing source inputs?

## PARS: Structured Delimiter and Brace Parsing in Scripts

- PARS-1: How do build and text-processing scripts split macro invocations on structural delimiters rather than arbitrary whitespace?
- PARS-2: What mechanism ensures nested braces are balanced when extracting embedded code blocks instead of relying on non-greedy regular expressions?
- PARS-3: How are multi-token arguments or nested expressions preserved during automated source and documentation extraction?
