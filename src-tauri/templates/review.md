You are reviewing code changes that a smaller local model made to implement a spec. The changes have already been applied to the project.

# Spec that was implemented
{{spec}}

# Applied changes (unified diff)
{{diff}}

{{files_after}}

Check the changes against the spec: correctness, bugs, missed requirements, unhandled edge cases, acceptance criteria, syntax or type errors, and changes the spec did not ask for.

Respond in Markdown with exactly these sections:

## Verdict
One line starting with `PASS` or `NEEDS_FIXES`, followed by one sentence.

## Problems
Bullets, one per problem: the file, what is wrong, and why it matters. Write "None." if there are none.

If the verdict is NEEDS_FIXES, end your answer with a line containing exactly

=== FOLLOW-UP SPEC ===

followed by a complete follow-up spec that fixes only the problems, written against the CURRENT code (after the diff above), using exactly these sections: `## Summary`, `## Files` (bullets: `path` (modify|create) — purpose), `## Changes` (one `### `path`` subsection per file, self-contained, with exact locations, signatures and precise step-by-step descriptions — describe the fix in words, do not write the code), `## Edge cases`, `## Acceptance criteria`. The implementing model only sees that spec and the listed files, so be explicit; it writes all of the code itself.
