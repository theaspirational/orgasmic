---
type: PurposeFitness
title: Purpose fitness sample
date: '2026-08-30'
prompt_version: manual-bootstrap@1
selector_version: 2
seed: 2114cdf7a9549850
sampled:
- SKILL
- operations/dispatch
- operations/forum
- operations/runs
- operations/task-graph
- recipes/dispatched-curator
- recipes/inspect-work
- recipes/install-update-runtime
- recipes/judge-document
- recipes/self-curated-forum
- references/agent-selection
- references/asking
- references/dispatch
- references/forum
- references/init
- references/install
- references/ledger
- references/recall-resume
- references/tiers
- references/update
rows:
- concept_id: SKILL
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: SKILL
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: SKILL
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: SKILL
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: operations/dispatch
  check_id: callable-from-concept-alone
  verdict: pass
  evidence: Names every exact command path in the family, gives the shared signature
    and error rule, and links executable recipes where the family participates in
    a workflow.
- concept_id: operations/dispatch
  check_id: recipe-grounded
  verdict: n/a
  evidence: Operation family, not a Recipe.
- concept_id: operations/dispatch
  check_id: family-findable
  verdict: pass
  evidence: Frontmatter aliases and the body inventory name every command path in
    this family.
- concept_id: operations/dispatch
  check_id: contract-scoped
  verdict: n/a
  evidence: Operation family, not a Contract.
- concept_id: operations/forum
  check_id: callable-from-concept-alone
  verdict: pass
  evidence: Names every exact command path in the family, gives the shared signature
    and error rule, and links executable recipes where the family participates in
    a workflow.
- concept_id: operations/forum
  check_id: recipe-grounded
  verdict: n/a
  evidence: Operation family, not a Recipe.
- concept_id: operations/forum
  check_id: family-findable
  verdict: pass
  evidence: Frontmatter aliases and the body inventory name every command path in
    this family.
- concept_id: operations/forum
  check_id: contract-scoped
  verdict: n/a
  evidence: Operation family, not a Contract.
- concept_id: operations/runs
  check_id: callable-from-concept-alone
  verdict: pass
  evidence: Names every exact command path in the family, gives the shared signature
    and error rule, and links executable recipes where the family participates in
    a workflow.
- concept_id: operations/runs
  check_id: recipe-grounded
  verdict: n/a
  evidence: Operation family, not a Recipe.
- concept_id: operations/runs
  check_id: family-findable
  verdict: pass
  evidence: Frontmatter aliases and the body inventory name every command path in
    this family.
- concept_id: operations/runs
  check_id: contract-scoped
  verdict: n/a
  evidence: Operation family, not a Contract.
- concept_id: operations/task-graph
  check_id: callable-from-concept-alone
  verdict: pass
  evidence: Names every exact command path in the family, gives the shared signature
    and error rule, and links executable recipes where the family participates in
    a workflow.
- concept_id: operations/task-graph
  check_id: recipe-grounded
  verdict: n/a
  evidence: Operation family, not a Recipe.
- concept_id: operations/task-graph
  check_id: family-findable
  verdict: pass
  evidence: Frontmatter aliases and the body inventory name every command path in
    this family.
- concept_id: operations/task-graph
  check_id: contract-scoped
  verdict: n/a
  evidence: Operation family, not a Contract.
- concept_id: recipes/dispatched-curator
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Recipe concept; callable-from-concept-alone applies to another archetype
    type.
- concept_id: recipes/dispatched-curator
  check_id: recipe-grounded
  verdict: pass
  evidence: Steps link at least one Operation concept and the Complete example composes
    the named verbs.
- concept_id: recipes/dispatched-curator
  check_id: family-findable
  verdict: n/a
  evidence: Recipe concept; family-findable applies to another archetype type.
- concept_id: recipes/dispatched-curator
  check_id: contract-scoped
  verdict: n/a
  evidence: Recipe concept; contract-scoped applies to another archetype type.
- concept_id: recipes/inspect-work
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Recipe concept; callable-from-concept-alone applies to another archetype
    type.
- concept_id: recipes/inspect-work
  check_id: recipe-grounded
  verdict: pass
  evidence: Steps link at least one Operation concept and the Complete example composes
    the named verbs.
- concept_id: recipes/inspect-work
  check_id: family-findable
  verdict: n/a
  evidence: Recipe concept; family-findable applies to another archetype type.
- concept_id: recipes/inspect-work
  check_id: contract-scoped
  verdict: n/a
  evidence: Recipe concept; contract-scoped applies to another archetype type.
- concept_id: recipes/install-update-runtime
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Recipe concept; callable-from-concept-alone applies to another archetype
    type.
- concept_id: recipes/install-update-runtime
  check_id: recipe-grounded
  verdict: pass
  evidence: Steps link at least one Operation concept and the Complete example composes
    the named verbs.
- concept_id: recipes/install-update-runtime
  check_id: family-findable
  verdict: n/a
  evidence: Recipe concept; family-findable applies to another archetype type.
- concept_id: recipes/install-update-runtime
  check_id: contract-scoped
  verdict: n/a
  evidence: Recipe concept; contract-scoped applies to another archetype type.
- concept_id: recipes/judge-document
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Recipe concept; callable-from-concept-alone applies to another archetype
    type.
- concept_id: recipes/judge-document
  check_id: recipe-grounded
  verdict: pass
  evidence: Steps link at least one Operation concept and the Complete example composes
    the named verbs.
- concept_id: recipes/judge-document
  check_id: family-findable
  verdict: n/a
  evidence: Recipe concept; family-findable applies to another archetype type.
- concept_id: recipes/judge-document
  check_id: contract-scoped
  verdict: n/a
  evidence: Recipe concept; contract-scoped applies to another archetype type.
- concept_id: recipes/self-curated-forum
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Recipe concept; callable-from-concept-alone applies to another archetype
    type.
- concept_id: recipes/self-curated-forum
  check_id: recipe-grounded
  verdict: pass
  evidence: Steps link at least one Operation concept and the Complete example composes
    the named verbs.
- concept_id: recipes/self-curated-forum
  check_id: family-findable
  verdict: n/a
  evidence: Recipe concept; family-findable applies to another archetype type.
- concept_id: recipes/self-curated-forum
  check_id: contract-scoped
  verdict: n/a
  evidence: Recipe concept; contract-scoped applies to another archetype type.
- concept_id: references/agent-selection
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/agent-selection
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/agent-selection
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/agent-selection
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/asking
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/asking
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/asking
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/asking
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/dispatch
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/dispatch
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/dispatch
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/dispatch
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/forum
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/forum
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/forum
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/forum
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/init
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/init
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/init
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/init
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/install
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/install
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/install
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/install
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/ledger
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/ledger
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/ledger
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/ledger
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/recall-resume
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/recall-resume
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/recall-resume
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/recall-resume
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/tiers
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/tiers
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/tiers
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/tiers
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
- concept_id: references/update
  check_id: callable-from-concept-alone
  verdict: n/a
  evidence: Topic concept retaining policy depth; callable-from-concept-alone applies
    to another archetype type.
- concept_id: references/update
  check_id: recipe-grounded
  verdict: n/a
  evidence: Topic concept retaining policy depth; recipe-grounded applies to another
    archetype type.
- concept_id: references/update
  check_id: family-findable
  verdict: n/a
  evidence: Topic concept retaining policy depth; family-findable applies to another
    archetype type.
- concept_id: references/update
  check_id: contract-scoped
  verdict: n/a
  evidence: Topic concept retaining policy depth; contract-scoped applies to another
    archetype type.
---

# Purpose fitness

Model-authored review of the deterministic risk-oriented sample. This is not an owner eval verdict.
