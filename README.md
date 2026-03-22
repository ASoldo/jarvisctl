<img width="73" height="25" alt="image" src="https://github.com/user-attachments/assets/7a880a0a-7ad9-4e8f-a8ac-08931f53089d" />


# jarvisctl

> Enterprise-grade orchestrator for CLI/TUI worker apps using a native PTY runtime

`jarvisctl` runs, inspects, and controls CLI or TUI applications in isolated namespaces. The runtime is native-only now: each namespace is a background PTY session process with a Unix-socket control plane. Older scripts may still pass `--backend native`, but there is no tmux backend anymore.

It is designed to sit underneath an Obsidian-driven Codex workflow: ticket notes stay in the vault, `jarvisctl dispatch` watches board transitions, and the operator uses the dashboard, attach flow, or Waybar counts to see what is live.

---

## Features

* **Namespaces**: Isolated native PTY session processes for agent groups
* **Agents**: Each agent runs in a dedicated PTY inside the namespace
* **Process Inspection**: Query live data (CPU, memory, status, etc.) by name or PID
* **Namespace Shell Access**: Use `nsenter` to exec into target process namespace
* **Structured Logging**: Enable `RUST_LOG=info` or `debug` for detailed logs
* **Agent Command Injection**: Send text/scripts into running agents through the native control plane
* **Codex Ticket Launches**: Start an interactive Codex session directly from a Markdown ticket note
* **Declarative Control Plane**: Manage `Namespace`, `Deployment`, `ReplicaSet`, `Job`, `CronJob`, `Application`, `Service`, `NetworkPolicy`, `ConfigMap`, `Secret`, `Volume`, and `Worker` resources from YAML
* **Kustomize / GitOps Flow**: Render local `kustomization.yaml` trees with `apply -k` or keep them synced through an `Application`
* **Rollout History / Status**: Track generated `ReplicaSet` revisions, pause or undo rollouts, and wait on managed Deployment progress
* **Git-Aware Applications**: Surface local or remote Git source revision state for `Application` resources
* **Worker Backends**: Register bounded Ollama, NVIDIA NIM, or Moonshot/Kimi-backed `Worker` resources for deterministic helper tasks that do not need a full Codex session
* **Obsidian Board Dispatch**: Watch `Ready for Codex` transitions and move cards through `Codex Working` to `Review`
* **Operator Dashboard**: Bare `jarvisctl` opens a ratatui control surface for live namespaces and agents
* **Waybar Status**: Emit namespace and agent counts for a compact desktop status widget
* **Attach/Exec**: Attach to full namespace or specific agent window/PTY
* **Clean Deletion**: Gracefully shut down sessions via `jarvisctl delete`
* **Native Runtime**: Run on `portable-pty` plus a Unix-socket control plane without tmux

---

## Install

```bash
cargo install --path .
```

There is no tmux runtime dependency anymore. A normal Rust toolchain is enough to build and install `jarvisctl`.

---

## Usage Examples

### Open the session dashboard

```bash
jarvisctl
```

Bare `jarvisctl` opens the ratatui operator dashboard. Think of it as a compact `k9s`-style view for local Codex namespaces and agents: use `j`/`k` or arrow keys to move, `Enter` to attach, `i` to interrupt the selected agent, `x` to close the selected namespace, and `r` to refresh.

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
repo_path: /home/rootster/documents/jarvisctl
```

The board column is the launch trigger. `status: ready_for_codex` is still a sensible convention for humans and other tooling, but the dispatcher itself keys off the card transition into `Ready for Codex` plus the ticket ownership and `autostart` gate.

### Launch Codex from a ticket note

```bash
jarvisctl codex \
  --task-note /home/rootster/documents/codex/Tickets/jarvisctl-codex-ticket-launch-bootstrap.md
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
repo_path: /home/rootster/documents/jarvisctl
codex_sandbox_mode: danger-full-access
codex_approval_policy: never
codex_profile: ollama-local
codex_model: gpt-5.4
codex_reasoning_effort: xhigh
codex_completion_status: review
codex_completion_column: Review
codex_finish_mode: close
codex_search: true
codex_add_dirs:
  - /home/rootster/documents/codex
```

`codex_reasoning_effort` currently accepts `none`, `minimal`, `low`, `medium`, `high`, or `xhigh`.
`codex_sandbox_mode` accepts `read-only`, `workspace-write`, or `danger-full-access`.
`codex_approval_policy` accepts `untrusted`, `on-failure`, `on-request`, or `never`.
`codex_finish_mode` accepts `close` or `keep`. The default is `close`, which keeps Waybar and `jarvisctl list` aligned with active work rather than idle shells. `close` means the dispatcher finalizes the run on the tracked Codex stop event and closes the namespace unless you explicitly choose `keep`. `codex_finish_tmux` is still accepted as a compatibility alias in older ticket notes.

### Apply declarative control-plane resources

```bash
jarvisctl apply -f control-plane.yaml
jarvisctl get deployment -n team-alpha
jarvisctl describe service planner-svc -n team-alpha
```

Supported resources:

* `Namespace`
* `Deployment`
* `ReplicaSet`
* `Job`
* `CronJob`
* `Application`
* `Service`
* `NetworkPolicy`
* `ConfigMap`
* `Secret`
* `Volume`
* `Worker`

`Deployment` now reconciles into generated `ReplicaSet` revisions and runtime namespaces such as `team-alpha--planner--rev2--r0`.
`ReplicaSet` is generated by the controller and preserved as rollout history, similar to Kubernetes.
`Job` launches one-shot managed runs and tracks completions, active runs, failures, retries, typed `conditions`, summary `events`, and per-run event timelines.
Short-lived native runs now tolerate the “completed before attach” race, so fast batch jobs can finish successfully without being misclassified as failed during runtime teardown.
`Job` can also target a `Worker` directly through `spec.worker`, which runs an asynchronous local-model task and persists structured `run_details` including backend, worker name, artifact path, optional output path, timestamps, and any terminal error.
`CronJob` accepts Kubernetes-style 5-field schedules such as `* * * * *`, creates timestamped child `Job` resources, and now reports typed `conditions`, `events`, plus child-job `history`.
`Application` is a thin local GitOps layer: it renders a source tree, applies owned resources, prunes resources that disappear from the rendered set, records resolved revision plus sync history, reports local Git dirty-worktree state, exposes typed `conditions` and `events`, and can also render from `spec.source.repoURL` plus `spec.source.targetRevision` through a cached remote checkout.

`Deployment` also supports:

* `spec.paused`
* `spec.progressDeadlineSeconds`
* `spec.restartToken`
* `spec.strategy.type: Recreate|RollingUpdate`
* `spec.strategy.rollingUpdate.maxUnavailable`
* `spec.strategy.rollingUpdate.maxSurge`

`Worker` currently supports:

* `spec.provider: ollama|nvidia|moonshot`
* `spec.model`
* `spec.endpoint`
* `spec.apiKeyEnv`
* `spec.role`
* `spec.systemPrompt`
* `spec.outputMode: text|json`
* `spec.temperature`
* `spec.topP`
* `spec.frequencyPenalty`
* `spec.presencePenalty`
* `spec.numPredict`
* `spec.numCtx`

Worker-targeting `Service` resources now also support schedulable routing defaults:

* `spec.className`
* `spec.fallbackClassNames`
* `spec.requiredCapabilities`
* `spec.preferredProviders`
* `spec.preferLocal`

`Application` source also supports:

* `spec.source.path`
* `spec.source.repoURL`
* `spec.source.targetRevision`

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

### Inspect or force Application sync

```bash
jarvisctl application diff demo-stack -n gitops
jarvisctl application sync demo-stack -n gitops
jarvisctl describe application demo-stack -n gitops --output json
```

`application diff` is read-only and compares the desired rendered source against the live resources currently owned by the `Application`.
`application sync` forces a reconcile even when automated sync is disabled in the manifest.
`describe application` now includes `repo_url`, `source_type`, `source_root`, `source_revision`, and `source_dirty` so Git-backed sources expose where the desired state came from.

### Define and invoke a local worker

```yaml
apiVersion: jarvisctl.io/v1alpha1
kind: Namespace
metadata:
  name: workers-lab
spec: {}
---
apiVersion: jarvisctl.io/v1alpha1
kind: Worker
metadata:
  name: qwen-junior
  namespace: workers-lab
spec:
  provider: ollama
  model: qwen3:8b
  locality: local
  memoryMiB: 8192
  gpuMemoryMiB: 6144
  role: junior
  capabilities:
    - vault
    - routing
  maxConcurrent: 1
  outputMode: json
  temperature: 0
  numPredict: 256
  numCtx: 4096
  systemPrompt: |
    You are a bounded local worker. Return strict JSON only.
```

```bash
jarvisctl apply -f workers.yaml
jarvisctl get workers -n workers-lab
jarvisctl describe worker qwen-junior -n workers-lab --output json
jarvisctl worker invoke qwen-junior -n workers-lab \
  --prompt 'Return JSON with schema {"task":"string","results":[{"path":"string","kind":"code|docs|vault"}]} ...'
```

Use `outputMode: json` for bounded classification, extraction, or routing tasks where a larger Codex session would be wasteful. Use `outputMode: text` only when the task contract is already narrow enough that a plain-text answer is acceptable.
`memoryMiB` and `gpuMemoryMiB` let the scheduler block a local worker before launch when the machine does not currently have enough free RAM or VRAM for that model class.
`describe worker --output json` now also reports `locality`, `capabilities`, `max_concurrent`, `active_runs`, `pending_runs`, `available_slots`, `admission`, `admission_code`, `admission_reason`, and the estimated vs machine-available memory fields, so operator clients can see slot pressure and placement readiness directly.
Hosted `nvidia` and `moonshot` workers use OpenAI-compatible chat-completions endpoints. By default, `nvidia` reads `NVIDIA_API_KEY` and targets `https://integrate.api.nvidia.com/v1/chat/completions`, while `moonshot` reads `MOONSHOT_API_KEY` and targets `https://api.moonshot.cn/v1/chat/completions`.
For GUI-launched operators like the Obsidian plugin, prefer `spec.apiKeySecretRef` over shell env inheritance. A worker can now resolve hosted credentials from a `Secret` resource in the same control namespace, with the env var acting only as an override when present.
Third-party models served through NVIDIA Build, such as `moonshotai/kimi-k2-instruct`, still use `provider: nvidia` because the endpoint and credential are NVIDIA-managed. Use `provider: moonshot` only when you are calling Moonshot's own API directly.
For a concrete hosted-lane setup, see [contrib/openclaw-hosted-workers.yaml](file:///home/rootster/documents/jarvisctl/contrib/openclaw-hosted-workers.yaml). It defines a small `openclaw` namespace with NVIDIA-backed `routing-svc` and `code-svc` worker services over Nemotron and Kimi using secret-backed credentials. Pair it with [contrib/openclaw-hosted-secrets.example.yaml](file:///home/rootster/documents/jarvisctl/contrib/openclaw-hosted-secrets.example.yaml) to create the `Secret` resources those workers expect.

### Define a hosted NVIDIA or Moonshot worker

```yaml
apiVersion: jarvisctl.io/v1alpha1
kind: Worker
metadata:
  name: nemotron-routing
  namespace: workers-lab
spec:
  provider: nvidia
  model: nvidia/nemotron-mini-4b-instruct
  locality: remote
  apiKeySecretRef:
    name: hosted-llm-creds
    key: nvidiaApiKey
  pool: nvidia-mini
  classes:
    - junior-routing
  capabilities:
    - routing
    - json
  maxConcurrent: 2
  outputMode: json
  temperature: 0.2
  topP: 0.7
  numPredict: 256
  systemPrompt: |
    You are a bounded hosted routing worker. Return strict JSON only.
---
apiVersion: jarvisctl.io/v1alpha1
kind: Worker
metadata:
  name: kimi-code
  namespace: workers-lab
spec:
  provider: moonshot
  model: moonshotai/kimi-k2-instruct
  locality: remote
  apiKeySecretRef:
    name: hosted-llm-creds
    key: moonshotApiKey
  pool: moonshot-code
  classes:
    - junior-code
  capabilities:
    - code
    - reasoning
  maxConcurrent: 2
  outputMode: json
  temperature: 0.1
  topP: 0.7
  numPredict: 512
  systemPrompt: |
    You are a bounded hosted code worker. Return strict JSON only.
```

For hosted workers, either provide `apiKeySecretRef` or make sure the API key env var is present in the process environment that runs `jarvisctl`. If the credential source is missing, the scheduler will surface `admission_code: credentials_missing` instead of attempting a request.

### Run a worker-backed Job

```yaml
apiVersion: jarvisctl.io/v1alpha1
kind: Job
metadata:
  name: summarize-docs
  namespace: workers-lab
spec:
  parallelism: 1
  completions: 1
  backoffLimit: 0
  worker:
    selector:
      matchLabels:
        lane: junior
    requiredCapabilities:
      - vault
    preferLocal: true
    timeoutSeconds: 120
    outputPath: /tmp/jarvisctl-worker-job-output.json
    prompt: |
      Return JSON with schema {"task":"string","results":[{"path":"string","kind":"code|docs|vault"}]}.
      Use task="scan" and return one result for path "/home/rootster/documents/codex/Home.md" with kind "vault".
```

```bash
jarvisctl apply -f worker-job.yaml
jarvisctl describe job summarize-docs -n workers-lab --output json
```

Worker-backed jobs do not create a live Codex runtime namespace. Instead, the controller spawns an asynchronous worker-run helper, stores the full worker response under `~/.jarvis/control-plane/state/worker-runs/.../artifacts/`, optionally mirrors it to `spec.worker.outputPath`, and reports per-run metadata in `status.run_details`.
When all matching workers are saturated, runs stay in `phase: pending` with `admission_state: pending` and a scheduler reason such as `waiting for capacity on preferred local worker ...` until a slot becomes available.
When `preferLocal: true` is set and a matching local worker is blocked by RAM or VRAM pressure, the scheduler can fall back to a matching remote worker and records that with `admission_code: remote_fallback`.
`describe job --output json` now also exposes top-level `conditions` and `events`, while each `run_details[]` entry carries its own `events` timeline for UI clients.
Optional `spec.worker.validation.shellCommand` lets you score or verify the produced worker output after the model returns. The validator receives `JARVIS_WORKER_ARTIFACT_PATH`, `JARVIS_WORKER_OUTPUT_PATH`, `JARVIS_WORKER_NAME`, `JARVIS_WORKER_NAMESPACE`, `JARVIS_WORKER_EXECUTION_ID`, `JARVIS_WORKER_PROVIDER`, and `JARVIS_WORKER_MODEL`. Set `failJobOnFailure: true` to make a failed scorecard fail the Job, or leave it `false` to keep the run successful while still surfacing `validation_state`, `validation_message`, and validation events in `status.run_details`.
For a concrete hosted-model scorecard example, see [contrib/openclaw-validated-probes.yaml](file:///home/rootster/documents/jarvisctl/contrib/openclaw-validated-probes.yaml). It layers route and code probes on top of the hosted OpenClaw worker services and demonstrates non-enforcing validation for Nemotron routing plus enforcing validation for Kimi code generation.

### Inspect CronJob status

```bash
jarvisctl describe cronjob minute-worker -n workers-lab --output json
```

`describe cronjob --output json` now includes `conditions`, `events`, `successful_jobs`, `failed_jobs`, and `history[]` entries that summarize each retained child job with its phase, worker-backed flag, selected workers, and last transition timestamp.

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

With that service active, moving a linked ticket card into `Ready for Codex` becomes the normal local launch trigger. The dispatcher then owns the board write-back loop:

* `Ready for Codex` -> launch or resume Codex
* `Codex Working` -> active tracked run
* `Review` -> default completion target after the tracked Codex stop event

The service file assumes:

* `jarvisctl` is installed at `%h/.local/bin/jarvisctl`
* your vault lives at `%h/documents/codex`
* the repo checkout lives at `%h/documents/jarvisctl`

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
