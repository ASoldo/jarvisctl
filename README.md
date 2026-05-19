<img width="73" height="25" alt="image" src="https://github.com/user-attachments/assets/7a880a0a-7ad9-4e8f-a8ac-08931f53089d" />


# jarvisctl

> Operator-first control plane for local and hybrid coding agents

`jarvisctl` turns durable task notes into controllable agent runtimes. It launches or resumes work from tickets, keeps live runtime state observable, and makes repeatable runtime and workspace setup portable across machines.

It is not a sandbox, a model marketplace, or a generic infrastructure control plane. It exists to solve the operator gap around coding agents: start the right work, steer it while it is live, inspect what it is doing, and complete the task lifecycle cleanly.

The local runtime is native-only now: each namespace is a background PTY session process with a Unix-socket control plane. Older scripts may still pass `--backend native`, but there is no tmux backend anymore.

It is designed to sit underneath an Obsidian-driven Codex workflow: ticket notes stay in the vault, `jarvisctl dispatch` watches board transitions, and the operator uses the Obsidian control surface, explicit dashboard, attach flow, or status-bar counts to see what is live.

For the current product direction and pruning criteria, see [docs/NORTH_STAR.md](docs/NORTH_STAR.md).

---

## Core Responsibilities

* Launch or resume coding-agent work from durable ticket notes
* Keep live runtimes steerable with attach, tell, interrupt, delete, and status surfaces
* Give the operator one place to inspect active work across sessions, threads, agents, and subagents
* Define repeatable runtime and workspace resources for multi-agent work
* Keep Obsidian board state, ticket state, and runtime state in sync
* Provide only the control-plane resources needed to support that workflow

## Deliberate Non-Goals

* It is not a sandbox or VM platform
* It is not a model catalog or benchmarking playground
* It is not trying to replace Kubernetes, GitOps, or a general infrastructure control plane

---

## Install

```bash
cargo install --path .
```

There is no tmux runtime dependency anymore. A normal Rust toolchain is enough to build and install `jarvisctl`.

---

## Usage Examples

### Inspect live runtime state

```bash
jarvisctl
```

Bare `jarvisctl` prints the current namespaces and agents without opening a terminal UI. Use `jarvisctl list --json` for automation and Obsidian integrations.

### Open the optional terminal dashboard

```bash
jarvisctl dashboard
```

The ratatui dashboard is an explicit local operator tool for the native multiplexer: use `j`/`k` or arrow keys to move, `Enter` to attach, `i` to interrupt the selected agent, `x` to close the selected namespace, and `r` to refresh.

### Launch a new namespace with multiple agents

```bash
jarvisctl run --namespace botfarm --agents 2 --working-directory /home/rootster/Pictures -- codex --full-auto
```

When `run` starts from an interactive terminal, the new PTY now inherits that terminal size immediately instead of starting at the old fixed `80x24`.

## Obsidian / obsidian-mcp Workflow

`jarvisctl` is the terminal/runtime layer. The task definition still lives in your Obsidian vault, so it works cleanly with an `obsidian-mcp` setup where tickets, projects, and boards are already the durable control plane.

A typical flow looks like this:

1. Create a ticket note with `repo_path`, `owner: codex`, and `autostart: true`.
2. Put a linked card for that ticket on `Ops/Codex Dispatch Board.md` or `Projects/<project>/Board.md`.
3. Move the card into `Ready for Codex`.
4. Let `jarvisctl dispatch` launch or resume Codex, move the card to `Codex Working`, and update the ticket to `active`.
5. Finish the run normally and let dispatch move the card to `Review` by default.

Minimal ticket frontmatter:

```yaml
type: ticket
status: ready_for_codex
owner: codex
autostart: true
project: Projects/jarvisctl/Project.md
repo_path: /home/rootster/work/jarvisctl
```

The board column is the launch trigger. `status: ready_for_codex` is still a sensible convention for humans and other tooling, but the dispatcher itself keys off the card transition into `Ready for Codex` plus the ticket ownership and `autostart` gate.

### Launch Codex from a ticket note

```bash
jarvisctl codex \
  --task-note /home/rootster/codex/Tickets/jarvisctl-codex-ticket-launch-bootstrap.md
```

This will:

* read the ticket note frontmatter and sections
* derive the repo working directory from `repo_path`
* create a `codex-*` namespace on the native runtime
* apply ticket-scoped Codex runtime settings such as sandbox, approval, profile, model, and reasoning effort
* render a prompt bundle from the note and pass it as the initial Codex prompt
* write a launch record under `~/.jarvis/codex/runs/`

Supported ticket frontmatter for Codex runtime overrides:

```yaml
repo_path: /home/rootster/work/jarvisctl
codex_sandbox_mode: danger-full-access
codex_approval_policy: never
codex_model: gpt-5.4
codex_reasoning_effort: xhigh
codex_reasoning_summary: concise
codex_personality: pragmatic
codex_approvals_reviewer: user
codex_service_name: jarvisctl
codex_goal: Finish this ticket and validate the result.
codex_goal_token_budget: 200000
codex_memory_mode: enabled
codex_completion_status: review
codex_completion_column: Review
codex_finish_mode: close
codex_search: true
codex_enable_features:
  - remote-control
codex_add_dirs:
  - /home/rootster/codex
```

`codex_reasoning_effort` currently accepts `none`, `minimal`, `low`, `medium`, `high`, or `xhigh`.
`codex_reasoning_summary` accepts `none`, `auto`, `concise`, or `detailed`.
`codex_personality` accepts `none`, `friendly`, or `pragmatic`.
`codex_approvals_reviewer` accepts `user`, `auto_review`, or `guardian_subagent`.
`codex_sandbox_mode` accepts `read-only`, `workspace-write`, or `danger-full-access`.
`codex_approval_policy` accepts `untrusted`, `on-failure`, `on-request`, or `never`.
`codex_finish_mode` accepts `close` or `keep`. The default is `close`, which keeps Waybar and `jarvisctl list` aligned with active work rather than idle shells. `close` means the dispatcher finalizes the run on the tracked Codex stop event and closes the namespace unless you explicitly choose `keep`. `codex_finish_tmux` is still accepted as a compatibility alias in older ticket notes.

For the current app-server protocol mapping used by the Obsidian plugin, including goals, memory, permission profiles, environments, remote-control status, feature flags, and escape-hatch config, see [docs/CODEX_APP_SERVER_MAPPING.md](docs/CODEX_APP_SERVER_MAPPING.md).

### Apply declarative control-plane resources

```bash
jarvisctl apply -f control-plane.yaml
jarvisctl get deployment -n team-alpha
jarvisctl describe service planner-svc -n team-alpha
```

This surface exists to support agent operations. If a resource does not help launch work, observe runtime state, replicate a workspace, or complete the task lifecycle, it should be treated as drift.

Core operator resources:

* `Node`
* `Namespace`
* `Deployment`
* `Service`
* `ConfigMap`
* `Secret`

Supporting resources:

* `ReplicaSet`
* `NetworkPolicy`
* `Volume`

`Deployment` now reconciles into generated `ReplicaSet` revisions and runtime namespaces such as `team-alpha--planner--rev2--r0`.
`ReplicaSet` is generated by the controller and preserved as rollout history, similar to Kubernetes. It is a supporting resource, not a product-center entrypoint.

`Deployment` also supports:

* `spec.paused`
* `spec.progressDeadlineSeconds`
* `spec.restartToken`
* `spec.strategy.type: Recreate|RollingUpdate`
* `spec.strategy.rollingUpdate.maxUnavailable`
* `spec.strategy.rollingUpdate.maxSurge`
* `spec.template.nodeSelector`
* `spec.template.tolerations`

### Register machine nodes

Nodes are Jarvis inventory for local, SSH, and Tailscale-reachable machines. They record where work can run, what a machine is good for, and whether new work should be scheduled there.

```bash
jarvisctl node register archiechokie --local --role control-plane --role worker --label arch=arm64 --label network=tailscale --max-sessions 3
jarvisctl node register archiebald --address 100.115.119.27 --ssh-host archiebald --ssh-user rootster --role worker --label arch=x86_64 --label network=tailscale --max-sessions 6
jarvisctl get nodes
jarvisctl node ping archiebald
jarvisctl node sync-codex-auth archiebald
jarvisctl node inspect archiebald
jarvisctl node cleanup archiebald
jarvisctl node cordon archiebald
jarvisctl node uncordon archiebald
```

Deployments can select a schedulable node:

```yaml
spec:
  template:
    nodeSelector:
      arch: arm64
      network: tailscale
```

Remote node selection uses SSH to call the selected node's own `jarvisctl codex` command, then folds `jarvisctl list --json` from that node back into local rollout status. Direct `tell`, `interrupt`, `attach`, and `delete` fall back to the remote node when the namespace is not local. Runtime metadata records `JARVIS_NODE_NAME`, `JARVIS_NODE_ADDRESS`, and the `jarvisctl.io/node` label.

Before launching Codex on a remote node, `jarvisctl` syncs the local `~/.codex/auth.json`, `config.toml`, and `version.json` over SSH into the remote node's `~/.codex` directory. The token is streamed over the SSH channel via `tar` stdin, not exposed as a command-line argument. Remote deployment launches are lease-based: the node's previous Codex auth/config files are backed up under `~/.jarvis/codex/auth-leases/<namespace>/` and restored when `jarvisctl delete --namespace <namespace>` closes the remote runtime. If the files did not exist before the leased launch, cleanup removes the synced copies.

You can also refresh a node manually:

```sh
jarvisctl node sync-codex-auth archiebald
```

`jarvisctl node ping <name>` reports `CODEX_AUTH` as `present` or `missing` so you can tell whether that node can start Codex without an interactive login.

### Visit a remote node with Codex

`visit` is the lightweight "go there, look around, come back" path for cluster nodes that keep their own vaults, memories, and workspaces. It sends a bounded prompt capsule to a registered SSH node, runs `codex exec` on that node, returns the final answer locally, and restores the leased Codex auth/config when the visit exits.

```bash
jarvisctl visit \
  --node archiebald \
  --text "Inspect this node's local Codex vault and report what is relevant."
```

Useful options:

* `--node auto` is the default and asks the scheduler to pick the best available remote worker.
* `--working-directory <path>` runs the remote visit from a specific path on that node.
* `--sandbox read-only|workspace-write|danger-full-access` controls the remote Codex sandbox.
* `--timeout-seconds <n>` bounds the whole SSH/Codex visit.
* `--role <role>` and `--label key=value` constrain scheduler selection when `--node auto` is used.
* `--retries <n>` retries scheduled work on another eligible node after a failed visit attempt.
* `--from-current` builds a capsule from the current shell/workspace, live Jarvis sessions, and latest local Codex transcript tail.
* `--from-node <node>` relays the visit through another registered node so one machine can visit another.
* `--full` prints the captured stdout/stderr envelope and cleanup status.

The visit does not require the remote node to share this machine's `/home/rootster/codex` vault. The remote Codex sees that node's own home directory, vault, memory, and local files.
Every visit sends a signed and encrypted capsule opened by `jarvisctl` on the receiving node, writes a local archive under `~/.jarvis/codex/visits/` with the selected options, final answer, stdout/stderr, duration, and cleanup status, and updates `~/.jarvis/codex/visit-index/` so running and finished visits can be listed.

Cluster orchestration helpers:

```bash
jarvisctl node schedule
jarvisctl node doctor
jarvisctl node links
jarvisctl node policy
jarvisctl node reconcile
jarvisctl node rotate-capsule-key
jarvisctl node index
jarvisctl node audit
jarvisctl node task --role worker --retries 1 --text "Inspect yourself and report readiness."
jarvisctl node start-session --task-note /home/rootster/codex/Tickets/name.md --node auto
jarvisctl node fanout --role worker --max-concurrency 4 --text "Report local Codex readiness in one line."
jarvisctl node migrate --session <namespace> --to-node auto
jarvisctl node bootstrap archiebald --ssh-host archiebald --ssh-user rootster --role worker --workspace-root /home/rootster --max-sessions 6
```

`node schedule` picks a reachable, uncordoned worker with Codex, Jarvis, auth, vault, and memory facts. `node doctor` checks all registered nodes for orchestration readiness. `node links` checks directed SSH reachability between registered nodes, including relay paths such as `archiebald -> archiechokie`, and prints Tailscale auth URLs when approval is required. `node policy` creates and prints `~/.jarvis/codex/orchestration.yaml`, which controls default role, labels, retry count, timeouts, fanout concurrency, cleanup retention, and remote index timeout. `node reconcile` runs doctor plus cleanup across available nodes. `node rotate-capsule-key` replaces the encrypted visit capsule key and syncs it to reachable remote nodes. `node index` combines live local/remote runtime sessions with local and remote visit indexes. `node audit` prints auth lease create/restore events. `node task` is the one-shot scheduled AI work path with retry/failover semantics. `node start-session` starts a durable remote Codex app-server session selected by the scheduler and records the node on runtime labels so `attach`, `tell`, `interrupt`, and `delete` can route back to it. `node fanout` sends one protected visit prompt to every selected remote node in bounded parallel batches and returns a per-node result table. `node migrate` sends a resume-style capsule for an existing session to another node so that node can reconstruct useful context in its own vault/memory. `node bootstrap` prepares stable non-interactive `jarvisctl` and `codex` wrappers and registers the node; it only copies the current binary when local and remote CPU architectures match, otherwise it requires an existing remote `jarvisctl`.

Tickets can opt into remote scheduling with frontmatter:

```yaml
jarvis_remote: true
jarvis_node: auto
jarvis_node_role: worker
jarvis_node_labels:
  - network=tailscale
jarvis_node_retries: 1
jarvis_mission: cv-triage-1779146687504
```

Auth lease events are appended to `~/.jarvis/codex/audit.jsonl` without recording token contents. The capsule key is stored at `~/.jarvis/codex/capsule.key` with mode `0600` and copied to nodes during visits/bootstrap so capsules are protected in transit and at rest in temporary files.

Mission ledger commands connect business objectives to the operational evidence produced by tickets, namespaces, nodes, visits, approvals, transcripts, and outcomes:

```bash
jarvisctl mission templates
jarvisctl mission create --template cv-triage --title "CV triage automation" --owner ops --ticket /home/rootster/codex/Tickets/cv-triage.md --node auto
jarvisctl mission create --title "CV triage automation" --objective "Rank candidates for HR review" --priority high --owner ops --ticket /home/rootster/codex/Tickets/cv-triage.md --node auto
jarvisctl mission event <mission-id> --stage task --status running --summary "Started remote review" --namespace cv-triage --node archiebald --evidence transcript:/tmp/cv.jsonl
jarvisctl mission complete <mission-id> --outcome "Shortlist ready for HR review" --evidence report:/tmp/shortlist.md
jarvisctl mission plan <mission-id>
jarvisctl mission policy
jarvisctl mission scorecards
jarvisctl mission smoke --first-node archiechokie --second-node archiebald --first-task-note ./Tickets/a.md --second-task-note ./Tickets/b.md --dry-run
jarvisctl mission list
jarvisctl mission show <mission-id>
```

Built-in mission templates currently cover CV triage, incident response, code review, report generation, bounded worker offload, gated external runtime evaluation, and cross-node relay handoff. Tickets can set `jarvis_mission` so dispatch automatically records launch and completion events against the mission. `node start-session`, `respond-request`, and `delete` also accept `--mission <id>` for direct lifecycle capture.

`mission plan` is the read-only controller view: it evaluates each mission against the current proposal queue and recommends the next bounded action. `mission policy` exposes the default autonomy gates for credentials, production mutation, bounded worker offload, and cross-node handoff. `mission scorecards` gives lane readiness for remote Codex sessions, bounded worker offload, and proposal gating so the Obsidian dashboard can show where autonomy is safe to expand and where evidence is still missing. `mission smoke` records a repeatable two-node mission smoke; run it with `--execute --dry-run=false` when you want it to actually launch paired sessions.

Mission records live under `~/.jarvis/codex/missions/` with append-only event timelines under `~/.jarvis/codex/mission-events/`. They are intentionally separate from tickets: the ticket remains the execution contract, while the mission ledger is the cross-run decision and evidence object.

Proposal commands let agents recommend operational changes before mutating systems:

```bash
jarvisctl proposal create --title "Approve worker offload" --mission <mission-id> --action "Run bounded worker probe" --rationale "Typed task with validator" --risk "Bad worker output" --proposed-by agent0 --evidence ticket:/path/to/ticket.md
jarvisctl proposal decide <proposal-id> --status approved --decision "Approved for this mission." --decided-by rootster
jarvisctl proposal list
jarvisctl proposal show <proposal-id>
```

Use proposals for credentials, paid endpoint onboarding, production mutations, external runtime installs, broad file rewrites, and worker-lane promotion decisions.

Operator requests are durable admin/operator notifications that survive dashboard reloads and normal login churn. They are stored under `~/.jarvis/codex/operator-requests/` and default to a 12-hour response window. Codex app-server approval/input requests are mirrored into this queue, and explicit privileged work can create a sudo request without storing credentials:

```bash
jarvisctl operator-request sudo --title "Install package" --reason "Needed for validator smoke" --command "sudo pacman -S package"
jarvisctl operator-request list
jarvisctl operator-request notify --persistent
jarvisctl operator-request resolve <request-id> --status approved --decision "Approved for this maintenance window."
jarvisctl notify list --output json
```

The request record stores title, reason, risk, command/context, namespace/request links, and decision metadata. It does not store passwords, tokens, or other secrets.

Capability and autonomy commands expose the production-readiness layer used by Mission Chain:

```bash
jarvisctl capability list
jarvisctl capability show codex-remote-session
jarvisctl capability validate
jarvisctl capability register --id custom-worker --title "Custom worker" --lane typed-worker-lane --description "Bounded worker lane"
jarvisctl autonomy reconcile --dry-run
jarvisctl autonomy reconcile --notify
```

The built-in capability registry models remote Codex sessions, bounded worker offload, and operator proposal gating. Each capability carries validators, artifact contracts, evidence, and gaps. `autonomy reconcile` expires stale requests, sends optional persistent desktop notifications, validates capability lanes, and separates safe actions from decision-grade blockers.

Protocol drift for the Codex app-server integration is checked with:

```bash
scripts/check_codex_app_server_schema.sh
```

Node inspection and cleanup support the visit lifecycle:

```bash
jarvisctl node inspect archiebald
jarvisctl node cleanup archiebald --max-age-days 7
```

`node inspect` reports vault, memory, work directory, tool, auth, stale lease, and visit-artifact facts. `node cleanup` restores stale auth leases that do not match live runtimes and prunes old remote visit artifacts.

### Inspect rollout status and history

```bash
jarvisctl rollout status planner -n team-alpha
jarvisctl rollout status planner -n team-alpha --watch --timeout-seconds 300
jarvisctl rollout history planner -n team-alpha
jarvisctl rollout restart planner -n team-alpha
jarvisctl rollout pause planner -n team-alpha
jarvisctl rollout resume planner -n team-alpha
jarvisctl rollout undo planner -n team-alpha --to-revision 3
```

`rollout status` shows the active revision, active `ReplicaSet`, rollout conditions, updated/ready replicas, and runtime namespaces.
`rollout status --watch` polls reconciliation until the Deployment becomes fully available or fails its progress deadline.
`rollout history` shows preserved `ReplicaSet` revisions with their template hashes.
`rollout restart` forces a new revision by changing the Deployment restart token and reconciling a fresh `ReplicaSet`.
`rollout pause` freezes further rollout changes while keeping the current live runtime state.
`rollout resume` unpauses and continues reconciliation.
`rollout undo` points the Deployment back at a prior `ReplicaSet` revision instead of minting a new one.

### Define a repeatable runtime workspace

```yaml
apiVersion: jarvisctl.io/v1alpha1
kind: Namespace
metadata:
  name: runtime-lab
spec:
  default_driver: app_server
---
apiVersion: jarvisctl.io/v1alpha1
kind: Deployment
metadata:
  name: review-bot
  namespace: runtime-lab
spec:
  replicas: 1
  agents: 2
  driver: app_server
  template:
    task_note: /home/rootster/codex/Tickets/review-bot.md
    working_directory: /home/rootster/work/jarvisctl
```

```bash
jarvisctl apply -f runtime-lab.yaml
jarvisctl get deployment -n runtime-lab
jarvisctl rollout status review-bot -n runtime-lab
jarvisctl list --json
```

This is the Kubernetes spirit `jarvisctl` should keep: repeatable runtime and workspace definitions for Codex sessions, not model-provider orchestration.

### Experimental Kubernetes adapter

This is an intentionally narrow cluster adapter. It exists to host remote Codex app-server runtimes and repeatable workspaces when needed, not to make `jarvisctl` a generic Kubernetes control plane.

```bash
jarvisctl kube render -f contrib/codex-kubernetes-runtime-hostpath.yaml
jarvisctl kube apply -f contrib/codex-kubernetes-runtime-hostpath.yaml --context archiebald-k3s
```

`jarvisctl kube render` compiles a supported adapter subset of `jarvisctl` resources into native Kubernetes YAML. Right now that subset is focused on runtime hosting:

* `Namespace -> v1/Namespace`
* `ConfigMap -> v1/ConfigMap`
* `Secret -> v1/Secret`
* `NetworkPolicy -> networking.k8s.io/v1/NetworkPolicy`
* `Deployment(spec.template.kubernetes) -> v1/ConfigMap + apps/v1/Deployment`
* `Service(targetKind=runtime) -> v1/Service`

Kubernetes-hosted Codex runtimes use the same ticket launch contract as local `jarvisctl codex`. A `Deployment` with `spec.template.kubernetes` renders:

* a launch `ConfigMap` carrying the prepared app-server manifest
* an `apps/v1/Deployment` that serves `jarvisctl codex-app-session-serve`
* an optional runtime `Service` exposing the Codex app control port when a matching `Service(targetKind=runtime)` exists

For a real cluster proof, use a baked runtime image instead of bind-mounting the host `jarvisctl` binary into the pod. The checked-in runtime image and manifest are:

* [contrib/Dockerfile.kube-runtime](file:///home/rootster/work/jarvisctl/contrib/Dockerfile.kube-runtime)
* [contrib/codex-kubernetes-runtime-hostpath.yaml](file:///home/rootster/work/jarvisctl/contrib/codex-kubernetes-runtime-hostpath.yaml)

Typical single-node k3s flow:

```bash
docker build -f contrib/Dockerfile.kube-runtime -t jarvisctl-kube-runtime:dev .
docker save jarvisctl-kube-runtime:dev | sudo k3s ctr images import -
jarvisctl kube apply -f contrib/codex-kubernetes-runtime-hostpath.yaml --context archiebald-k3s
kubectl --context archiebald-k3s -n runtime-lab rollout status deployment/codex-runtime
```

The current Kubernetes proof is intentionally narrow:

* `spec.agents` must be `1`
* `spec.replicas` must be `1`
* `spec.driver` must be `app_server`
* `kubernetes.workspaceHostPath` and `kubernetes.workspaceMountPath` must match `spec.template.working_directory`

Generic cluster parity is explicitly out of scope. This surface gets smoke-test maintenance while the core operator path is prioritized.

`jarvisctl kube apply --dry-run-server` automatically falls back to a client dry run when the rendered namespace does not exist yet, because Kubernetes cannot perform a server-side dry run for namespaced objects inside a namespace that has not actually been created.

### Operate an experimental Kubernetes-hosted Codex runtime

Once the compiled Deployment and runtime Service are live in the cluster, `jarvisctl` can reach the in-pod Codex app-server through `kubectl port-forward`:

```bash
jarvisctl kube runtime metadata --service runtime-svc -n runtime-lab --context archiebald-k3s --json
jarvisctl kube runtime attach --service runtime-svc -n runtime-lab --context archiebald-k3s
jarvisctl kube runtime tell --service runtime-svc -n runtime-lab --context archiebald-k3s --text "Continue from the checkpoint." --mode queue
jarvisctl kube runtime interrupt --service runtime-svc -n runtime-lab --context archiebald-k3s
jarvisctl kube runtime delete --deployment codex-runtime -n runtime-lab --context archiebald-k3s
```

These commands accept either `--deployment` or `--service`. `delete` removes the runtime Deployment, its generated `<deployment>-codex-launch` ConfigMap, and any matching runtime Services that resolve to that Deployment.

For the current hostPath-based proof, keep these paths aligned:

* `spec.template.working_directory`
* `spec.template.kubernetes.workspaceHostPath`
* `spec.template.kubernetes.workspaceMountPath`

The pod then gets a usable repo workspace plus whatever additional hostPath mounts you provide for `.codex`, the shared vault, and `.jarvis`.

### Apply a local kustomization tree

```bash
jarvisctl apply -k /path/to/overlay
```

The built-in renderer supports local `resources`, `namespace`, `commonLabels`, and `patches` entries from a `kustomization.yaml` tree. That gives `jarvisctl` the same packaging shape you would use for Kubernetes overlays, without needing an external Kustomize binary.

### Scan boards once for dispatchable tickets

```bash
RUST_LOG=info jarvisctl dispatch --once --dry-run
```

This scans the default vault boards, detects transitions into `Ready for Codex`, and reports what it would do without launching anything or writing back to the board.

### Run the board dispatcher continuously

```bash
RUST_LOG=info jarvisctl dispatch
```

By default this:

* discovers `Ops/Codex Dispatch Board.md` plus project `Board.md` files under the vault
* loads dispatch state from `~/.jarvis/dispatch/<vault>-state.json`
* polls every `15` seconds unless you override `--interval-seconds`
* only reloads boards whose file contents changed, plus boards that still own active Codex runs
* launches only tickets with `owner: codex` and `autostart: true`
* moves launched cards from `Ready for Codex` to `Codex Working`
* changes launched ticket `status` to `active`
* resumes the previous Codex conversation for that ticket when a recorded session id exists
* detects completion from the tracked Codex stop hook, even when the wrapper process is still alive for a moment
* moves completed cards from `Codex Working` to the ticket-defined completion column, defaulting to `Review`
* changes completed ticket `status` to the ticket-defined completion status, defaulting to `review`
* treats a manual move out of `Codex Working` as cancellation, writes that back to the ticket, clears the active Codex session, and closes the namespace

Ticket lifecycle defaults:

* `Ready for Codex` means "launch or resume now"
* `Codex Working` means there is an active tracked run in the dispatcher state
* `Review` is the default completion column
* `review` is the default completion status
* completion closes the namespace by default unless `codex_finish_mode: keep`

If you are using the Codex CLI from inside the launched PTY, Codex can still spawn its own subagents normally. `jarvisctl` is responsible for the namespace, attachability, lifecycle, and visibility of that session, not for replacing Codex's agent model.

### Attach to the full namespace

```bash
jarvisctl attach --namespace botfarm
```

When the native agent exits, `attach` returns automatically to your original terminal. If you attached from the dashboard, `ctrl+b d` or `ctrl+b :detach` returns you to the list instead.

By default the local leader is `ctrl+b`. If `jarvisctl` is launched from inside an outer tmux session, the leader automatically switches to `ctrl+g` so the outer mux does not swallow `ctrl+b` before the attach client sees it. You can override this with `JARVISCTL_LEADER=ctrl-a`, `ctrl-b`, or `ctrl-g`.

Use the active local leader inside attach:

* `<leader> d` detaches locally
* `<leader> :` opens a local command line, so `:detach` works without sending `:` into the app
* `<leader> c` sends an interrupt to the foreground app
* `<leader> <leader-key>` forwards a literal leader keystroke into the app

Direct detach fallbacks are `ctrl+]`, `ctrl+\\`, and `F12`. Plain `:` still goes to the app unless you entered the local command line through `<leader> :`.
The native attach client reserves a full-width footer line showing the namespace, agent, and local controls, and it now resizes the remote PTY with a real `SIGWINCH` path so full-screen TUIs can expand to the available terminal area.
It also recognizes modern CSI-u / enhanced keyboard escape sequences from apps like Neovim and Codex, so leader and fast-detach keys still work when the terminal stops sending plain raw control bytes.

### Exec into a single agent window

```bash
jarvisctl exec --namespace botfarm --agent agent0
```

### Send a file to an agent (one line per command)

```bash
jarvisctl tell --namespace botfarm --agent agent0 --file prompt.md 
```

For Codex app-server sessions, `tell` also accepts `--mode auto|steer|queue`.

### Read Codex thread history

```bash
jarvisctl history --namespace codex-example --json
jarvisctl history --namespace codex-example
```

`history` calls app-server `thread/read` with `includeTurns` and returns the persisted thread payload. The plain view is intentionally compact for operator scans; plugin clients should use `--json`.
`auto` keeps the existing behavior and steers the active turn when one is running.
`queue` starts a follow-up turn without clobbering the current active turn pointer, which covers the queued follow-up workflow added in newer Codex builds.

```bash
jarvisctl tell --namespace codex-ticket-123 --agent agent0 --text "After this, audit the tests too." --mode queue
```

Interrupt a stuck agent:

```bash
jarvisctl interrupt --namespace botfarm --agent agent0
```

### List managed sessions and their windows

```bash
jarvisctl list
```

### Inspect a process by name or PID

```bash
jarvisctl inspect --name codex-cli
jarvisctl inspect --pid 1234 --exec-shell
```

### Kill a namespace and all associated agents

```bash
jarvisctl delete --namespace botfarm
```

---

## Internals

* the runtime uses a per-namespace background process plus `portable-pty`
* Process info is gathered using `sysinfo`
* Shell access via `nsenter -t PID -a /bin/bash` (requires `sudo`)
* Generic text injection goes through the native control socket so multiline content stays intact
* Ticket-driven Codex launches write prompt bundles to `~/.jarvis/codex/prompts/`, pass the bundle as the initial Codex prompt, and write launch records to `~/.jarvis/codex/runs/`

## Native Backend Scope

Current native coverage:

* `run`
* `codex`
* `dispatch`
* `dashboard`
* `list`
* `attach`
* `exec`
* `tell`
* `interrupt`
* `delete`

Current native limitations:

* `attach` defaults to `agent0` for native `jarvisctl attach --namespace ...`
* the native runtime is intentionally session-scoped, not a full general-purpose multiplexer
* native sessions automatically clean themselves up once every agent has exited

## Seamless Workflow Shape

The clean integration point for the future board watcher is:

1. Board transition watcher detects `Ready for Codex`
2. Watcher validates the linked ticket note
3. Watcher calls `jarvisctl codex --task-note <ticket.md>`
4. `jarvisctl` handles namespace creation, ticket-scoped runtime flags, initial prompt handoff, run recording, and Codex session id capture
5. The watcher owns lifecycle policy: launch, resume, completion, cancellation, and write-back without owning terminal orchestration details

That keeps the board/rules engine separate from the terminal orchestration layer. The watcher only decides **when** to launch; `jarvisctl` decides **how** to launch Codex cleanly.

Today `jarvisctl dispatch` is that first watcher layer. It is still a lean polling daemon with JSON state rather than a full SQLite rules engine, but the boundary is already there: board scanning and policy live in dispatch, while the Codex launch contract stays in `jarvisctl codex`.

## Systemd User Service

An example service unit is included at [contrib/jarvisctl-dispatch.service](./contrib/jarvisctl-dispatch.service).

Typical setup:

```bash
install -Dm644 contrib/jarvisctl-dispatch.service ~/.config/systemd/user/jarvisctl-dispatch.service
systemctl --user daemon-reload
systemctl --user enable --now jarvisctl-dispatch.service
```

The example service intentionally runs at lower priority with a `15` second interval so it does not compete aggressively with active Codex sessions or other local development work on the same machine.

With that service active, moving a linked ticket card into `Ready for Codex` becomes the normal local launch trigger. The dispatcher then owns the board write-back loop:

* `Ready for Codex` -> launch or resume Codex
* `Codex Working` -> active tracked run
* `Review` -> default completion target after the tracked Codex stop event

The service file assumes:

* `jarvisctl` is installed at `%h/.local/bin/jarvisctl`
* your vault lives at `%h/codex`
* the repo checkout lives at `%h/work/jarvisctl`

Adjust the paths if your machine layout differs.

## Waybar Widget

An example Waybar script is included at [dotfiles/jarvisctl_status.sh](./dotfiles/jarvisctl_status.sh), with a matching module snippet in [dotfiles/wayland.config.jsonc](./dotfiles/wayland.config.jsonc).

Typical setup:

```bash
install -Dm755 dotfiles/jarvisctl_status.sh ~/.jarvis/scripts/jarvisctl_status.sh
```

The widget parses `jarvisctl list` and emits JSON with:

* live namespace count
* live agent count
* a tooltip listing the active namespaces and agents

Example output while one dispatched Codex run is active:

```json
{"text":"  1  1","tooltip":" NAMESPACES:\n..."}
```

Because `codex_finish_mode` defaults to `close`, completed board-driven runs drop back out of `jarvisctl list` and the widget returns to zero instead of showing stale idle shells.

---

## Logging

Enable structured logs using environment variables:

```bash
RUST_LOG=info jarvisctl list
```

---

## License

MIT License

---
