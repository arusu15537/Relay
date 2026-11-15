You are reviewing code changes that a smaller local model made to implement a spec. The changes have already been applied to the project.

# Spec that was implemented
{{spec}}

# Applied changes (unified diff)
{{diff}}

{{files_after}}

Check the changes against the spec: correctness, bugs, missed requirements, unhandled edge cases, acceptance criteria, syntax or type errors, deviations from the Design (names, signatures, responsibilities, relationships, contracts), and changes the spec did not ask for. Do not write code in your answer.

Respond in Markdown with exactly these sections:

## Verdict
One line starting with `PASS` or `NEEDS_FIXES`, followed by one sentence.

## Problems
Bullets, one per problem: the file, what is wrong, and why it matters. Write "None." if there are none.

If the verdict is NEEDS_FIXES, end your answer with a line containing exactly

=== FOLLOW-UP SPEC ===

followed by a complete follow-up spec that fixes only the problems, written against the CURRENT code (after the diff above), using exactly these sections: `## Summary`, `## Design` (the elements involved in the fix at class-diagram level: responsibilities, exact signatures, relationships, and the contract each must meet), `## Files` (bullets: `path` (modify|create) — purpose), `## Changes` (one `### `path`` subsection per file, self-contained: which element changes, where, and what it must guarantee afterwards), `## Edge cases`, `## Acceptance criteria`. Describe what must hold, not how: do not write the code, pseudo-code, step-by-step logic, or expressions and call chains in code syntax (inline `code` only for names, signatures, types and literal values). The implementing model only sees that spec and the listed files and writes all of the code itself.
