# jarvisctl North Star

## Thesis

`jarvisctl` should be an operator-first control plane for local and hybrid coding agents.

Its job is to turn a durable task into a controllable runtime, keep that runtime observable, and make repeatable runtime and workspace setup portable across machines.

It should not become the agent itself, the sandbox itself, the model platform itself, or a generic Kubernetes clone.

## What jarvisctl Should Own

### 1. Task intake and lifecycle

- Launch work from durable ticket notes and board transitions.
- Resume the right runtime for the same task instead of spawning disposable shells.
- Record enough runtime metadata that an operator can tell what is running, why it exists, and how to steer it.

This is the strongest existing path today:

- ticket note -> `jarvisctl codex`
- board transition -> `jarvisctl dispatch`
- live runtime -> `tell`, `interrupt`, `attach`, `delete`

### 2. Runtime control contract

- Define one narrow contract for agent runtimes:
  - launch
  - metadata
  - recent events
  - tell/queue/interrupt
  - attach
  - delete
- Support multiple runtime implementations behind that contract.

Today the main runtime is Codex `app-server`. That is correct. The contract matters more than the specific runtime.

### 3. Repeatable workspace and topology setup

- Define a narrow set of runtime-oriented resources for repeatable workspaces.
- Keep multi-agent session shape portable across local and remote execution.
- Focus deployments and services on Codex runtime hosting, not on model-provider orchestration.

### 4. Operator surface

- Show what is live.
- Show enough event history to understand current state.
- Make steering, review, and interruption fast.
- Keep the human in control without forcing them to dig through raw transcripts or random process trees.

### 5. Narrow cluster bridge

- Reuse the same runtime and workspace contracts on Kubernetes when the operator needs remote or long-lived execution.
- Keep the cluster story focused on agent operations, not on building a second general-purpose platform.
- Treat physical and virtual machines as `Node` inventory with labels, roles, taints, probes, and explicit scheduling intent.
- Prefer small adapters such as SSH/Tailscale runtime launch before considering heavier cluster machinery.

## What jarvisctl Should Not Own

### 1. Sandbox and policy platform

Do not rebuild OpenShell or NemoClaw inside `jarvisctl`.

If sandboxing, network policy, file policy, or inference egress control is needed, integrate an external system and treat it as a runtime or environment adapter.

`jarvisctl` should know:

- how to launch into that environment
- how to talk to it
- how to observe it

It should not own the sandbox implementation.

### 2. Model marketplace and provider sprawl

Do not keep adding providers because a new model exists.

The product value is:

- stable runtime operations
- repeatable workspace launch
- operator control

The product value is not "support every model backend."

Provider-specific model routing does not belong in the core product.

### 3. Generic Kubernetes / GitOps parity

Do not chase full Kubernetes parity, full Argo parity, or full control-plane generality.

Keep only the subset that directly improves agent operations:

- runtimes
- deployments
- services
- secrets/config needed for those flows

If a resource exists only because Kubernetes has one, it is probably not worth building.

### 4. Agent behavior framework

`jarvisctl` should not become the place where prompts, agent memory strategy, or high-level reasoning policy live.

That belongs to the runtime or the task note.

### 5. Terminal multiplexer replacement

The native PTY layer should stay focused on running and controlling work sessions.

It does not need to become a general replacement for `tmux`, `screen`, or a shell manager.

## Product Shape

The primary hot path should be:

1. A human moves a ticket into `Ready for Codex`.
2. `jarvisctl dispatch` launches or resumes the right runtime.
3. The runtime works through the task with visible metadata and events.
4. The operator can replicate or host that runtime shape locally or remotely without changing the task contract.
5. The run completes, records outcome, and moves cleanly into review.

Everything outside that path is secondary.

## What to De-Prioritize Now

- Provider-specific model plumbing.
- Broad new resource kinds that do not directly improve runtime or workspace operations.
- Sandbox-specific features that belong in NemoClaw/OpenShell.
- More UI surface area before the core runtime contract is cleaner.

## What to Keep Investing In

- Ticket-driven launches and resume logic.
- Headless `codex app-server` control.
- Queue/interrupt/tell ergonomics.
- Agents and subagent metadata quality.
- Runtime metadata and event quality.
- Portable runtime and workspace manifests.

## The Next 3 Refactors

### 1. Extract a real runtime interface

Problem:

- Codex launch and control paths are split across `main.rs`, `codex.rs`, `codex_app.rs`, and `native.rs`.
- Runtime control is real, but the abstraction boundary is still implicit.

Refactor:

- Introduce an explicit runtime boundary such as:
  - `runtime/mod.rs`
  - `runtime/codex_app.rs`
  - `runtime/native_pty.rs`
- Move launch/control metadata operations behind one trait or command surface.
- Treat TCP, Unix socket, and filesystem queueing as transport details behind that contract.

Result:

- Easier to add or remove runtime backends.
- Cleaner integration point for external environments such as NemoClaw.
- Less `if runtime == ...` growth across the CLI.

### 2. Split `control_plane.rs` into bounded modules

Problem:

- `src/control_plane.rs` is already very large and currently carries schema, storage, reconciliation, services, deployments, and Kubernetes compilation in one file.

Refactor:

- Split by responsibility:
  - manifest schema and parsing
  - state/store
  - reconcile/controllers
  - runtime/deployment services
  - Kubernetes compiler

Result:

- Lower risk when deleting or simplifying features.
- Easier to see what is actually core versus accidental complexity.
- Better chance of pruning dead or weak branches without destabilizing everything else.

### 3. Formalize workspace manifests and deployment topology

Problem:

- Workspace and deployment resources exist, but the intended hot path can still get buried under older non-core control-plane features.
- The product needs a smaller, clearer manifest story centered on Codex runtime hosting and reproducibility.

Refactor:

- Define the supported hot-path resources first:
  - namespace
  - deployment
  - service(targetKind=runtime)
  - config/secret/volume bindings that make those runtimes portable
- Keep Kubernetes rendering focused on the same runtime contract instead of broad parity.
- Remove or de-emphasize any manifest path that primarily exists to orchestrate third-party model backends.

Result:

- The product becomes "repeatable agent workspaces" instead of "many resource kinds."
- You can prune legacy control-plane branches aggressively without losing the core workflow.

## Removal Rule

Keep a feature only if it improves at least one of these:

- launch the right work
- control a live run
- observe current state
- replicate a runtime or workspace safely
- complete the task lifecycle cleanly

If it does not help one of those, it is probably drift.

## Short Version

`jarvisctl` should be the thing that makes coding agents operable.

It should own task-to-runtime control, repeatable workspace setup, and operator visibility.

It should not chase sandbox platforms, model catalogs, or generic infrastructure parity.
