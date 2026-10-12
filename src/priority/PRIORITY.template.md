# Priority intent

This document records goals, rationale and examples for people and tuning agents.
It is not executable. Scheduling reads only the applied `priority.toml` policy.
Run `powerqueue tune "translate the intent document into policy" --scope priority
--dry-run` to review a validated proposal and simulated queue before applying it.

## Goals and rationale

Restore service and address security incidents promptly. Customer-facing problems
usually matter more than housekeeping. Prefer smaller tasks when their impact is
similar so useful work ships sooner.

## Examples to discuss

An incident blocking customers should run ahead of a documentation cleanup.
A chore can wait unless it unblocks higher-impact work. Model preferences must
still respect provider availability and the machine's budget.

Edit these goals to reflect your project. Prose edits alone never change scheduling.
