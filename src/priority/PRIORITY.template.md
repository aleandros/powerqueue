# Priority rules

powerqueue re-reads this file whenever it changes. Rules are plain bullet
lines under `##` sections. A ticket's criticality is the first section
(Critical → High → Normal → Low) with a matching rule; unmatched tickets are
`normal`, or whatever `## Default` says.

Conditions: `field: value` (equals, case-insensitive), `field ~ regex`,
`field > n` / `field < n`, `field != value`. Fields: `label`, `priority`
(urgent|high|normal|low|none or 1-4), `estimate`, `project`, `team`,
`title`, `description`, `source` (linear|manual), `key`. Join conditions
with ` and `.

## Critical
- priority: urgent
- label: incident
- label: security

## High
- priority: high
- label: customer

## Normal
- priority: normal

## Low
- label: chore
- label: docs

## Default
- normal

## Scoring
- +40 if label: customer
- +20 if priority: high
- -30 if estimate > 8
- +10 if source: manual

## Overrides
<!-- Pin a ticket: `KEY: critical|high|normal|low`, `KEY: +50`, `KEY: model = opus | gpt-6-astra`, `KEY: skip` -->

## Models
<!-- Preferred models per criticality, most wanted first. Alternatives may belong to
     other providers (enable them in config.toml): `critical: fable | gpt-6.1-sol`.
     Conditional rows pick by label (or any condition) and beat the criticality row;
     the first match wins. Linear child labels are `group/name`:
     - if label: model/fable: fable -->
- critical: fable
- high: opus
- normal: sonnet
- low: sonnet

## Jev
- enabled: false
- question: How important is it to ship this ticket this week for a small product team?
- levels: can wait indefinitely | nice to have | important this week | blocking customers or revenue
