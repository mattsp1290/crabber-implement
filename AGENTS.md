# Agent instructions

## Create verification skills as the implementation develops

The user has authorized both a project-local app verification skill and a formal
verification skill for this repository. Create each when its prerequisites exist,
without asking for approval again. Reassess these triggers when implementing new
behavior. The two skills can become appropriate at different times.

- When the first user-facing behavior runs locally, invoke
  `/create-verification-skill`. Create `.cursor/skills/verify-crabber-implement/`
  with actual launch, drive, evidence, and cleanup commands, plus a feature map.
  Run one mapped journey end to end and confirm its evidence survives cleanup
  before declaring the skill complete.
- When the code contains a consequential property that formal verification can
  meaningfully check, invoke `/create-formal-verification-skill`. Examples include
  state transitions, concurrent operations, retries, or parser invariants. Choose
  the target from the actual code and user impact. Record the covered code, sync
  commit, tool version, bounds, and assumptions. Prove that the check catches a
  deliberately broken variant, restore the code, and rerun the check before
  declaring the skill complete.

Until these prerequisites exist, defer the corresponding skill. Do not invent
commands, model hypothetical code, or add product behavior solely to enable a
verification skill. If implementation work reaches a trigger but creation cannot
finish, report the concrete blocker and the remaining work.

After creation, update the app verification skill and feature map when user-facing
behavior changes. Recheck and update the formal verification when covered code
changes. Use `/maintain-verification-skill` and
`/maintain-formal-verification-skill` for dedicated maintenance passes.
