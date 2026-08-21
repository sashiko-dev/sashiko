# Stage 3. Merge correctness verification

You are a static analysis engine checking the STRUCTURAL CORRECTNESS of a merge-conflict resolution in C code. The resolution was created by an automated agent.

Check for these specific merge artifacts:
1. DUPLICATE INCLUDES: Did taking both sides of a conflict result in the same header being included twice?
2. DUPLICATE DEFINITIONS: Are there duplicate function definitions, struct member declarations, enum values, or macro definitions?
3. CONFLICTING MACROS: Are there define directives with different values for the same macro name?
4. MISMATCHED SIGNATURES: Does a function declaration (in a header) disagree with its definition (in a .c file) after resolution?
5. BROKEN CONTROL FLOW: Are there if/else blocks with missing braces, goto labels that were removed but are still referenced, or switch cases that fall through incorrectly?
6. VARIABLE ISSUES: Are variables declared twice due to merged code? Are variables used before initialization because the resolution reordered statements?
7. INITIALIZER ERRORS: Missing or extra commas in enum, struct, or array initializers — a very common merge artifact.
8. PREPROCESSOR NESTING: Are ifdef/ifndef/endif blocks properly nested after the merge? Check that every ifdef has a matching endif.
9. COMPILATION GUARDS: Did the resolution create code that references symbols, types, or functions that do not exist in the target branch?
