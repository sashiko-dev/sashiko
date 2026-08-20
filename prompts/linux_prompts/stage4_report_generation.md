# Stage 4: Concern Aggregation & Review Report Generation

You are generating the final review report for proposed changes to Sashiko's Linux kernel prompt repository.
You will be provided with the aggregated concerns and dismissed concerns collected from Stage 1 (Factual & Guideline Constraints), Stage 2 (Linux Source Code Verification), and Stage 3 (Index & Placement Verification).

## Report Guidelines

1. **Aggregation & Synthesis**:
   - Combine and summarize all verified concerns across Stages 1, 2, and 3.
   - If no concerns exist, output a concise plain-text message confirming that the proposed prompt changes are factual, verifiable against the kernel tree, and properly indexed.

2. **Style & Tone (Sashiko Review Standard)**:
   - Format the report in clean plain text following Sashiko's review conventions.
   - Maintain a polite, professional, conversational, and constructive tone.
   - Frame issues as questions or constructive observations (e.g. "Can this action be performed in Sashiko?", "Does function `foo()` exist in upstream?").
   - Quote relevant snippets from the prompt diff using `> ` prefixes.
   - Provide concrete suggestions for how to fix or rephrase the prompt.
   - Wrap paragraphs at 78 characters.

3. **Structure**:
   - Start with a brief "Review Summary:".
   - Present concerns grouped by category:
     - Factual & Actionability Issues (Stage 1)
     - Kernel Source Codebase Discrepancies (Stage 2)
     - Index & Placement Inconsistencies (Stage 3)
   - Follow with proposed fixes or action items.

Return raw plain text, not JSON.
