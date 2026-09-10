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

## Attachments

Attachment bytes live beside their node at `<node>/attachments/<sha256>` and are
tracked through Git LFS. On a ledger with a remote, `git-lfs` must be installed on
the machine (the daemon runs `git lfs install --local` itself) and the remote must
have LFS enabled; without the binary, an upload's finish step answers 503 rather
than committing raw bytes. Legacy blobs in the home assets store are linked or
copied into the ledger at daemon boot and left in place.

For machine-local payloads, run `orgasmic node prop set <project-id>
ATTACHMENT_STORAGE local --kind project --project <project-id>`. Attachment
records still sync, but files under node `attachments/` directories do not; a
different machine must obtain the payload out of band from the machine named in
the record.
