// Pure constants shared between fake-claude.mjs (which emits them) and the
// specs (which assert on them). No side effects — safe to import from either.
export const ANALYSIS_MARKER =
  "Eir e2e fake analysis: all quiet on this isolated test machine.";
export const ASK_MARKER =
  "Eir e2e fake answer: this response comes from the e2e fake CLI, not a real model.";
