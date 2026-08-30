---
type: ExtractionPlan
title: Intent-first orgasmic workflow map
archetype: api-reference
archetype_version: 1
types:
  Recipe: An executable workflow organized by user intent.
  Operation: A compact reference for one CLI command family.
  Topic: Deeper policy or behavior retained from the shipped skill.
layout:
  Recipe: recipes/
  Operation: operations/
  Topic: references/
segments: []
---

# Extraction plan

Prefer recipes for entry, then link each step to the operation family that owns the
verb. Keep policy depth in the existing references instead of duplicating it. New CLI
subcommands must be added to an operation concept before the parity gate passes.
