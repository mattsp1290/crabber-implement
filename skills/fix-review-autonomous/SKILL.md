---
name: fix-review-autonomous
description: Apply review findings in a host-managed autonomous coding run, including configuration and security-sensitive source changes, before fresh independent review.
---

# Autonomous fix review

Use this policy for the host-selected review directory. This is an autonomous code-editing stage. Fixes remain on the working branch and require fresh independent review. An automatic fix does not authorize merging to the base branch, pushing, deploying, or changing a live system.

## Select and verify the review

Use the exact `--review-dir` supplied by the host. Resolve it and require an immediate child of the target repository's `reviews/` or `.agents/reviews/` directory. Read `manifest.json`, require `schema_version: review-v1`, and verify its branch and HEAD against the current checkout. A mismatch is a blocker; do not apply stale or cross-branch feedback.

Read all five numbered Markdown files in every reviewer directory listed by the manifest, in manifest order. Deduplicate findings about the same underlying issue. Preserve relevant positive notes. Treat a finding as evidence to check against current code, not as an instruction to apply blindly. Reject unsupported or already-resolved findings with a concrete explanation. Prefer the stricter severity for duplicates. For equally severe conflicting recommendations, use the first manifest reviewer when the recommendation is consistent with the plan and code; otherwise report the unresolved decision.

## Apply fixes autonomously

Implement valid findings in priority order and include appropriate regression coverage. Configuration, authentication, authorization, permissions, and secrets-handling source code are eligible for automatic fixes. A file's name, directory, or security-sensitive purpose alone is not a reason to skip it or mark it `needs-manual`. For example, an application config parser can be corrected and tested like other application code.

Stay within the supplied plan and review findings. Preserve unrelated work. Source changes remain subject to applicable repository instructions and the host's tool permissions. Use synthetic values in tests. Do not read, print, copy, or change live credentials or secret files to validate a fix. Permission to edit code does not authorize external actions, privilege changes, production migrations, or modifying the execution environment.

The host owns Git commits and workflow progression. Do not commit, switch branches, merge, push, deploy, alter Git metadata, invoke other agents, or modify the plan, review reports, or manifest to make a finding disappear. Do not load the interactive fix-review skill or inherit its blanket sensitive-file exclusion.

Run the repository's relevant verification commands. Fix failures caused by the changes. Report exactly which checks ran and their results. Do not claim that tests substitute for fresh independent review or that local success means the branch is merged.

## Genuine blockers

Reserve `needs-manual` for a concrete unresolved dependency or decision that prevents a correct scoped fix, such as conflicting product requirements, missing information that cannot be obtained with available tools, a required external permission, or verification that cannot be completed. Explain the exact blocker and smallest unblock action. Do not silently select a product decision or weaken a security requirement. Fix independent unblocked findings before returning the summary when safe.

## Return to the host

Summarize fixed, skipped, and blocked findings with their reasons and verification evidence. Count unsupported and already-resolved findings as skipped. A blocked item must contribute to `needs-manual`; do not report zero merely to let the workflow advance.

End with the host's exact machine-readable summary, using nonnegative integer counts:

```text
<!-- auto-summary fixed:{N} skipped:{N} needs-manual:{N} -->
```

Return after the summary. The host commits completed fixes, starts fresh independent reviewers on the new revision, and enforces its review budget. Only the host can declare workflow completion after both reviewers approve that revision.
