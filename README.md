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
