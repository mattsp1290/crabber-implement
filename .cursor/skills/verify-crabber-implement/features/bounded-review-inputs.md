# Verify bounded review inputs

Use local fixture providers and disposable repositories. These journeys call the actual CLI, DMT graph, Crabber runtime, tools, and Git.

```sh
mkdir -p .audit/bounded-review-inputs
cargo test --locked --test cli bounded_reviews_read_omitted_evidence_across_four_rounds_and_linked_worktrees -- --nocapture > .audit/bounded-review-inputs/bounded-cli.txt 2>&1
cargo test --locked --bin crabber-implement review_artifacts -- --nocapture > .audit/bounded-review-inputs/reader.txt 2>&1
cargo test --locked --bin crabber-implement review_admission_tests -- --nocapture > .audit/bounded-review-inputs/configured-admission.txt 2>&1
cargo test --locked --test admission_contract -- --nocapture > .audit/bounded-review-inputs/admission-contract.txt 2>&1
cargo test --locked --test cli admission -- --nocapture > .audit/bounded-review-inputs/admission-otlp.txt 2>&1
cargo test --locked --bin crabber-implement review_inputs::tests -- --nocapture > .audit/bounded-review-inputs/input-storage.txt 2>&1
cargo test --locked --test cli maximum_skill_reaches -- --nocapture > .audit/bounded-review-inputs/accepted-large-inputs.txt 2>&1
```

Require three fixes followed by review 4 in normal and linked worktrees. Each stage emits more than 1 MB through real tools. Both reviewers use the snapshot catalog and reader to retrieve omitted failure or success evidence, plan text, and committed source text. The fixture chooses its verdict from the retrieved evidence. Inspect measured final prompt bytes and provider-facing tool message bytes. A generic approval fixture does not establish this behavior.

Final prompts must fit 256 KiB, raw excerpts 64 KiB, and encoded reader responses 32 KiB. Snapshot limits are 16 MiB per artifact, 128 MiB total, 4096 entries, and 1 MiB for the manifest. Check `input-metrics.json` against the saved prompt files. Full source logs must remain byte-identical. Catalog pagination and read offsets must preserve all content, including multibyte, control, and escaped text. Binary ranges use hex encoding when needed.

Reader tests reject unknown IDs, paths, invalid ranges, corrupt digests, symlink files, and changed directory ancestry. A sticky integrity failure prevents subsequent approval even after bytes are restored. Configured admission tests include the actual host system prompt and six reviewer tools at the 256 KiB ceiling. A real SDK request for shell remains denied. The standalone admission contract distinguishes typed proof exhaustion from ordinary invalid input and preserves unresolved preparation metadata through close.

The capture tests preserve a large Git diff, binary and deleted files, committed symlink text, an accepted near-2 MB rendered plan, and a 16 MiB skill. An oversized committed blob fails through bounded Git capture before manifest readiness. The CLI large-skill journey reaches fresh review with the full skill retained in its snapshot. A near-2 MB normal implementation prompt can still exhaust SDK admission before review. Configuration acceptance and bounded review capture do not guarantee that implementation admission fits.

The CLI exporter test triggers actual admission exhaustion before any provider request. Require finite typed limit metadata in captured OTLP and no private prompt, path, or credential sentinels. A model's printed verification claim never sets a host success flag.

Fixture temporary repositories and listeners are removed after each test. Exported evidence stays under `.audit/bounded-review-inputs/`. Confirm the saved transcripts and JSON after cleanup. For browser evidence and fresh child provenance, follow [review recovery](review-recovery.md).
