# Work Plan

Backlog snapshot from current GitHub label state, maintained by the Guide role. Direction lives in the hand-written roadmap: [docs/ROADMAP.md](docs/ROADMAP.md).

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. The `_Generated:` stamp at the top of the generated region dates the snapshot; `git log -1 -- WORK_PLAN.md` finds the merge that wrote it.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
_Generated: 2026-11-28T06:21:38Z (snapshot; compare against live labels)_
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — a human merge decision, or the prerequisite shown, is missing.

- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills *(held: champion merge-risk hold)*
- **#14046**: refactor: remove the loom-api crate from the workspace *(held: champion critical-file hold)*
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one *(held: champion merge-risk hold)*
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder *(held: champion merge-risk hold)*
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw *(held: champion merge-risk hold)*
- **#14696**: fix(dispatch): admit on the containment proof the walk already took *(held: champion merge-risk hold)*
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge *(held: champion merge-risk hold)*
- **#14916**: fix: enumerate cleanup branch candidates by full ref under residue refs *(held: champion merge-risk hold)*
- **#15066**: feat: reap agent per-ticket scratch roots on the worktree volume *(held: champion merge-risk hold)*
- **#15071**: feat: relay private-seat preparation Git reads through the fleet gateway *(held: champion merge-risk hold)*
- **#15099**: feat: share codex account holds across fleet hosts on the heartbeat *(held: champion merge-risk hold)*
- **#15101**: fix: refuse writer seat jobs before lease when no writer is published *(held: champion merge-risk hold)*
- **#15133**: feat: read Builder claim and split-parent evidence from Gitea *(held: champion merge-risk hold)*
- **#15224**: fix: scan rm targets behind function-definition and group openers in rm-scope guard *(held: Local worktree pr-15224 holding feature/issue-15221 is dirty at b680d5547 behind PR head ccbf98811; preserved untouched per dirty-worktree refusal policy)*
- **#15307**: refactor(prompts): load-gate the docs-only fast path bodies (#15236) *(held: champion merge-risk hold)*
- **#15334**: feat(telemetry): Codex seat on runtime spans and seat refusals (#15291) *(held: champion merge-risk hold)*
- **#15390**: fix: guard quoted targets, gh flag prefixes, bare git checkout - *(held: champion merge-risk hold)*
- **#15411**: Gate private-clone sealed admission on the session image's own capabilities *(held: champion merge-risk hold)*
- **#15429**: fix: recognize explicit merge-ratification denials and deferrals *(held: champion merge-risk hold)*
- **#15434**: Share the live-sweep and checkpoint checks for issue and step logs *(held: champion merge-risk hold)*
- **#15451**: fix: hold records that require both reviewer keys before their PR merges *(held: champion merge-risk hold)*
- **#15459**: fix: make the installer's same-version gate report held-back paths *(held: champion merge-risk hold)*
- **#15480**: fix: prune stale manifest entries for commands, agents and skills *(held: champion merge-risk hold)*
- **#15485**: fix: sweep abandoned agy per-launch HOMEs at the next launch *(held: champion critical-file hold)*
- **#15489**: feat: move private session seats onto the daemon release image *(held: champion merge-risk hold)*
- **#15494**: fix: make resync --dry-run name the prune for a file it would remove *(held: champion merge-risk hold)*
- **#15497**: fix: scope primary-checkout mutation guard to the agent's workspace *(held: champion merge-risk hold)*
- **#15501**: fix: allow quoted documentation in the primary-checkout guard *(held: champion merge-risk hold)*
- **#15526**: fix: catch primary-checkout mutations inside command substitutions *(held: champion merge-risk hold)*
- **#15533**: fix: raise REQUIRES_DAEMON to 0.20.34 and hard-floor claude-seat *(held: champion merge-risk hold)*
- **#15556**: fix: enforce v0.20.41 daemon floor for agy mandatory pool *(held: champion merge-risk hold)*
- **#15564**: fix: merge-redirect lexer catches assembled and piped merge commands (#15525) *(held: champion merge-risk hold)*
- **#15577**: fix: spool --list output of the lib test binary when stdout is a pipe *(held: champion merge-risk hold)*
- **#15584**: refactor: add proc_exec::run_bounded_to_file for combined-log runners *(held: champion merge-risk hold)*
- **#15589**: fix(worktree-root): propagate config lookup failure status *(held: champion merge-risk hold)*
- **#15616**: refactor: share PR-diff file-list transport across Champion carve-out verbs *(held: champion merge-risk hold)*
- **#15619**: refactor: route remaining output_with_timeout callers onto proc_exec::run_bounded *(held: champion merge-risk hold)*
- **#15660**: fix: keep worktree.sh's guards in force on a tools-only install *(held: champion merge-risk hold)*

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(held)*
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures *(held)*
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools *(held)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(held)*
- **#15308**: main CI red since 11:10Z: install-compat (session-exec claude-seat vs requires_daemon 0.20.5) and E0308 in daemon tests
- **#15553**: test flake: nextest discovery stalls inside the Run tests step (lib binary --list) on Rust Unit Tests (5/5) — sixth occurrence, first after #15378

## Ready

Human-approved issues (`loom:issue`) ready for implementation — excluding held (`loom:blocked`/`loom:operator`/`loom:frozen`), `loom:building`, and issues with an open linked PR (native admission `OPEN_PR`; listed under Issues with Open PRs).

_None._

## Issues with Open PRs (Not Dispatchable)

`loom:issue` issues an open linked PR already holds — native admission would refuse them (`OPEN_PR`); listed with the PR and its stage, not counted above.

- **#13820**: Remove the loom-api crate: reads a database the daemon does not write, 3 of 5 endpoints query tables nothing creates — open PR #14046 (held)
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills — open PR #13997 (held)
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time — open PR #14498 (held)
- **#14626**: Private-clone admission contains the winning seat twice per admission — open PR #14696 (held)
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue — open PR #14715 (held)
- **#14876**: clean: stale-branch pass deletes a mis-prefixed 'heads/<name>' branch and fails permanently on PR-fetch ref residue — open PR #14916 (held)

## In Progress

Issues currently being built (`loom:building`).

- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder
- **#14460**: held_back residue: install-loom.sh's "already installed" gate still reads the stamp alone, and three install_compat doc comments are stale
- **#14476**: Verify the installed create, enter and remove workflow without a daemon
- **#14560**: Work-finder completion gaps leave the Fleet queue stale
- **#14935**: Fetch private-seat clones through the configured fleet Git read relay
- **#14936**: Diagnose writer renewal and refuse private-seat write jobs before leasing when credentials are missing
- **#14938**: Gitea gap: forge check-claim and forge parent split-state are GitHub-only, blocking targeted Builder steps
- **#14952**: Per-ticket agent scratch roots on /Volumes/Stripe are never cleaned up (~300 new dirs/day, volume fills)
- **#15003**: Primary-checkout mutation guard: quoted pathspec denied, gh global flags bypass, bare `-` unguarded
- **#15033**: Remove 7 superseded pub functions (and GraphqlReading) in loom-daemon kept alive only by self-tests
- **#15091**: Codex: an account's weekly exhaustion is learned per host, so every newly seated host burns attempts on it
- **#15109**: agy adapter: the per-launch isolated HOME is never reclaimed, and three claims about the pilot are stale
- **#15221**: rm-scope guard: an rm on an unresolved variable is admitted when it is the only statement in a function body (the same rm at top level is denied)
- **#15236**: Load-gate the docs-only fast-path block out of judge.md and champion-pr-merge.md
- **#15291**: Telemetry: Codex runs and refusals carry no seat, and seat state (held / container missing / quota) is log-only
- **#15308**: main CI red since 11:10Z: install-compat (session-exec claude-seat vs requires_daemon 0.20.5) and E0308 in daemon tests
- **#15356**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered)
- **#15377**: Ratification detector holds a record that says merging does NOT ratify it (STATUS + CLAUSE ignore negation)
- **#15397**: Codex private-clone steps keep drawing seats with no recorded hook trust ('sealed'); 21% of Codex runs fail in seconds
- **#15417**: guard: quoted $(...)-tailed /tmp redirect target is resolved as a cwd-relative token and denied
- **#15421**: Share the live-sweep and checkpoint checks for issue and step logs
- **#15430**: Ratification hold and reviewer dispatch miss a proposed record requiring both keys before merge
- **#15475**: resync: manifest prune (#14012) skips surfaces outside .loom/, so a retired file under .claude/commands/loom leaves a stale installed_files entry
- **#15478**: resync: --dry-run does not preview the manifest prune for a retired file the same run would remove
- **#15483**: guard: primary-checkout-mutation denies git commit in an independent disposable repository during an agent step (any standalone repository reads as a primary checkout)
- **#15487**: Primary-checkout guard scans heredoc bodies as commands (#7923's inert-heredoc fix never reached it)
- **#15505**: Primary-checkout guard: every command-substitution form bypasses it (pre-existing, unrelated to #15487)
- **#15525**: Merge redirect misses executing commands with the daemon installed; run the full workflow-guard suite in native CI
- **#15532**: agy pilot: an installed loom-daemon older than v0.20.41 skips the manifest's mandatory pool, so an ambient GEMINI_API_KEY reaches the child
- **#15553**: test flake: nextest discovery stalls inside the Run tests step (lib binary --list) on Rust Unit Tests (5/5) — sixth occurrence, first after #15378
- **#15581**: Add proc_exec::run_bounded_to_file and consolidate the 4 combined-log temp-file try_wait runners
- **#15583**: Consolidate duplicate gh argv and gh api parsers in gh_invocation onto route::split_with and api_request
- **#15588**: Make pr-worktree.sh stop when worktree root config resolution fails
- **#15605**: Consolidate duplicated PR file-list pagination and head-content plumbing across the four forge PR-diff verbs
- **#15617**: Retire sweep_registry::reaper::output_with_timeout and route its 13 remaining callers onto proc_exec::run_bounded
- **#15652**: Retire unreferenced SweepRegistry::spawn_step / begin_step_spawn and wire check_role_command into spawn_step_unlocked_timed
- **#15714**: resync: attended merge-pr.sh skips post-merge reconciliation; manifest keeps retired command files
- **#15718**: Consumer installs keep rjwalters/loom in install-time files: resync-installed.sh never refreshes .loom/README.md or the dated guides, and nothing migrates or flags .loom/config
- **#15729**: checkout_ff: emit a record per fast-forward it performs, so a managed checkout's mover is attributable from the store
- **#15730**: Consolidate triplicated recursive directory-sync loops in init/file_ops.rs
- **#15732**: test: 4 forge_gateway tests fail under umask 0002 because the fixture's gh-ok marker is written group-writable
- **#15741**: Restore launcher wiring for project hooks during resync and install

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12969**: feat: age-bound Codex rollout history in live profiles *(held)*
- **#14612**: fix: launch eager reclaim detached so it cannot hold back a work-finder tick *(held)*
- **#15047**: refactor: drop superseded zero-caller pub helpers from loom-daemon *(held)*
- **#15679**: fix: run role-command check on live step spawn; retire locked spawn *(held)*
- **#15742**: test: pin gh-ok marker mode to 0600 in forge_gateway fixtures

## Approved (Awaiting Merge)

PRs that passed review (`loom:pr`); unmarked PRs are queued for Champion auto-merge, PRs marked `*(held …)*` are not (the hold decides).

- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills — closes #13957 *(held: champion merge-risk hold)*
- **#14046**: refactor: remove the loom-api crate from the workspace — closes #13820 *(held: champion critical-file hold)*
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one — closes #13933 *(held: champion merge-risk hold)*
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder — closes #14376 *(held: champion merge-risk hold)*
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw — closes #14455 *(held: champion merge-risk hold)*
- **#14696**: fix(dispatch): admit on the containment proof the walk already took — closes #14626 *(held: champion merge-risk hold)*
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge — closes #14714 *(held: champion merge-risk hold)*
- **#14916**: fix: enumerate cleanup branch candidates by full ref under residue refs — closes #14876 *(held: champion merge-risk hold)*
- **#15066**: feat: reap agent per-ticket scratch roots on the worktree volume — closes #14952 *(held: champion merge-risk hold)*
- **#15071**: feat: relay private-seat preparation Git reads through the fleet gateway — closes #14935 *(held: champion merge-risk hold)*
- **#15099**: feat: share codex account holds across fleet hosts on the heartbeat — closes #15091 *(held: champion merge-risk hold)*
- **#15101**: fix: refuse writer seat jobs before lease when no writer is published — closes #14936 *(held: champion merge-risk hold)*
- **#15133**: feat: read Builder claim and split-parent evidence from Gitea — closes #14938 *(held: champion merge-risk hold)*
- **#15224**: fix: scan rm targets behind function-definition and group openers in rm-scope guard — closes #15221 *(held: Local worktree pr-15224 holding feature/issue-15221 is dirty at b680d5547 behind PR head ccbf98811; preserved untouched per dirty-worktree refusal policy)*
- **#15307**: refactor(prompts): load-gate the docs-only fast path bodies (#15236) — closes #15236 *(held: champion merge-risk hold)*
- **#15334**: feat(telemetry): Codex seat on runtime spans and seat refusals (#15291) — closes #15291 *(held: champion merge-risk hold)*
- **#15390**: fix: guard quoted targets, gh flag prefixes, bare git checkout - — closes #15003 *(held: champion merge-risk hold)*
- **#15411**: Gate private-clone sealed admission on the session image's own capabilities — closes #15397 *(held: champion merge-risk hold)*
- **#15429**: fix: recognize explicit merge-ratification denials and deferrals — closes #15377 *(held: champion merge-risk hold)*
- **#15434**: Share the live-sweep and checkpoint checks for issue and step logs — closes #15421 *(held: champion merge-risk hold)*
- **#15451**: fix: hold records that require both reviewer keys before their PR merges — closes #15430 *(held: champion merge-risk hold)*
- **#15459**: fix: make the installer's same-version gate report held-back paths — closes #14460 *(held: champion merge-risk hold)*
- **#15480**: fix: prune stale manifest entries for commands, agents and skills — closes #15475 *(held: champion merge-risk hold)*
- **#15485**: fix: sweep abandoned agy per-launch HOMEs at the next launch — closes #15109 *(held: champion critical-file hold)*
- **#15489**: feat: move private session seats onto the daemon release image — closes #14112 *(held: champion merge-risk hold)*
- **#15494**: fix: make resync --dry-run name the prune for a file it would remove — closes #15478 *(held: champion merge-risk hold)*
- **#15497**: fix: scope primary-checkout mutation guard to the agent's workspace — closes #15483 *(held: champion merge-risk hold)*
- **#15501**: fix: allow quoted documentation in the primary-checkout guard — closes #15487 *(held: champion merge-risk hold)*
- **#15526**: fix: catch primary-checkout mutations inside command substitutions — closes #15505 *(held: champion merge-risk hold)*
- **#15533**: fix: raise REQUIRES_DAEMON to 0.20.34 and hard-floor claude-seat — closes #15308 *(held: champion merge-risk hold)*
- **#15556**: fix: enforce v0.20.41 daemon floor for agy mandatory pool — closes #15532 *(held: champion merge-risk hold)*
- **#15564**: fix: merge-redirect lexer catches assembled and piped merge commands (#15525) — closes #15525 *(held: champion merge-risk hold)*
- **#15577**: fix: spool --list output of the lib test binary when stdout is a pipe — closes #15553 *(held: champion merge-risk hold)*
- **#15584**: refactor: add proc_exec::run_bounded_to_file for combined-log runners — closes #15581 *(held: champion merge-risk hold)*
- **#15589**: fix(worktree-root): propagate config lookup failure status — closes #15588 *(held: champion merge-risk hold)*
- **#15616**: refactor: share PR-diff file-list transport across Champion carve-out verbs — closes #15605 *(held: champion merge-risk hold)*
- **#15619**: refactor: route remaining output_with_timeout callers onto proc_exec::run_bounded — closes #15617 *(held: champion merge-risk hold)*
- **#15660**: fix: keep worktree.sh's guards in force on a tools-only install — closes #14476 *(held: champion merge-risk hold)*
- **#15740**: refactor: consolidate init directory-sync loops into one helper — closes #15730

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
- **#12978**: Measure production Codex step startup, admission refusals, fallback and seat selection *(curated)* *(held)*
- **#13317**: Audit Champion merge-risk self-releases two weeks after #12960 is live *(curated)* *(held)*
- **#13378**: Scope trust for github-actions[bot] to ci-daily tracker authorship instead of repo-wide trustedCommenters (alternative to #13340) *(curated)* *(held)*
- **#13486**: Champion resync waiver: a byte-identical test under .loom/scripts/tests/ is not a merge-gate path — every release shipping test-champion-resync-waiver.sh holds all resync PRs *(curated)* *(held)*
- **#13521**: Builder: ~11% of fleet sweeps still end without an outcome line after #13447 (worker-3 29%, worker-4 0%) *(curated)* *(held)*
- **#13590**: Observe convert_awaiting_revision_parks on a live Guide tick (#10936 follow-up) *(curated)* *(held)*
- **#13640**: ci_telemetry: owner-kind probe users/2amlogic hangs 120s every cycle in the daemon (0.3s by hand) and a timeout never fails over — 2AMLogic CI dropped 20+ days *(curated)* *(held)*
- **#13668**: Replay strict resync waiver on recent fasterhenry resync PRs (follow-up to #13665) *(curated)* *(held)*
- **#13720**: worktree.issue_state_rest reads spike every 4 h (synchronized cache expiry?) *(curated)* *(held)*
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one *(curated)*
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills *(curated)*
- **#13966**: Z.ai weekly-limit 429 is bad-marked for 60 s: exhausted keys are relaunched all day and the role never falls through to Claude *(curated)* *(held)*
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll *(curated)*
- **#14218**: Verify the repaired publish paths actually fire after the org transfer (post-merge live check for #14158) *(curated)*
- **#14232**: Spike: does regenerating CODEX_HOME/config.toml with inline hooks per launch avoid the private-clone hook-trust prompt? *(curated)* *(held)*
- **#14292**: test flake: test-docs-guide-lock.sh Test 4 (#5573 race) fails on Shell Test Suites (hermetic, 2/2) — two concurrent acquires both win *(curated)* *(held)*
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container *(curated)* *(held)*
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder *(curated)*
- **#14433**: accounts session recover --publish cannot authenticate: the seat's credential helper has no token outside a job *(curated)* *(held)*
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time *(curated)*
- **#14460**: held_back residue: install-loom.sh's "already installed" gate still reads the stamp alone, and three install_compat doc comments are stale *(curated)*
- **#14461**: Serve the remaining forge actions through the shared loom-tools dispatcher *(curated)* *(held)*
- **#14464**: Expose park records and operator decisions through loom-tools *(curated)* *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools *(curated)* *(held)*
- **#14530**: Live-verify the admitted loom-tools forge writes against a controlled repository *(curated)* *(held)*
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433) *(curated)* *(held)*
- **#14553**: Live-verify the migrated merge-queue actions in tools-only session mode (#14520 AC3 remainder) *(curated)* *(held)*
- **#14560**: Work-finder completion gaps leave the Fleet queue stale *(curated)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(curated)* *(held)*
- **#14583**: Keep quiet dispatch disposition rows inside the five-minute Fleet freshness target *(curated)* *(held)*
- **#14626**: Private-clone admission contains the winning seat twice per admission *(curated)*
- **#14665**: Live verification: post-fix Codex usage-missing breakdown and the 17 unaccounted runs (#14664) *(curated)* *(held)*
- **#14685**: Live-verify Docker retention per-evaluation summaries on a containerd-store worker (#14679 follow-up) *(curated)* *(held)*
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue *(curated)*
- **#14741**: pricing: gpt-6.2-astra is not on the pricing card; every Codex step is costed at the Sonnet fallback *(curated)* *(held)*
- **#14765**: Live verification: session_reconcile creates an App-only private-clone seat after #14744 *(curated)* *(held)*
- **#14808**: A PR that says "Part of #<operator-held tracker>" inherits the hold and is never drawn by Judge or Doctor, with no visible reason (three chip PRs stalled 6 to 27 days; a fourth held with no visible reason) *(curated)* *(held)*
- **#14810**: A parked PR whose blocker cleared but which now conflicts with main has no route back: the unblock check reports it Held and Doctor's hold exclusion hides it *(curated)* *(held)*
- **#14811**: Roll requeue of a Curator step uses the Builder restore: adds loom:issue, leaves loom:curating, and the issue is never drawn again *(curated)* *(held)*
- **#14859**: Live verification: stale loom:curating recovery after the #14816 reconciler rolls out (gf180-pll#753, klayout-tools#6731/#6845, fleet count) *(curated)* *(held)*
- **#14876**: clean: stale-branch pass deletes a mis-prefixed 'heads/<name>' branch and fails permanently on PR-fetch ref residue *(curated)*
- **#14884**: Session-only mode: publish the gateway relay profiles so attended hosts keep Worker access *(curated)* *(held)*
- **#14935**: Fetch private-seat clones through the configured fleet Git read relay *(curated)*
- **#14936**: Diagnose writer renewal and refuse private-seat write jobs before leasing when credentials are missing *(curated)*
- **#14938**: Gitea gap: forge check-claim and forge parent split-state are GitHub-only, blocking targeted Builder steps *(curated)*
- **#14945**: runtime: add Google Antigravity CLI (agy), keyed from the gemini api-keys pool *(curated)* *(held)*
- **#14947**: Live baseline: record one real off-tick seat-reclaim walk (per-seat elapsed and outcome) *(curated)* *(held)*
- **#14952**: Per-ticket agent scratch roots on /Volumes/Stripe are never cleaned up (~300 new dirs/day, volume fills) *(curated)*
- **#15003**: Primary-checkout mutation guard: quoted pathspec denied, gh global flags bypass, bare `-` unguarded *(curated)*
- **#15054**: Live verification: private-seat /tmp stays flat with caches on /workspace/cache (AC from #14929) *(curated)* *(held)*
- **#15059**: agy: live verification of the pinned Antigravity CLI (version, read-only policy, update control) *(curated)* *(held)*
- **#15070**: Gitea qualification: record the numbered Judge/Doctor gate outcomes (#14939 AC4) *(curated)* *(held)*
- **#15091**: Codex: an account's weekly exhaustion is learned per host, so every newly seated host burns attempts on it *(curated)*
- **#15105**: lease pr check answers PR_LEASE_STATE=unknown for every PR/role on a host, silently blocking every Judge approval *(curated)* *(held)*
- **#15109**: agy adapter: the per-launch isolated HOME is never reclaimed, and three claims about the pilot are stale *(curated)*
- **#15160**: Gitea claim probe reads issue comments as a single unpaginated page *(curated)* *(held)*
- **#15161**: Gitea reads use curl's %header{} write-out, which needs curl >= 7.84 *(curated)* *(held)*
- **#15162**: Gitea claim probe: open-PR leg drops GitHub's feature/issue-N head widening, and walks every pull *(curated)* *(held)*
- **#15221**: rm-scope guard: an rm on an unresolved variable is admitted when it is the only statement in a function body (the same rm at top level is denied) *(curated)*
- **#15236**: Load-gate the docs-only fast-path block out of judge.md and champion-pr-merge.md *(curated)*
- **#15291**: Telemetry: Codex runs and refusals carry no seat, and seat state (held / container missing / quota) is log-only *(curated)*
- **#15308**: main CI red since 11:10Z: install-compat (session-exec claude-seat vs requires_daemon 0.20.5) and E0308 in daemon tests *(curated)*
- **#15356**: [ci-daily] Session and Private-Workspace Docker Fixtures (unfiltered) *(curated)*
- **#15377**: Ratification detector holds a record that says merging does NOT ratify it (STATUS + CLAUSE ignore negation) *(curated)*
- **#15397**: Codex private-clone steps keep drawing seats with no recorded hook trust ('sealed'); 21% of Codex runs fail in seconds *(curated)*
- **#15417**: guard: quoted $(...)-tailed /tmp redirect target is resolved as a cwd-relative token and denied *(curated)*
- **#15418**: Telemetry: report the seat/account for Claude and Z.ai runs too (provider-neutral, extends #15291) *(curated)* *(held)*
- **#15430**: Ratification hold and reviewer dispatch miss a proposed record requiring both keys before merge *(curated)*
- **#15474**: agy adapter: default the self-update kill switch and pin an explicit permission mode (live findings #15059) *(curated)* *(held)*
- **#15475**: resync: manifest prune (#14012) skips surfaces outside .loom/, so a retired file under .claude/commands/loom leaves a stale installed_files entry *(curated)*
- **#15478**: resync: --dry-run does not preview the manifest prune for a retired file the same run would remove *(curated)*
- **#15483**: guard: primary-checkout-mutation denies git commit in an independent disposable repository during an agent step (any standalone repository reads as a primary checkout) *(curated)*
- **#15487**: Primary-checkout guard scans heredoc bodies as commands (#7923's inert-heredoc fix never reached it) *(curated)*
- **#15493**: test-spawn-agy.sh resolves the shipped agy manifest one directory too high, so the fixture copy never runs *(curated)* *(held)*
- **#15504**: Primary-checkout guard: inert-documentation allowlist misses the sanctioned issue-filing and verdict-posting forms (follow-up to #15487) *(curated)* *(held)*
- **#15505**: Primary-checkout guard: every command-substitution form bypasses it (pre-existing, unrelated to #15487) *(curated)*
- **#15506**: docs(guard-hooks): finish the Known-consequence update for agent-step markers *(curated)* *(held)*
- **#15525**: Merge redirect misses executing commands with the daemon installed; run the full workflow-guard suite in native CI *(curated)*
- **#15532**: agy pilot: an installed loom-daemon older than v0.20.41 skips the manifest's mandatory pool, so an ambient GEMINI_API_KEY reaches the child *(curated)*
- **#15553**: test flake: nextest discovery stalls inside the Run tests step (lib binary --list) on Rust Unit Tests (5/5) — sixth occurrence, first after #15378 *(curated)*
- **#15588**: Make pr-worktree.sh stop when worktree root config resolution fails *(curated)*
- **#15605**: Consolidate duplicated PR file-list pagination and head-content plumbing across the four forge PR-diff verbs *(curated)*
- **#15692**: Triage remaining hand-rolled Command::new("git") sites in test code after the test_git_helpers consolidation *(curated)* *(held)*
- **#15713**: resync: prune_stale_manifest_entries() cannot prune retired .claude/commands/loom/* entries (#14012 gap) *(curated)* *(held)*
- **#15714**: resync: attended merge-pr.sh skips post-merge reconciliation; manifest keeps retired command files *(curated)*
- **#15718**: Consumer installs keep rjwalters/loom in install-time files: resync-installed.sh never refreshes .loom/README.md or the dated guides, and nothing migrates or flags .loom/config *(curated)*
- **#15719**: resync manifest prune misses retired .claude/commands/loom/* entries (classifier narrower than the synced surface map) *(curated)* *(held)*
- **#15729**: checkout_ff: emit a record per fast-forward it performs, so a managed checkout's mover is attributable from the store *(curated)*
- **#15732**: test: 4 forge_gateway tests fail under umask 0002 because the fixture's gh-ok marker is written group-writable *(curated)*
- **#15741**: Restore launcher wiring for project hooks during resync and install *(curated)*
- **#15744**: resync manifest prune (#14012) cannot clear stale .claude/commands/loom/ entries *(curated)*
- **#15745**: resync: installed_files is pruned but never extended — manifest under-claims newly shipped payload files (36 → 80 unlisted across one resync) *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)* *(held)*
- **#14186**: Decide whether to retire the sweep warning for out-of-set dependencies *(hermit)* *(held)*
- **#15212**: Remove superseded parse_queue_ref and members_for from group_authz.rs *(hermit)* *(held)*
- **#15364**: Remove unused Autoprefixer from the desktop quickstart *(hermit)* *(held)*
- **#15410**: Remove the senderless SQLite issue_claims registry and its nine IPC requests (~1,200 LOC) *(hermit)* *(held)*
- **#15568**: Remove three remaining senderless terminal IPC requests (GetTerminalOutput, GetTerminalActivity, GetCurrentCommit) and their unreachable output-scraping chain (~2,800 LOC) *(hermit)* *(held)*
- **#15601**: Reuse the reaper’s send_group_signal in role timeouts *(hermit)* *(held)*

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
| Operator merge-risk holds | 38 |
| Very important | 0 |
| Important | 8 |
| Ready (`loom:issue`, net of holds/building/open-linked-PRs) | 0 |
| In Progress (`loom:building`) | 44 |
| Held, no other bucket | 80 |
| PRs awaiting review | 5 |
| Approved PRs awaiting merge | 39 |
| Curated | 122 |
| Architect / Hermit proposals | 7 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
