# Plan input

## Sub-features

Load one Markdown file or ordered numerically prefixed Markdown documents in a directory containing `00-overview.md`. Reject empty and blocked plans.

## How to get to it (user POV)

Pass the plan path as the single positional CLI argument. Use `--repo` to choose the repository separately.

## Driving it with the CLI harness

Run `cargo test --locked directory_reads_ordered_documents_and_rejects_blocked`. Run the CLI integration target for actual argument and startup behavior. The live helper passes a committed Markdown file and verifies the program specified by that plan.

## Gotchas

Plan text is instruction to an agent, rather than an executable schema. The CLI does not mechanically certify every acceptance criterion in arbitrary prose. Ordered directories must include `00-overview.md`.
