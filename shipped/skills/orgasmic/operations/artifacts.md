---
type: Operation
title: Artifact, verification, and member commands
description: Replay verification proofs, submit or inspect artifacts, and manage local
  members.
aliases:
- orgasmic verify
- orgasmic artifact
- orgasmic member
- orgasmic artifact blocks
- orgasmic artifact submit
- orgasmic artifact feedback
- orgasmic artifact comments
- orgasmic member add
- orgasmic member revoke
- orgasmic member list
sources:
- cli-help/verify.txt
- cli-help/artifact.txt
- cli-help/member.txt
- cli-help/artifact/blocks.txt
- cli-help/artifact/submit.txt
- cli-help/artifact/feedback.txt
- cli-help/artifact/comments.txt
- cli-help/member/add.txt
- cli-help/member/revoke.txt
- cli-help/member/list.txt
tags:
- cli-reference
---

# Artifact, verification, and member commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic verify`
- `orgasmic artifact`
- `orgasmic member`
- `orgasmic artifact blocks`
- `orgasmic artifact submit`
- `orgasmic artifact feedback`
- `orgasmic artifact comments`
- `orgasmic member add`
- `orgasmic member revoke`
- `orgasmic member list`

## Parameters

Read `orgasmic <command-path> --help` immediately before use. Flags are scoped to
the leaf verb; do not infer a flag from a sibling command.

## Returns

Read-only verbs print text or JSON. Mutating verbs report their identifiers or tx
evidence; dispatch verbs additionally identify the dispatch generation.

## Errors

Unknown verbs and invalid lifecycle transitions are refused by name. Treat a timeout
or a reported worker result as evidence to inspect, not as lifecycle closure.

## Example

```bash
orgasmic member list --help
```
