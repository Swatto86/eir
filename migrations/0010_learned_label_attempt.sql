-- Marks when the Tier-2 AI labeller has attempted to explain a learned fact, so a fact
-- whose explanation can't be produced (model returns nothing, or the call errors) is
-- attempted at most ONCE per lifetime rather than re-queried every cycle. A fresh attempt
-- only happens if the fact is forgotten and re-forms (a new row). See ARCHITECTURE.md.
ALTER TABLE learned_facts ADD COLUMN ai_label_attempted_at TEXT;
