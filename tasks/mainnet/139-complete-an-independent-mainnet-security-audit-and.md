---
id: "139"
title: "Complete an independent mainnet security audit and launch bug bounty"
status: cancelled
priority: critical
effort: large
dependencies: ["134", "135", "137"]
tags: ["mainnet", "security", "audit", "release"]
created_at: 2026-08-14
parent: 053
type: chore
cancelled_at: 2026-08-17
---

# Complete an independent mainnet security audit and launch bug bounty

## Objective

Have reviewers who did not build nano attack the frozen mainnet follower,
consensus assumptions and operational evidence before public launch, then keep a
funded disclosure path open after launch.

## Tasks

- [ ] Freeze an audit candidate and publish its threat model, trust boundaries,
      Epoch 4.0 profile, checkpoint ceremony, dependency/SBOM data and existing
      qualification evidence.
- [ ] Commission independent review of Bitcoin-derived sortition/fork choice,
      signer authentication, clarity-wasm compiler/host ABI, state persistence
      and recovery, checkpoint trust, P2P/RPC resource safety and release supply
      chain.
- [ ] Require adversarial code review and executable reproductions rather than a
      document-only architecture assessment.
- [ ] Record every finding as a task with severity, owner, disclosure state and
      retest evidence. Critical and high findings block the release umbrella.
- [ ] Rerun the complete mandatory corpus and affected qualification evidence
      after every audit fix; any source change invalidates the frozen candidate.
- [ ] Publish the audit report or a complete public summary with justified,
      time-bounded redactions.
- [ ] Launch a funded bug bounty with scope, safe harbor, severity/payment rules,
      encrypted contact, response targets and an incident/disclosure process.

## Acceptance Criteria

- Independent reviewers cover every named trust boundary and can build and test
  the exact candidate without private instructions.
- No unresolved critical or high finding remains; accepted lower findings have
  an owner, rationale and expiry.
- Every fixed finding has independent retest evidence and a permanent regression.
- The public bounty is funded and operational before the mainnet-ready label is
  applied.
- The post-audit artifact is rebuilt, frozen and requalified from the beginning.
