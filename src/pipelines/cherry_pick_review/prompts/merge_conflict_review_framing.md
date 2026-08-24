

=== MERGE-CONFLICT RESOLUTION REVIEW ===
An automated agent cherry-picked/forward-ported a patch onto a different branch and resolved the resulting conflicts. THREE commits are involved; only defects INTRODUCED BY THE RESOLUTION are in scope.

1. ORIGINAL PATCH (the change being ported):
   SHA:     {{original_sha}}
   Subject: {{original_subject}}
   ^ Bugs that already existed in this patch are NOT in scope.

2. TARGET BASE (the branch HEAD it was applied ONTO):
   SHA:     {{base_sha}}
   Subject: {{base_subject}}
   ^ Bugs that already existed in the base branch are NOT in scope.

3. RESOLUTION COMMIT (what you are reviewing):
   SHA:     {{resolution_sha}}
   Subject: {{resolution_subject}}
   ^ = (original patch applied) + (conflict resolution). Report ONLY bugs that exist HERE but did NOT exist in the original patch (1) or target base (2).
=========================================
