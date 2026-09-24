#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────────
// EIR E2E FAKE CLAUDE CLI. NEVER contacts a real model.
//
// SAFETY (read this before touching the branches below): eir-svc parses this
// script's stdout as the `claude --print --output-format json` envelope and,
// for analysis/investigation prompts, parses the envelope's `result` string
// AGAIN as a ClaudeDecision. Every branch below that builds a ClaudeDecision
// MUST set "problems": [] — a non-empty problems array is what lets the real
// fix executor act on this machine (service restarts, registry writes, disk
// cleanup, …), and this fixture's only job is to prove the plumbing works
// without ever doing that. If you add a branch, keep problems empty.
// ─────────────────────────────────────────────────────────────────────────
import { readFileSync } from "node:fs";
import { ANALYSIS_MARKER, ASK_MARKER } from "./markers.mjs";

function readStdin() {
  try {
    return readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

// eir-svc/src/ai/prompt.rs: the fixed instruction every analysis/investigation
// prompt ends with, asking for a ClaudeDecision JSON object.
const ANALYSIS_MARKER_TEXT = "Respond ONLY with valid JSON";
// eir-svc/src/ask.rs: the fixed opening line of every Ask/Explain/Investigate-
// answer prompt.
const ASK_MARKER_TEXT =
  "You are Eir, an autonomous Windows guardian, answering the PC owner's question";

function envelope(resultText) {
  process.stdout.write(
    JSON.stringify({
      result: resultText,
      total_cost_usd: 0,
      usage: {
        input_tokens: 1,
        output_tokens: 1,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
      },
    }),
  );
}

const prompt = readStdin();

if (prompt.includes(ANALYSIS_MARKER_TEXT)) {
  const decision = {
    analysis: ANALYSIS_MARKER,
    problems: [], // ALWAYS empty — see the safety note above.
    needs_deeper_analysis: false,
  };
  envelope(JSON.stringify(decision));
} else if (prompt.includes(ASK_MARKER_TEXT)) {
  envelope(ASK_MARKER);
} else {
  // Anything unrecognised (e.g. an app-update check) — plain text, never JSON
  // that could be mistaken for a decision.
  envelope(ASK_MARKER);
}
