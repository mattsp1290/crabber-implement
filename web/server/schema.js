import { z } from "zod";

export const nodeSchema = z.enum(["implement", "review", "fix-review", "done"]);
export const stageSchema = z.object({
  node: nodeSchema,
  occurrence: z.number().int().positive(),
  outcome: z.enum(["started", "implemented", "changes", "approved", "fixed"]),
});
export const metadataSchema = z.object({
  version: z.literal(1),
  run_id: z.string(),
  repo: z.string(),
  branch: z.string(),
  model: z.string(),
  heartbeat_at_ms: z.number().nonnegative(),
});
export const resultSchema = z.object({
  run_id: z.string().optional(),
  status: z.enum(["completed", "failed", "cancelled"]),
  error: z.string().nullable().optional(),
});
export const sessionSchema = z.object({
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
});
export const snapshotSchema = summarySchema.extend({
  stages: z.array(stageSchema),
  sessions: z.array(
    z.object({
      name: z.string(),
      status: z.enum(["running", "completed", "failed", "unknown"]),
      observedTurns: z.number().nullable(),
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
