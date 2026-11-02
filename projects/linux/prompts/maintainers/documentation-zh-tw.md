+++
title = "Traditional Chinese Documentation Translations"
paths = ["Documentation/translations/zh_TW/"]
+++
# Traditional Chinese Documentation Translations: maintainer notes

Review policy for patches that touch the Traditional Chinese kernel documentation under Documentation/translations/zh_TW/; the English Documentation/ tree remains the authoritative documentation. The rules follow Documentation/translations/zh_TW/glossary.rst, index.rst, and disclaimer-zh_TW.rst.

- Report a translation that changes technical meaning: mistranslated commands, function, type or field names, numeric values, error codes, or code examples make readers act on different behavior than the English document describes.
- Report new or rewritten text that uses a zh_CN or legacy term where glossary.rst specifies a Taiwan term, for example 内核 or 內核 where the glossary requires 核心, 软件 or 軟件 where it requires 軟體, and 硬件 where it requires 硬體 (glossary.rst, terminology tables); report only changed text, not untouched legacy lines.
- Report new or rewritten text that uses non-standard characters instead of the glossary's standard forms: 為 (not 爲), 裡 (not 裏), 著 (not 着), 才 (not 纔), 啟 (not 啓), 只 (not 隻), and 發布 (not 發佈) (glossary.rst, standard character conventions).
- Report new or rewritten paragraphs that insert spaces between Chinese and English words or numbers; zh_TW uses no space, as in Linux核心 (glossary.rst, spacing conventions).
- Preserve whitespace required by inline RST markup such as ``literal`` and :ref: even where the no-space convention applies (glossary.rst, spacing conventions).
- Report new or rewritten lines wider than about 75 half-width characters; this is the zh_TW line-width convention, and it does not require rewrapping untouched legacy lines (glossary.rst, line-width convention).
- For a translated document, report a patch that adds, removes, or changes documentation content only in zh_TW without the matching English change: disclaimer-zh_TW.rst says the translation is not a fork and changes should go to the English original first.
- Report a translated file whose location under Documentation/translations/zh_TW/ does not mirror the English source: tools/docs/checktransupdate.py tracks the English counterpart, so a misplaced file is tracked against the wrong document.
- For a translated document, report a missing or mismatched :Original: field: translated zh_TW files use :Original: Documentation/..., a :ref: with the English label, or a :doc: with a relative path (process/development-process.rst, admin-guide/index.rst, process/coding-style.rst); it must identify the English source and agree with the path mirrored by the translated file. Source documents may omit it (see the source-document rule below).
- For a translated document, report a missing translator attribution: the tree uses :Translator: (process/development-process.rst) and :譯者: (process/coding-style.rst); at least one form with a reachable name is required, and a separate proofreader field is not required. :作者: is not a translator field; it is used for original authors or the glossary author (admin-guide/bootconfig.rst, glossary.rst).
- Do not require :Original:, translator fields, or an English counterpart on source documents in the zh_TW tree, such as glossary.rst and the top-level index.rst; review them for terminology, format, and references only.
- Report a missing or broken disclaimer include where the translated file uses one: subdirectory files include ../disclaimer-zh_TW.rst (process/development-process.rst) or a deeper relative path such as ../../../disclaimer-zh_TW.rst (admin-guide/mm/damon/index.rst), and the relative path must resolve from the file's location; top-level files that do not use the include are not defective.
- Report a translated file that duplicates an English or zh_CN RST label: zh_TW uses tw_-prefixed labels such as .. _tw_process_index: (process/index.rst) and .. _tw_glossary: (glossary.rst), so translated labels stay distinct in the combined build.
- Report a :ref: or :doc: target that does not resolve; stale references break the build.
- Report a toctree entry renamed from the English file name; toctree entries keep the English file names (index.rst, toctree sections).
- Report a translated directory whose parent index does not link the new page in the same series: an unlinked page is unreachable in the built documentation; a valid single-file translation is not a defect.
- Report build-breaking RST visible in the patch, such as an unresolved reference, a duplicate label, a malformed directive, or a broken include.
- Do not report that cleandocs/htmldocs was not run or that Documentation/output was not inspected; those are contributor workflow steps the review cannot observe.
- For a translated document that syncs an upstream English change, report a missing or malformed update to commit <12-hex> ("<subject>") reference (the form used by zh_TW commits; Documentation/doc-guide/checktransupdate.rst).
- Report a translation update that references a merge commit instead of the last non-merge commit that changed the English file.
- Do not require the update reference on an initial translation or a standalone fix with no corresponding English commit.
- Report a translation commit without Signed-off-by: the kernel DCO requires it.
- Report checkpatch-visible issues in the patch, such as whitespace errors or a malformed commit message.
- Do not report that scripts/checkpatch.pl was not run; its output is not part of the review input.
- Do not report that the patch was not sent through git send-email or to scripts/get_maintainer.pl recipients, including linux-doc; mail delivery is not observable in the review input.
- Do not report mailing-list concurrency; it is contributor workflow, not patch content.
- Do not report pure stylistic wording preferences that preserve meaning; this suppression does not cover glossary terminology, standard character forms, line-width, or spacing deviations, which the rules above require reporting.
- Do not ask to rewrite untouched legacy text; the glossary's spacing rule explicitly applies to new and rewritten paragraphs (glossary.rst, spacing conventions).
- Do not report a translation-only patch for missing code, tests, or benchmark evidence: the review is about translation content, not code-test coverage.
