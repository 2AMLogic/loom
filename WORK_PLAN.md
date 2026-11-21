# Work Plan

Backlog snapshot from current GitHub label state, maintained by the Guide role. Direction lives in the hand-written roadmap: [docs/ROADMAP.md](docs/ROADMAP.md).

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. The `_Generated:` stamp at the top of the generated region dates the snapshot; `git log -1 -- WORK_PLAN.md` finds the merge that wrote it.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
_Generated: 2026-11-21T22:13:08Z (snapshot; compare against live labels)_
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs under a hold (`loom:blocked` or `loom:operator`) — only a human merge decision is missing.

- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills
- **#14046**: refactor: remove the loom-api crate from the workspace
- **#14078**: fix: treat gh api -F/--field and -f/--raw-field as writes and flag stalled census reads
- **#14335**: docs: drop stale file-size ratchet comments from loom-daemon and ci.yml (slice c)
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder
- **#14441**: fix: publish worker images under the repository owner's GHCR namespace
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file
- **#14478**: feat: serve hold diagnostics and scoped issue workflows from loom-tools
- **#14510**: fix: select hook-ready Codex seats and honor shared account disables
- **#14576**: fix(lease): route installed issue lease reads through Loom mode
- **#14696**: fix(dispatch): admit on the containment proof the walk already took
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge
- **#14728**: ci: capture kernel stack via sudo -n fallback in discovery-stall diagnostic
- **#14747**: fix: stamp daemon compatibility on quick installs
- **#14769**: fix: skip source-remote lookup on no-op resyncs
- **#14820**: feat(telemetry): break down ratification key outcomes
- **#14855**: fix: treat unparseable package.json as not-a-stub in resync gitignore stale-check
- **#14858**: feat(daemon): update ci-gate parked branches once main recovers
- **#14864**: docs(judge,doctor): name the dirty-worktree disposition in drift recovery
- **#14866**: fix: verify Forge head identity before PR worktree selection
- **#14871**: fix(roll): fence and retry the release of interrupted steps
- **#14873**: feat: add host-owned per-repo Git object mirrors with proved read authority
- **#14890**: fix: re-record Champion holds at a new head so --release-hold stays usable

## Very Important

`loom:very-important` (one counts per workspace); every stage takes these first.

_None._

## Important

`loom:important`; taken before unlabelled work.

- **#10420**: loom-daemon on Linux: ~2.2 GB RSS baseline (Macs ~200 MB); ci_telemetry adds ~2.2 GB and its spans overflow the 2000-record export queue *(held)*
- **#12909**: ci_telemetry: 2amlogic owner discovery times out every cycle since Oct 17 (no loom-ui CI facts; 478 consecutive failures) *(held)*
- **#12993**: Shadow evaluation of the new Champion (2026-10-20): 1/95 holds cleared — ranked remaining causes + fixtures *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools
- **#14565**: Test which forge route serves installed issue leases in the three Loom modes
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(held)*
- **#14851**: Investigate Codex Champion merge steps that exit before recording a verdict *(held)*

## Ready

Human-approved issues (`loom:issue`) ready for implementation — excluding held (`loom:blocked`/`loom:operator`/`loom:frozen`), `loom:building`, and issues closed by an open `loom:pr` PR (listed under Approved).

- **#11107**: Hyperparameters: drop the optimizer-only $LOOM_HYPERPARAMS vector tier (precedence becomes env > config > default)
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435)
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools
- **#14730**: First daemon start on a new host cannot fetch the fleet store: fleet-config render is shed when readers are configured and no daemon is running
- **#14740**: Fresh install writes no requires_daemon, so newly admitted repos are dispatch-held when the fleet floor moves

## In Progress

Issues currently being built (`loom:building`).

- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills
- **#14011**: gh-cached: `gh api -F/--raw-field` writes are classified as reads (retried on timeout, and marked cacheable); two smaller #13930 follow-ups
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145)
- **#14221**: Shipped files, installer and resync output name 2AMLogic/loom; test rejects the old slug
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder
- **#14419**: Release cannot publish container images since the transfer: pushes to ghcr.io/rjwalters are denied, so no session image exists after 0.19.1311
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1
- **#14474**: Expose worktree operations through the shared Rust CLI
- **#14560**: Work-finder completion gaps leave the Fleet queue stale
- **#14586**: Stub detection root vs authority root: two residual asymmetries in the #13898 .gitignore path
- **#14656**: judge.md/doctor.md drift recovery: dirty-worktree arm refuses with no named next step (and no longer surfaces the snapshot path)
- **#14657**: Check the PR head repository before selecting Judge and Doctor worktrees
- **#14680**: Share origin-only Git objects across Codex seats through read-only per-repository mirrors
- **#14739**: forge_identity: a release.checksum refusal withdraws the reader gateway for the whole owner, shedding every 2AMLogic read ~20 min/h since 0.20.0
- **#14750**: Part-of parent-reference rule is inconsistent across dependency-ref consumers (and Part of Epic #N defeats it)
- **#14777**: Bound the private-workspace seat reclaim pass's contribution to the dispatch tick
- **#14798**: Ratification telemetry cannot explain which key reviews are withheld
- **#14813**: ci-gate parked PRs never reach exact-head green after main recovers; Champion keep-parked text states an unreachable release
- **#14815**: Release each step’s own role claim when a daemon roll interrupts it
- **#14816**: Reconcile stale Curator claims and verify recovery of the three roll incidents
- **#14833**: merge-pr.sh --release-hold refuses a valid release after Champion re-holds at a new head without a new hold-state marker
- **#14848**: workspace_resync: an install-incompatible held workspace gets no resync PR (four repos held 26 h+)
- **#14850**: Run one stage against a fresh Gitea instance: verify the Gitea path on current main and list what is broken
- **#14870**: Share the orphan-directory fallback in issue and PR worktree cleanup
- **#14872**: Consolidate the four ListSweeps routing cases into one two-workspace IPC test
- **#14876**: clean: stale-branch pass deletes a mis-prefixed 'heads/<name>' branch and fails permanently on PR-fetch ref residue
- **#14878**: clean --safe: stale 'initializing' worktree locks (git add crash debris) are never reclaimed
- **#14879**: clean --safe: closed-issue branch cleanup force-deletes unpushed commits via the -d to -D fallback
- **#14881**: Use the observation timestamp when booking a scoped rate-limit refusal
- **#14883**: Completion envelopes never count Codex steps: merge_step_tokens_by_model reads Claude and OpenCode only, so the feed shows no Codex model
- **#14884**: Session-only mode: publish the gateway relay profiles so attended hosts keep Worker access
- **#14885**: loom-daemon finds docker only by bare name on PATH, so a launchd-started caller cannot reach session containers
- **#14886**: accounts check --live reports a missing container runtime as session_probe_error, with nothing on stderr
- **#14888**: Verify that one Doctor tick advances past an inherited-red PR in a 15-row queue
- **#14889**: Seat reclaim pass: log and emit each removed worktree and scratch entry
- **#14892**: Guide sends evidence waits containing “deployed revision” or “installed revision” to triage
- **#14898**: Carry the Judge verdict and evaluated commit onto the step root
- **#14899**: Determine why Judge post-state probes and terminal outcomes are missing on the same attempts
- **#14902**: Reconcile issue bookkeeping when a merged PR is observed

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#12969**: feat: age-bound Codex rollout history in live profiles *(held)*
- **#14108**: fix(private_workspace): forward the freshest owner token, not a fixed primary-first one
- **#14260**: fix(daemon): contained launches draw only private-clone accounts registering the workspace repository
- **#14281**: docs: name 2AMLogic/loom in shipped files and add old-slug guard test
- **#14470**: fix: remove stale by-owner writer credentials once the owner is no longer a cross-owner
- **#14498**: fix: pass over unprovisioned and sandbox-held Codex seats before a draw *(held)*
- **#14514**: feat(tools): share worktree CLI operations and read routing
- **#14612**: fix: launch eager reclaim detached so it cannot hold back a work-finder tick *(held)*
- **#14753**: fix: mint a bootstrap reader so first-start fleet-config render can confirm the store
- **#14758**: fix(release): keep checksum reads on the reader route *(held)*
- **#14860**: feat: heal stale loom:curating claims through the trusted Curator lease
- **#14887**: fix: reclaim worktrees stuck behind stale git 'initializing' locks
- **#14895**: fix: keep revision evidence waits on hold in Guide
- **#14897**: fix: keep unpushed issue branch tips during safe cleanup
- **#14901**: refactor: share the orphan-directory fallback between issue and PR worktree cleanup
- **#14906**: test: consolidate ListSweeps routing cases into one two-workspace IPC test
- **#14907**: fix: report a missing session runtime once during live account checks
- **#14908**: fix: book scoped refusal bucket entries at the observation instant
- **#14911**: feat: log and export each seat-reclaim removal as a typed record
- **#14913**: fix: retry the step post-state read at settle when the release noted its read failed
- **#14914**: test: cover a step tick walking past an inherited-red Doctor row
- **#14916**: fix: enumerate cleanup branch candidates by full ref under residue refs
- **#14917**: fix: exclude 'Part of Epic #N' parents in every dependency-ref consumer

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#13997**: docs(judge): split judge.md into always-read core + on-demand judge-* sub-skills — closes #13957
- **#14046**: refactor: remove the loom-api crate from the workspace — closes #13820
- **#14078**: fix: treat gh api -F/--field and -f/--raw-field as writes and flag stalled census reads — closes #14011
- **#14335**: docs: drop stale file-size ratchet comments from loom-daemon and ci.yml (slice c) — closes #13827
- **#14384**: fix: require native ownership before handing a fresh worktree to a Builder — closes #14376
- **#14441**: fix: publish worker images under the repository owner's GHCR namespace — closes #14419
- **#14452**: fix: record held_back paths and exit 3 when resync withholds a locally-fixed file — closes #14434
- **#14478**: feat: serve hold diagnostics and scoped issue workflows from loom-tools
- **#14510**: fix: select hook-ready Codex seats and honor shared account disables — closes #14497
- **#14576**: fix(lease): route installed issue lease reads through Loom mode — closes #14565
- **#14696**: fix(dispatch): admit on the containment proof the walk already took — closes #14626
- **#14715**: fix: send verified Champion-held Doctor repairs back to Judge — closes #14714
- **#14728**: ci: capture kernel stack via sudo -n fallback in discovery-stall diagnostic — closes #14708
- **#14747**: fix: stamp daemon compatibility on quick installs
- **#14769**: fix: skip source-remote lookup on no-op resyncs — closes #14712
- **#14820**: feat(telemetry): break down ratification key outcomes — closes #14798
- **#14855**: fix: treat unparseable package.json as not-a-stub in resync gitignore stale-check — closes #14586
- **#14858**: feat(daemon): update ci-gate parked branches once main recovers — closes #14813
- **#14864**: docs(judge,doctor): name the dirty-worktree disposition in drift recovery — closes #14656
- **#14866**: fix: verify Forge head identity before PR worktree selection — closes #14657
- **#14871**: fix(roll): fence and retry the release of interrupted steps — closes #14815
- **#14873**: feat: add host-owned per-repo Git object mirrors with proved read authority
- **#14890**: fix: re-record Champion holds at a new head so --release-hold stays usable — closes #14833

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
- **#13827**: Remove stale file-size ratchet comments: 242 files cite a gate #12099 deleted, and new code still obeys it *(curated)*
- **#13865**: [ci-daily] Supply Chain *(curated)* *(held)*
- **#13933**: forge_auth forwards an expired token: a stale per-workspace gh-config-by-owner wins over the refreshed primary one *(curated)*
- **#13957**: Shrink always-read core of judge.md (173 KB) into on-demand sub-skills *(curated)*
- **#13966**: Z.ai weekly-limit 429 is bad-marked for 60 s: exhausted keys are relaunched all day and the role never falls through to Claude *(curated)* *(held)*
- **#14011**: gh-cached: `gh api -F/--raw-field` writes are classified as reads (retried on timeout, and marked cacheable); two smaller #13930 follow-ups *(curated)*
- **#14110**: Contained roles: select only private-clone accounts registering the repository; clear refusal, status and docs *(curated)*
- **#14112**: Private-clone session image follows the daemon release tag; recreate idle private sessions at a daemon roll *(curated)* *(held)*
- **#14195**: A 2xx probe with no 5h-utilization header writes an evidence-free `available` row that collapses a weekly hold (sibling of #14145) *(curated)*
- **#14218**: Verify the repaired publish paths actually fire after the org transfer (post-merge live check for #14158) *(curated)*
- **#14221**: Shipped files, installer and resync output name 2AMLogic/loom; test rejects the old slug *(curated)*
- **#14232**: Spike: does regenerating CODEX_HOME/config.toml with inline hooks per launch avoid the private-clone hook-trust prompt? *(curated)* *(held)*
- **#14235**: Design: let a roleDraw attempt table escalate on named outcome reasons, once #14233 has data *(curated)* *(held)*
- **#14292**: test flake: test-docs-guide-lock.sh Test 4 (#5573 race) fails on Shell Test Suites (hermetic, 2/2) — two concurrent acquires both win *(curated)* *(held)*
- **#14371**: Per-seat session container for Claude steps: launch in the configured session image, one login per container *(curated)*
- **#14376**: Refuse a peer-owned claim before handing a new worktree to a Builder *(curated)*
- **#14419**: Release cannot publish container images since the transfer: pushes to ghcr.io/rjwalters are denied, so no session image exists after 0.19.1311 *(curated)*
- **#14433**: accounts session recover --publish cannot authenticate: the seat's credential helper has no token outside a job *(curated)* *(held)*
- **#14434**: resync-installed.sh restamps loom_version even when a file is blocked and the run exits 1 *(curated)*
- **#14450**: Remove a by-owner credential directory once its owner is no longer a cross-owner (follow-up to #14435) *(curated)*
- **#14455**: Codex Curator steps are dispatched to stopped, unprovisioned seats and end SANDBOX_UNAVAILABLE every time *(curated)*
- **#14461**: Serve the remaining forge actions through the shared loom-tools dispatcher *(curated)* *(held)*
- **#14464**: Expose park records and operator decisions through loom-tools *(curated)* *(held)*
- **#14465**: Serve hold diagnostics, curation labels and blocker notifications from loom-tools *(curated)*
- **#14474**: Expose worktree operations through the shared Rust CLI *(curated)*
- **#14475**: Verify attended issue-lease routing in all three loom-tools modes *(curated)* *(held)*
- **#14497**: Codex seats: trusted-seat selection for guarded roles, machine-level accounts disable, state-change release of the sandbox hold *(curated)*
- **#14530**: Live-verify the admitted loom-tools forge writes against a controlled repository *(curated)* *(held)*
- **#14534**: recover --publish: CLI discovers the daemon's workspace root without an exported variable (AC1 of #14433) *(curated)* *(held)*
- **#14553**: Live-verify the migrated merge-queue actions in tools-only session mode (#14520 AC3 remainder) *(curated)* *(held)*
- **#14560**: Work-finder completion gaps leave the Fleet queue stale *(curated)*
- **#14565**: Test which forge route serves installed issue leases in the three Loom modes *(curated)*
- **#14566**: Telemetry: about 40% of OpenCode (GLM) runs on 0.19.1316 emit no loom.runtime.usage row *(curated)* *(held)*
- **#14583**: Keep quiet dispatch disposition rows inside the five-minute Fleet freshness target *(curated)* *(held)*
- **#14586**: Stub detection root vs authority root: two residual asymmetries in the #13898 .gitignore path *(curated)*
- **#14613**: CI: backend path filter omits the role-prompt trees, so prompt-only PRs skip the 14 Rust tests that lint them *(curated)* *(held)*
- **#14626**: Private-clone admission contains the winning seat twice per admission *(curated)*
- **#14634**: Dashboard Builder repeatedly selects OpenCode and exits at preflight on loom-worker-3 *(curated)* *(held)*
- **#14656**: judge.md/doctor.md drift recovery: dirty-worktree arm refuses with no named next step (and no longer surfaces the snapshot path) *(curated)*
- **#14657**: Check the PR head repository before selecting Judge and Doctor worktrees *(curated)*
- **#14663**: [ci-daily] Flake Detection (shell suites, ubuntu-latest, parallelism 1) *(curated)* *(held)*
- **#14665**: Live verification: post-fix Codex usage-missing breakdown and the 17 unaccounted runs (#14664) *(curated)* *(held)*
- **#14680**: Share origin-only Git objects across Codex seats through read-only per-repository mirrors *(curated)*
- **#14685**: Live-verify Docker retention per-evaluation summaries on a containerd-store worker (#14679 follow-up) *(curated)* *(held)*
- **#14708**: ci: discovery-stall diagnostic prints `(unreadable)` for /proc/<pid>/stack — needs sudo to capture the kernel stack *(curated)*
- **#14712**: resync: a no-op resync warns that it stamped the canonical loom_source_remote, but leaves the stale one *(curated)*
- **#14714**: Held Doctor repairs leave stale approvals outside the Judge queue *(curated)*
- **#14730**: First daemon start on a new host cannot fetch the fleet store: fleet-config render is shed when readers are configured and no daemon is running *(curated)*
- **#14737**: Observed Loom step attempt spans omit the recorded failure class *(curated)* *(held)*
- **#14739**: forge_identity: a release.checksum refusal withdraws the reader gateway for the whole owner, shedding every 2AMLogic read ~20 min/h since 0.20.0 *(curated)*
- **#14740**: Fresh install writes no requires_daemon, so newly admitted repos are dispatch-held when the fleet floor moves *(curated)*
- **#14741**: pricing: gpt-6.2-astra is not on the pricing card; every Codex step is costed at the Sonnet fallback *(curated)* *(held)*
- **#14750**: Part-of parent-reference rule is inconsistent across dependency-ref consumers (and Part of Epic #N defeats it) *(curated)*
- **#14765**: Live verification: session_reconcile creates an App-only private-clone seat after #14744 *(curated)* *(held)*
- **#14777**: Bound the private-workspace seat reclaim pass's contribution to the dispatch tick *(curated)*
- **#14785**: Guide work plan counts issues with open partial PRs as ready for new Builders *(curated)* *(held)*
- **#14792**: Finished Builder lease cleanup can shed its repository lookup before yielding *(curated)* *(held)*
- **#14795**: Stage-occupancy snapshots block Fleet disposition refreshes for minutes *(curated)* *(held)*
- **#14798**: Ratification telemetry cannot explain which key reviews are withheld *(curated)*
- **#14808**: A PR that says "Part of #<operator-held tracker>" inherits the hold and is never drawn by Judge or Doctor, with no visible reason (three chip PRs stalled 6 to 27 days; a fourth held with no visible reason) *(curated)* *(held)*
- **#14810**: A parked PR whose blocker cleared but which now conflicts with main has no route back: the unblock check reports it Held and Doctor's hold exclusion hides it *(curated)* *(held)*
- **#14813**: ci-gate parked PRs never reach exact-head green after main recovers; Champion keep-parked text states an unreachable release *(curated)*
- **#14815**: Release each step’s own role claim when a daemon roll interrupts it *(curated)*
- **#14816**: Reconcile stale Curator claims and verify recovery of the three roll incidents *(curated)*
- **#14823**: CI: fleet-wide docs-only early exit (one allowlist, green-base diff, fail closed) — operator-approved rule 3/11 exception *(curated)* *(held)*
- **#14833**: merge-pr.sh --release-hold refuses a valid release after Champion re-holds at a new head without a new hold-state marker *(curated)*
- **#14841**: Judge steps on Claude often run about 8 minutes and end with no outcome line; the ledger then classes them from a post-state probe that fails *(curated)* *(held)*
- **#14848**: workspace_resync: an install-incompatible held workspace gets no resync PR (four repos held 26 h+) *(curated)*
- **#14850**: Run one stage against a fresh Gitea instance: verify the Gitea path on current main and list what is broken *(curated)*
- **#14851**: Investigate Codex Champion merge steps that exit before recording a verdict *(curated)* *(held)*
- **#14876**: clean: stale-branch pass deletes a mis-prefixed 'heads/<name>' branch and fails permanently on PR-fetch ref residue *(curated)*
- **#14878**: clean --safe: stale 'initializing' worktree locks (git add crash debris) are never reclaimed *(curated)*
- **#14879**: clean --safe: closed-issue branch cleanup force-deletes unpushed commits via the -d to -D fallback *(curated)*
- **#14881**: Use the observation timestamp when booking a scoped rate-limit refusal *(curated)*
- **#14883**: Completion envelopes never count Codex steps: merge_step_tokens_by_model reads Claude and OpenCode only, so the feed shows no Codex model *(curated)*
- **#14884**: Session-only mode: publish the gateway relay profiles so attended hosts keep Worker access *(curated)*
- **#14885**: loom-daemon finds docker only by bare name on PATH, so a launchd-started caller cannot reach session containers *(curated)*
- **#14886**: accounts check --live reports a missing container runtime as session_probe_error, with nothing on stderr *(curated)*
- **#14888**: Verify that one Doctor tick advances past an inherited-red PR in a 15-row queue *(curated)*
- **#14889**: Seat reclaim pass: log and emit each removed worktree and scratch entry *(curated)*
- **#14892**: Guide sends evidence waits containing “deployed revision” or “installed revision” to triage *(curated)*
- **#14898**: Carry the Judge verdict and evaluated commit onto the step root *(curated)*
- **#14899**: Determine why Judge post-state probes and terminal outcomes are missing on the same attempts *(curated)*
- **#14902**: Reconcile issue bookkeeping when a merged PR is observed *(curated)*
- **#14910**: Test flake: tools-only issue lease fixture fails to observe the ordinary renewer *(curated)*
- **#14912**: Session-only hosts: Codex session containers have no restart policy and nothing reconciles them after a Docker restart *(curated)*
- **#14915**: Champion WF_BLAST: 'secrets.' pattern matches file names in path filters and step names *(curated)*

## Proposed (Architect / Hermit)

- **#13071**: Peer claims: move machine coordination off rendered room messages (custom event type / state events) *(architect)* *(held)*
- **#14186**: Remove warn-out-of-set-deps.sh: advisory script with no caller *(hermit)* *(held)*
- **#14647**: Remove unused Radix dropdown and switch dependencies from both UI quickstarts *(hermit)* *(held)*
- **#14846**: Exercise the installer’s retired-file cleanup directly in Section 5b *(hermit)* *(held)*

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
| Operator merge-risk holds | 23 |
| Very important | 0 |
| Important | 7 |
| Ready (`loom:issue`, net of holds/building/merge-pending) | 7 |
| In Progress (`loom:building`) | 42 |
| PRs awaiting review | 23 |
| Approved PRs awaiting merge | 23 |
| Curated | 127 |
| Architect / Hermit proposals | 4 |
| Active epics | 7 |
<!-- guide:plan-body:end -->
