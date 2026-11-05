# Plan input

## Sub-features

Load one Markdown file or ordered numerically prefixed Markdown documents in a directory containing `00-overview.md`. Reject empty and blocked plans. Count every directory entry, including ignored files, up to 4096. Select at most 256 numbered Markdown documents. Include rendered headers and separators in the 2,000,000-byte plan limit. Bound review skill input at 16 MiB. Reject FIFOs and other special files without blocking.

## How to get to it (user POV)

Pass the plan path as the single positional CLI argument. Use `--repo` to choose the repository separately.

## Driving it with the CLI harness

Run `cargo test --locked directory_reads_ordered_documents_and_rejects_blocked`. Run the CLI integration target for actual argument and startup behavior. The live helper passes a committed Markdown file and verifies the program specified by that plan.

## Gotchas

Plan text is instruction to an agent, rather than an executable schema. The CLI does not mechanically certify every acceptance criterion in arbitrary prose. Ordered directories must include `00-overview.md`.

Run `cargo test --locked --test cli input_bounds -- --test-threads=2` for actual normal, resume, and refresh entry points. These subprocesses have no provider credentials. Require failure before provider calls, workflow artifact creation, or branch changes. The fixtures cover sparse oversized files, excess selected and ignored entries, rendered header overflow, and special files.
