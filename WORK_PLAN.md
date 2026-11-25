# Work Plan

Backlog snapshot from current GitHub label state, maintained by the Guide role. Direction lives in the hand-written roadmap: [docs/ROADMAP.md](docs/ROADMAP.md).

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. The `_Generated:` stamp at the top of the generated region dates the snapshot; `git log -1 -- WORK_PLAN.md` finds the merge that wrote it.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
_Generated: 2026-11-24T18:21:06Z (snapshot; compare against live labels)_
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#11305**: Retire $LOOM_HYPERPARAMS vector tier (#11107)
- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills
- **#14046**: refactor: remove the loom-api crate from the workspace
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository
- **#14411**: fix: carry exhausted weekly-hold row forward over a header-less 2xx probe
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw
- **#14510**: fix: select hook-ready Codex seats and honor shared account disables
- **#14596**: feat: dispatch Claude steps into persistent account seats
- **#14696**: fix(dispatch): admit on the containment proof the walk already took
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge
- **#14753**: fix: mint a bootstrap reader so first-start fleet-config render can confirm the store
- **#14758**: fix(release): keep checksum reads on the reader route
- **#14769**: fix: skip source-remote lookup on no-op resyncs
- **#14864**: docs(judge,doctor): name the dirty-worktree disposition in drift recovery
- **#14866**: fix: verify Forge head identity before PR worktree selection
- **#14897**: fix: keep unpushed issue branch tips during safe cleanup
- **#14916**: fix: enumerate cleanup branch candidates by full ref under residue refs
- **#14937**: fix: classify workflow secret use structurally in Champion WF_BLAST
- **#14950**: refactor: parse workflow-lint YAML with serde-saphyr, drop serde_yaml_ng
- **#14996**: fix: rehome open-PR heads in checkout_ff and block PR-head checkouts in the primary-checkout guard
- **#15055**: fix: read private-workspace branch names via full refs under PR-fetch residue
- **#15060**: feat: add agy tier-3 runtime keyed from the gemini API-key pool
- **#15066**: feat: reap agent per-ticket scratch roots on the worktree volume
- **#15071**: feat: relay private-seat preparation Git reads through the fleet gateway
- **#15079**: build(deps): bump the github-actions group across 1 directory with 5 updates
- **#15084**: fix(claim-reconcile): keep a trusted hand-applied changes-requested pinned at an unmoved head
- **#15101**: fix: refuse writer seat jobs before lease when no writer is published
- **#15118**: fix(review-gate): GraphQL fallback for the comment-verdict read (#15025)
- **#15120**: feat: publish and renew the Builder lease through the Gitea API on a Gitea checkout
- **#15127**: test: fail the fallback-docker CLI test on skip under a CI opt-in; document LOOM_DOCKER_BIN
- **#15128**: ci: run discovery-stall diagnostic after the run step, gate on either failure
- **#15129**: ci: pin actions/checkout to v7.0.1 SHA in merge-group-check workflow and template
- **#15133**: feat: read Builder claim and split-parent evidence from Gitea
- **#15169**: fix(guard): resolve rm-scope relative targets against an earlier cd (#15112)
- **#15216**: fix(verdict): verify the full label transition and keep Judge off contradictory label sets
- **#15223**: feat(judge,champion): gate docs-only fast path on the WORK_PLAN plan-body machine receipt
- **#15224**: fix: scan rm targets behind function-definition and group openers in rm-scope guard

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(held)*
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(held)*
- **#14851**: Investigate Codex Champion merge steps that exit before recording a verdict

## Ready

Human-approved issues (`loom:issue`) ready for implementation — excluding held (`loom:blocked`/`loom:operator`/`loom:frozen`), `loom:building`, and issues with an open linked PR (native admission `OPEN_PR`; listed under Issues with Open PRs).

- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools
- **#14851**: Investigate Codex Champion merge steps that exit before recording a verdict

## Issues with Open PRs (Not Dispatchable)

`loom:issue` issues an open linked PR already holds — native admission would refuse them (`OPEN_PR`); listed with the PR and its stage, not counted above.

- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) — open PR #11305 (held)
- **#13820**: Remove the loom-api crate: reads a database the daemon does not write, 3 of 5 endpoints query tables nothing creates — open PR #14046 (held)
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills — open PR #13997 (held)
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs — open PR #14260 (held)
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1 — open PR #14452 (held)
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435) — open PR #14470 (held)
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time — open PR #14498 (held)
- **#14474**: Expose worktree operations through the shared Rust CLI — open PR #14514 (unlabeled)
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold — open PR #14510 (held)
- **#14626**: Private-clone admission contains the winning seat twice per admission — open PR #14696 (held)
- **#14712**: resync: a no-op resync warns that it stamped the canonical loom_source_remote, but leaves the stale one — open PR #14769 (held)
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue — open PR #14715 (held)
- **#14730**: First daemon start on a new host cannot fetch the fleet store: fleet-config render is shed when readers are configured and no daemon is running — open PR #14753 (held)
- **#14739**: forge_identity: a release.checksum refusal withdraws the reader gateway for the whole owner, shedding every 2AMLogic read ~20 min/h since 0.20.0 — open PR #14758 (held)
- **#14876**: clean: stale-branch pass deletes a mis-prefixed 'heads/<name>' branch and fails permanently on PR-fetch ref residue — open PR #14916 (held)
- **#14946**: Remove serde_yaml_ng (and unsafe-libyaml): consolidate workflow-lint onto serde-saphyr — open PR #14950 (held)
- **#15095**: merge-group-check.yml is the repo's only unpinned action (actions/checkout@v7), and its defaults/optional template still says @v4 — open PR #15129 (held)

## In Progress

Issues currently being built (`loom:building`).

- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145)
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder
- **#14560**: Work-finder completion gaps leave the Fleet queue stale
- **#14656**: judge.md/doctor.md drift recovery: dirty-worktree arm refuses with no named next step (and no longer surfaces the snapshot path)
- **#14657**: Check the PR head repository before selecting Judge and Doctor worktrees
- **#14879**: clean --safe: closed-issue branch cleanup force-deletes unpushed commits via the -d to -D fallback
- **#14905**: Retire duplicate post-merge callers after the daemon overlap release
- **#14915**: Champion WF_BLAST: 'secrets.' pattern matches file names in path filters and step names
- **#14935**: Fetch private-seat clones through the configured fleet Git read relay
- **#14936**: Diagnose writer renewal and refuse private-seat write jobs before leasing when credentials are missing
- **#14938**: Gitea gap: forge check-claim and forge parent split-state are GitHub-only, blocking targeted Builder steps
- **#14941**: Lease publish in worktree.sh is gh-backed: on Gitea the lease stays local-only and cross-host protection is lost
- **#14945**: runtime: add Google Antigravity CLI (agy), keyed from the gemini api-keys pool
- **#14952**: Per-ticket agent scratch roots on /Volumes/Stripe are never cleaned up (~300 new dirs/day, volume fills)
- **#14973**: Review sessions switch the primary checkout to a PR head; checkout_ff logs wrong-branch and never repairs it (30 checkouts on four hosts)
- **#15025**: check-review-feedback.sh §1b: the comment-verdict read (issues/<n>/comments) is still single-path REST, so a reader shed still hard-stalls Judge approvals
- **#15027**: private_workspace: unpublished_branches/recover enumerate with %(refname:short) and break under PR-fetch ref residue
- **#15033**: Remove 7 superseded pub functions (and GraphqlReading) in loom-daemon kept alive only by self-tests
- **#15068**: An operator's hand-posted changes-requested verdict carries no verdict-sha marker, so stale-verdict reconciliation clears it and re-queues the rejected head
- **#15086**: Make #14885's fallback-docker CLI test prove it ran, and document LOOM_DOCKER_BIN as a fleet-wide fail-closed override
- **#15091**: Codex: an account's weekly exhaustion is learned per host, so every newly seated host burns attempts on it
- **#15094**: test flake: nextest discovery stalls inside the Run tests step (lib binary --list) on Rust Unit Tests (2/5) — fourth occurrence, #14066's diagnostic skipped
- **#15112**: rm-scope guard resolves a relative rm target against the repo root, ignoring a cd earlier in the same command
- **#15185**: post-verdict approve left loom:review-requested on the PR, re-dispatching Judge for 7h (#15155)
- **#15204**: serial_test key mismatch on LOOM_OPENCODE_DB: default-keyed vs opencode_db_env-keyed writers race (pre-existing flake, 3 tests on main)
- **#15220**: Guide docs tick hand-writes the WORK_PLAN plan-body region and reports prose in the receipt line when update_work_plan() is never called (#14204 follow-up)
- **#15221**: rm-scope guard: an rm on an unresolved variable is admitted when it is the only statement in a function body (the same rm at top level is denied)
- **#15240**: Pin the missing-ancestor 0700 guarantee of private_directory_with with a test
- **#15242**: split-state gate (#13635) refuses a split parent whose body was revised post-split to carry residue work

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12969**: feat: age-bound Codex rollout history in live profiles *(held)*
- **#14612**: fix: launch eager reclaim detached so it cannot hold back a work-finder tick *(held)*
- **#15047**: refactor: drop superseded zero-caller pub helpers from loom-daemon *(held)*
- **#15247**: test: pin private_directory_with ancestor 0700 creation (#15240)
- **#15248**: feat(parent-link): honor <!-- loom:split-rescoped --> in split-state (#15242)
- **#15249**: refactor(merge-pr): retire post-merge callers the daemon now owns (#14905)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#11305**: Retire $LOOM_HYPERPARAMS vector tier (#11107) — closes #11107
- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills — closes #13957
- **#14046**: refactor: remove the loom-api crate from the workspace — closes #13820
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one — closes #13933
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository — closes #14110
- **#14411**: fix: carry exhausted weekly-hold row forward over a header-less 2xx probe — closes #14195
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file — closes #14434
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner — closes #14450
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw — closes #14455
- **#14510**: fix: select hook-ready Codex seats and honor shared account disables — closes #14497
- **#14596**: feat: dispatch Claude steps into persistent account seats
- **#14696**: fix(dispatch): admit on the containment proof the walk already took — closes #14626
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge — closes #14714
- **#14753**: fix: mint a bootstrap reader so first-start fleet-config render can confirm the store — closes #14730
- **#14758**: fix(release): keep checksum reads on the reader route
- **#14769**: fix: skip source-remote lookup on no-op resyncs — closes #14712
- **#14864**: docs(judge,doctor): name the dirty-worktree disposition in drift recovery — closes #14656
- **#14866**: fix: verify Forge head identity before PR worktree selection — closes #14657
- **#14897**: fix: keep unpushed issue branch tips during safe cleanup — closes #14879
- **#14916**: fix: enumerate cleanup branch candidates by full ref under residue refs — closes #14876
- **#14937**: fix: classify workflow secret use structurally in Champion WF_BLAST — closes #14915
- **#14950**: refactor: parse workflow-lint YAML with serde-saphyr, drop serde_yaml_ng — closes #14946
- **#14996**: fix: rehome open-PR heads in checkout_ff and block PR-head checkouts in the primary-checkout guard — closes #14973
- **#15055**: fix: read private-workspace branch names via full refs under PR-fetch residue — closes #15027
- **#15060**: feat: add agy tier-3 runtime keyed from the gemini API-key pool — closes #14945
- **#15066**: feat: reap agent per-ticket scratch roots on the worktree volume — closes #14952
- **#15071**: feat: relay private-seat preparation Git reads through the fleet gateway — closes #14935
- **#15079**: build(deps): bump the github-actions group across 1 directory with 5 updates
- **#15084**: fix(claim-reconcile): keep a trusted hand-applied changes-requested pinned at an unmoved head — closes #15068
- **#15101**: fix: refuse writer seat jobs before lease when no writer is published — closes #14936
- **#15118**: fix(review-gate): GraphQL fallback for the comment-verdict read (#15025) — closes #15025
- **#15120**: feat: publish and renew the Builder lease through the Gitea API on a Gitea checkout — closes #14941
- **#15127**: test: fail the fallback-docker CLI test on skip under a CI opt-in; document LOOM_DOCKER_BIN — closes #15086
- **#15128**: ci: run discovery-stall diagnostic after the run step, gate on either failure — closes #15094
- **#15129**: ci: pin actions/checkout to v7.0.1 SHA in merge-group-check workflow and template — closes #15095
- **#15133**: feat: read Builder claim and split-parent evidence from Gitea — closes #14938
- **#15169**: fix(guard): resolve rm-scope relative targets against an earlier cd (#15112) — closes #15112
- **#15216**: fix(verdict): verify the full label transition and keep Judge off contradictory label sets — closes #15185
- **#15223**: feat(judge,champion): gate docs-only fast path on the WORK_PLAN plan-body machine receipt — closes #15220
- **#15224**: fix: scan rm targets behind function-definition and group openers in rm-scope guard — closes #15221

## Proposed

Issues carrying `loom:curated`.

- **#8191**: Port merge-pr.sh to a daemon subcommand (1,458 lines; 48 fixes in 6 months, and the ratchet now blocks fixing it) *(curated)* *(held)*
- **#8525**: tracing: instrument sweep phases and role attempts, including Pi/OpenCode and repair cycles *(curated)* *(held)*
- **#8698**: Live end-to-end verification of the credential egress proxy (AC5 of #8674) *(curated)* *(held)*
- **#9136**: main ruleset: bypass_actors 'RepositoryRole:always' makes pull_request and required_status_checks advisory *(curated)* *(held)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(curated)* *(held)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(curated)* *(held)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version *(curated)*
- **#10838**: merge_pr_reconcile_differential: claim-gate differential disagrees on case-variant duplicate label (loom:buildinG) *(curated)* *(held)*
- **#10941**: No-PR retry hold fires when the only remaining work is a live check or a cross-repo PR *(curated)* *(held)*
- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default) *(curated)*
- **#11112**: Remove the safehouse (Matrix) integration from Loom: narration, ChatOps/Concierge, peer-claim channel (forge leases remain) *(curated)*
- **#11283**: OpenCode 2.x guarded launches: load the native-tools plugin via the 2.x default-export shape, live-verify, flip guard_verified *(curated)* *(held)*
- **#11284**: Runtime readiness: per-repo, per-stage weighted model selection in fleet-gitops *(curated)* *(held)*
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(curated)* *(held)*
- **#11300**: Z.ai seat telemetry via the egress proxy in transparent observe mode: per-request tokens/latency/429s to SigNoz, OpenCode headers untouched *(curated)* *(held)*
- **#11595**: Pause-and-roll budget path can still kill an in-flight merge-pr.sh (follow-up to #11263) *(curated)* *(held)*
- **#11703**: Config precedence: fleet-gitops values must override repo-committed .loom/config.json (general rule + enforcement) *(curated)* *(held)*
- **#11911**: Finish retiring daemon-spawned /loom:sweep and merge the two concurrency budgets *(curated)* *(held)*
- **#12029**: Telemetry: merge runs record PR number and observed result (merge-pr chokepoint) — Judge parts moved to #12362; held behind #12063 *(curated)* *(held)*
- **#12063**: Umbrella: refactor merge-pr to a minimal contract (deletion-first slices; merge queue after the 2AMLogic move) *(curated)*
- **#12094**: Retire the telemetry replay subsystem and the kinds left without a producer by #11301 *(curated)* *(held)*
- **#12248**: Retired hold labels: remove core registry/work_finder readers (1/3 of #11843) *(curated)* *(held)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths *(curated)*
- **#12320**: Cleanup once every worker reads the worklist: delete the per-call `forge_mirror` discovery path, `maxStalenessSecs` and `per_call`; docs and ADR-0024; direct backend preserved *(curated)* *(held)*
- **#12408**: Live-verify epic child discovery against sky130-sar-adc#23 (follow-up to #12388) *(curated)* *(held)*
- **#12418**: Verify epic #10698 acceptance: completed-vs-not-planned dispositions and live fleet receipts *(curated)* *(held)*
- **#12626**: docs: /repo:release Phase 1.5 CHANGELOG completeness gate has no seam for generated-notes policy *(curated)* *(held)*
- **#12703**: Add a per-repo setting to turn off Champion's out-of-band acceptance-criteria hold (#6883) *(curated)* *(held)*
- **#12713**: Part-of merges re-queue the issue to Builders: landed-route marker is read as a Curator 'kept' decision *(curated)* *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(curated)* *(held)*
- **#12966**: Bound Codex session/thread history inside live profiles (the real codex-profiles growth) + uv cache *(curated)* *(held)*
- **#12978**: Codex shared-seat dispatch: live receipts and startup measurements (split from #10796) *(curated)* *(held)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)* *(held)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)* *(held)*
- **#13486**: Champion resync waiver: a byte-identical test under .loom/scripts/tests/ is not a merge-gate path — every release shipping test-champion-resync-waiver.sh holds all resync PRs *(curated)* *(held)*
- **#13521**: Builder: ~11% of fleet sweeps still end without an outcome line after #13447 (worker-3 29%, worker-4 0%) *(curated)* *(held)*
- **#13590**: Observe convert_awaiting_revision_parks on a live Guide tick (#10936 follow-up) *(curated)* *(held)*
- **#13640**: ci_telemetry: owner-kind probe users/2amlogic hangs 120s every cycle in the daemon (0.3s by hand) and a timeout never fails over — 2AMLogic CI dropped 20+ days *(curated)* *(held)*
- **#13668**: Replay strict resync waiver on recent fasterhenry resync PRs (follow-up to #13665) *(curated)* *(held)*
- **#13720**: worktree.issue_state_rest reads spike every 4 h (synchronized cache expiry?) *(curated)* *(held)*
- **#13865**: [ci-daily] Supply Chain *(curated)* *(held)*
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one *(curated)*
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills *(curated)*
- **#13966**: Z.ai weekly-limit 429 is bad-marked for 60 s: exhausted keys are relaunched all day and the role never falls through to Claude *(curated)* *(held)*
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs *(curated)*
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll *(curated)* *(held)*
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145) *(curated)*
- **#14218**: Verify the repaired publish paths actually fire after the org transfer (post-merge live check for #14158) *(curated)*
- **#14232**: Spike: does regenerating CODEX_HOME/config.toml with inline hooks per launch avoid the private-clone hook-trust prompt? *(curated)* *(held)*
- **#14235**: Design: let a roleDraw attempt table escalate on named outcome reasons, once #14233 has data *(curated)* *(held)*
- **#14292**: test flake: test-docs-guide-lock.sh Test 4 (#5573 race) fails on Shell Test Suites (hermetic, 2/2) — two concurrent acquires both win *(curated)* *(held)*
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container *(curated)*
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder *(curated)*
- **#14433**: accounts session recover --publish cannot authenticate: the seat's credential helper has no token outside a job *(curated)* *(held)*
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1 *(curated)*
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435) *(curated)*
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time *(curated)*
- **#14461**: Serve the remaining forge actions through the shared loom-tools dispatcher *(curated)* *(held)*
- **#14464**: Expose park records and operator decisions through loom-tools *(curated)* *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools *(curated)*
- **#14474**: Expose worktree operations through the shared Rust CLI *(curated)*
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold *(curated)*
- **#14530**: Live-verify the admitted loom-tools forge writes against a controlled repository *(curated)* *(held)*
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433) *(curated)* *(held)*
- **#14553**: Live-verify the migrated merge-queue actions in tools-only session mode (#14520 AC3 remainder) *(curated)* *(held)*
- **#14560**: Work-finder completion gaps leave the Fleet queue stale *(curated)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(curated)* *(held)*
- **#14583**: Keep quiet dispatch disposition rows inside the five-minute Fleet freshness target *(curated)* *(held)*
- **#14613**: CI: backend path filter omits the role-prompt trees, so prompt-only PRs skip the 14 Rust tests that lint them *(curated)* *(held)*
- **#14626**: Private-clone admission contains the winning seat twice per admission *(curated)*
- **#14634**: Dashboard Builder repeatedly selects OpenCode and exits at preflight on loom-worker-3 *(curated)* *(held)*
- **#14656**: judge.md/doctor.md drift recovery: dirty-worktree arm refuses with no named next step (and no longer surfaces the snapshot path) *(curated)*
- **#14657**: Check the PR head repository before selecting Judge and Doctor worktrees *(curated)*
- **#14665**: Live verification: post-fix Codex usage-missing breakdown and the 17 unaccounted runs (#14664) *(curated)* *(held)*
- **#14685**: Live-verify Docker retention per-evaluation summaries on a containerd-store worker (#14679 follow-up) *(curated)* *(held)*
- **#14712**: resync: a no-op resync warns that it stamped the canonical loom_source_remote, but leaves the stale one *(curated)*
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue *(curated)*
- **#14730**: First daemon start on a new host cannot fetch the fleet store: fleet-config render is shed when readers are configured and no daemon is running *(curated)*
- **#14737**: Observed Loom step attempt spans omit the recorded failure class *(curated)* *(held)*
- **#14739**: forge_identity: a release.checksum refusal withdraws the reader gateway for the whole owner, shedding every 2AMLogic read ~20 min/h since 0.20.0 *(curated)*
- **#14740**: Fresh install writes no requires_daemon, so newly admitted repos are dispatch-held when the fleet floor moves *(curated)* *(held)*
- **#14741**: pricing: gpt-6.2-astra is not on the pricing card; every Codex step is costed at the Sonnet fallback *(curated)* *(held)*
- **#14765**: Live verification: session_reconcile creates an App-only private-clone seat after #14744 *(curated)* *(held)*
- **#14792**: Finished Builder lease cleanup can shed its repository lookup before yielding *(curated)* *(held)*
- **#14808**: A PR that says "Part of #<operator-held tracker>" inherits the hold and is never drawn by Judge or Doctor, with no visible reason (three chip PRs stalled 6 to 27 days; a fourth held with no visible reason) *(curated)* *(held)*
- **#14810**: A parked PR whose blocker cleared but which now conflicts with main has no route back: the unblock check reports it Held and Doctor's hold exclusion hides it *(curated)* *(held)*
- **#14811**: Roll requeue of a Curator step uses the Builder restore: adds loom:issue, leaves loom:curating, and the issue is never drawn again *(curated)* *(held)*
- **#14851**: Investigate Codex Champion merge steps that exit before recording a verdict *(curated)*
- **#14859**: Live verification: stale loom:curating recovery after the #14816 reconciler rolls out (gf180-pll#753, klayout-tools#6731/#6845, fleet count) *(curated)* *(held)*
- **#14876**: clean: stale-branch pass deletes a mis-prefixed 'heads/<name>' branch and fails permanently on PR-fetch ref residue *(curated)*
- **#14879**: clean --safe: closed-issue branch cleanup force-deletes unpushed commits via the -d to -D fallback *(curated)*
- **#14884**: Session-only mode: publish the gateway relay profiles so attended hosts keep Worker access *(curated)* *(held)*
- **#14905**: Retire duplicate post-merge callers after the daemon overlap release *(curated)*
- **#14915**: Champion WF_BLAST: 'secrets.' pattern matches file names in path filters and step names *(curated)*
- **#14935**: Fetch private-seat clones through the configured fleet Git read relay *(curated)*
- **#14936**: Diagnose writer renewal and refuse private-seat write jobs before leasing when credentials are missing *(curated)*
- **#14938**: Gitea gap: forge check-claim and forge parent split-state are GitHub-only, blocking targeted Builder steps *(curated)*
- **#14941**: Lease publish in worktree.sh is gh-backed: on Gitea the lease stays local-only and cross-host protection is lost *(curated)*
- **#14945**: runtime: add Google Antigravity CLI (agy), keyed from the gemini api-keys pool *(curated)*
- **#14947**: Live baseline: record one real off-tick seat-reclaim walk (per-seat elapsed and outcome) *(curated)* *(held)*
- **#14952**: Per-ticket agent scratch roots on /Volumes/Stripe are never cleaned up (~300 new dirs/day, volume fills) *(curated)*
- **#14973**: Review sessions switch the primary checkout to a PR head; checkout_ff logs wrong-branch and never repairs it (30 checkouts on four hosts) *(curated)*
- **#15025**: check-review-feedback.sh §1b: the comment-verdict read (issues/<n>/comments) is still single-path REST, so a reader shed still hard-stalls Judge approvals *(curated)*
- **#15027**: private_workspace: unpublished_branches/recover enumerate with %(refname:short) and break under PR-fetch ref residue *(curated)*
- **#15054**: Live verification: private-seat /tmp stays flat with caches on /workspace/cache (AC from #14929) *(curated)* *(held)*
- **#15068**: An operator's hand-posted changes-requested verdict carries no verdict-sha marker, so stale-verdict reconciliation clears it and re-queues the rejected head *(curated)*
- **#15070**: Gitea qualification: record the numbered Judge/Doctor gate outcomes (#14939 AC4) *(curated)* *(held)*
- **#15086**: Make #14885's fallback-docker CLI test prove it ran, and document LOOM_DOCKER_BIN as a fleet-wide fail-closed override *(curated)*
- **#15091**: Codex: an account's weekly exhaustion is learned per host, so every newly seated host burns attempts on it *(curated)*
- **#15094**: test flake: nextest discovery stalls inside the Run tests step (lib binary --list) on Rust Unit Tests (2/5) — fourth occurrence, #14066's diagnostic skipped *(curated)*
- **#15095**: merge-group-check.yml is the repo's only unpinned action (actions/checkout@v7), and its defaults/optional template still says @v4 *(curated)*
- **#15105**: lease pr check answers PR_LEASE_STATE=unknown for every PR/role on a host, silently blocking every Judge approval *(curated)* *(held)*
- **#15112**: rm-scope guard resolves a relative rm target against the repo root, ignoring a cd earlier in the same command *(curated)*
- **#15160**: Gitea claim probe reads issue comments as a single unpaginated page *(curated)* *(held)*
- **#15161**: Gitea reads use curl's %header{} write-out, which needs curl >= 7.84 *(curated)* *(held)*
- **#15162**: Gitea claim probe: open-PR leg drops GitHub's feature/issue-N head widening, and walks every pull *(curated)* *(held)*
- **#15185**: post-verdict approve left loom:review-requested on the PR, re-dispatching Judge for 7h (#15155) *(curated)*
- **#15204**: serial_test key mismatch on LOOM_OPENCODE_DB: default-keyed vs opencode_db_env-keyed writers race (pre-existing flake, 3 tests on main) *(curated)*
- **#15220**: Guide docs tick hand-writes the WORK_PLAN plan-body region and reports prose in the receipt line when update_work_plan() is never called (#14204 follow-up) *(curated)*
- **#15221**: rm-scope guard: an rm on an unresolved variable is admitted when it is the only statement in a function body (the same rm at top level is denied) *(curated)*
- **#15240**: Pin the missing-ancestor 0700 guarantee of private_directory_with with a test *(curated)*
- **#15242**: split-state gate (#13635) refuses a split parent whose body was revised post-split to carry residue work *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)* *(held)*
- **#14186**: Decide whether to retire the sweep warning for out-of-set dependencies *(hermit)*
- **#14647**: Remove unused Radix dropdown and switch dependencies from both UI quickstarts *(hermit)*
- **#14846**: Exercise the installer’s retired-file cleanup directly in Section 5b *(hermit)*
- **#14986**: Remove remaining single-workspace cadence loops (spawn_supervisor_thread and spawn_token_ranking_refresh_task) *(hermit)*
- **#15212**: Remove superseded parse_queue_ref and members_for from group_authz.rs *(hermit)* *(held)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management *(held)*
- **#10332**: Epic: cut GitHub API usage ~10x — per-consumer quotas, fix chatty loops, then push-not-poll via a loom-ui read mirror (baseline 105k gh calls/day) *(held)*
- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#10698**: Fleet version floor: the daemon enforces `loom_min_version` from the fleet store, rolls itself forward, and brings each repo's installed Loom up to its own version
- **#11287**: Umbrella: Z.ai GLM-5.3-Flash seats landing code in the fleet via OpenCode *(held)*
- **#12304**: Epic: fleet discovery from one mirror read per tick — zero forge reads for discovery; delete per-host listing paths
- **#12708**: Epic: loom-tools CLI for agents, shared Rust implementation with the daemon

## Backlog Balance

| Bucket | Count |
|------|-------|
| Operator merge-risk holds | 39 |
| Very important | 0 |
| Important | 6 |
| Ready (`loom:issue`, net of holds/building/open-linked-PRs) | 2 |
| In Progress (`loom:building`) | 31 |
| Held, no other bucket | 81 |
| PRs awaiting review | 6 |
| Approved PRs awaiting merge | 40 |
| Curated | 122 |
| Architect / Hermit proposals | 6 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
