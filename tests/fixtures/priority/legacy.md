# Synthetic legacy policy
Prose never executes.
<!-- hidden
## Critical
- label: hidden
-->
## Low
- label: chore
## HIGH ###
* PRIORITY == medium AND label != blocked
- cycle: active and cycle_number > 2
- estimate < 8 and project != Unassigned
## Critical
- title ~ "outage:.*"
- label: model/fable
## Default
- low
- critical
## Scoring
- + 40 if label: customer
- -20 estimate > 8
- +3 if source = github
- +2 if team: ENG
- +1 if description ~ "follow:up"
## Overrides
- PIN-1: low
- pin-1: critical
- PIN-1: +40
- pin-1: -2
- PIN-1: model = opus
- pin-1: model = codex:gpt-6 | fable
- SKIP-1: skip
## Models
- if title ~ a:b: codex:gpt-6 | fable
- if label: model/fable: fable
- if label: model/fable: opus
- normal: opus
- normal: sonnet | sonnet
- critical: fable | codex:gpt-6
## Jev
- enabled: yes
- instructions: Ship this week?
- criteria: later | soon | now
## Unknown
- ignored: yes
