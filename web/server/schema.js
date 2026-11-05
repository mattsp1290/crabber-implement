import { z } from "zod";

export const nodeSchema = z.enum(["implement", "review", "fix-review", "done"]);
export const stageSchema = z.object({
  node: nodeSchema,
  occurrence: z.number().int().positive(),
  outcome: z.enum(["started", "implemented", "changes", "approved", "fixed"]),
});
export const metadataSchema = z.object({
  version: z.union([z.literal(1), z.literal(2)]),
  execution_generation: z.number().int().positive().optional(),
  run_id: z.string(),
  repo: z.string(),
  branch: z.string(),
  model: z.string(),
  heartbeat_at_ms: z.number().nonnegative(),
  parent_workflow_id: z.string().uuid().optional(),
  recovery_mode: z.literal("refresh").optional(),
  review_round: z.number().int().min(1).max(100).optional(),
}).superRefine((value, context) => {
  if (Boolean(value.parent_workflow_id) !== Boolean(value.recovery_mode) ||
      (value.recovery_mode && value.review_round === undefined) ||
      value.parent_workflow_id === value.run_id) {
    context.addIssue({ code: "custom", message: "Invalid refresh lineage" });
  }
});
export const resultSchema = z.object({
  execution_generation: z.number().int().positive().optional(),
  run_id: z.string().optional(),
  status: z.enum(["completed", "failed", "cancelled"]),
  error: z.string().nullable().optional(),
});
export const sessionSchema = z.object({
  execution_generation: z.number().int().positive().optional(),
  review_round: z.number().int().positive().nullish(),
  review_role: z.enum(["correctness", "maintainability"]).nullish(),
  review_attempt: z.number().int().positive().nullish(),
  name: z.string(),
  workflow_run_id: z.string().optional(),
  session_id: z.string().optional(),
  run_id: z.string().optional(),
  max_turns: z.number().int().nonnegative().optional(),
  observed_turns: z.number().int().nonnegative().optional(),
  status: z.enum(["running", "completed", "failed"]),
  error: z.string().nullable().optional(),
});
const tailSchema = z.object({ content: z.string(), truncated: z.boolean() });
export const summarySchema = z.object({
  id: z.string(),
  repo: z.string().nullable(),
  branch: z.string().nullable(),
  model: z.string().nullable(),
  status: z.enum(["running", "completed", "failed", "unknown"]),
  currentNode: nodeSchema.nullable(),
  updatedAt: z.number(),
  heartbeatAt: z.number().nullable(),
  resultStatus: z.string().nullable(),
  error: z.string().nullable(),
  parentWorkflowId: z.string().uuid().nullable(),
  recoveryMode: z.literal("refresh").nullable(),
  reviewRound: z.number().int().min(1).max(100).nullable(),
});
export const snapshotSchema = summarySchema.extend({
  stages: z.array(stageSchema),
  sessions: z.array(
    z.object({
      name: z.string(),
      status: z.enum(["running", "completed", "failed", "unknown"]),
      observedTurns: z.number().nullable(),
      reviewRound: z.number().nullable(),
      reviewRole: z.enum(["correctness", "maintainability"]).nullable(),
      reviewAttempt: z.number().nullable(),
      reused: z.boolean(),
      maxTurns: z.number().nullable(),
      text: tailSchema,
      tools: tailSchema,
      error: z.string().nullable(),
    }),
  ),
  plan: tailSchema.nullable(),
  warnings: z.array(z.string()),
});
export const runListSchema = z.object({
  runs: z.array(summarySchema),
  warnings: z.array(z.string()),
});

export const recoverySchema = z.object({
  version: z.union([z.literal(1), z.literal(2)]), workflow_id: z.string(), round: z.number().int().min(1).max(100),
  generation: z.number().int().positive(),
  phase: z.enum(["review_pending", "review_reports_ready", "fix_intent_recorded", "fix_committed", "completed"]),
  attempts: z.tuple([z.number().int().min(0).max(4294967295),z.number().int().min(0).max(4294967295)]),
  reused: z.tuple([z.boolean(),z.boolean()]),
});
