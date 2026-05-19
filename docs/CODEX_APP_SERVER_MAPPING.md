# Codex app-server mapping

`jarvisctl` treats Codex app-server as the agent runtime contract. The Obsidian plugin should write durable ticket frontmatter; `jarvisctl` turns that frontmatter into app-server `thread/start`, `thread/resume`, `thread/goal/set`, and `turn/start` parameters.

Checked against Codex CLI `0.131.0` and the generated experimental app-server schema from `codex app-server generate-json-schema --experimental --out <dir>`.

## Current release signals

- `codex remote-control` is now the preferred headless remote entrypoint for remotely controlled app-server runtimes.
- `thread/start`, `thread/resume`, and `turn/start` now expose richer per-thread and per-turn configuration: model, service tier, approvals reviewer, permission profiles, environments, personality, and instruction overrides.
- Experimental app-server fields require `initialize.capabilities.experimentalApi = true`.
- `permissions` is a named profile string in `thread/start`, `thread/resume`, and `turn/start`; extra writable roots map to `runtimeWorkspaceRoots`.
- `codex_memory_mode` is applied after thread creation with `thread/memoryMode/set`.
- Server-initiated request methods now include command/file/permission approvals, MCP elicitation, tool user input, dynamic tool calls, auth refresh, and attestation. `jarvisctl` records these as blocked operator events, mirrors them into the durable operator-request queue, and waits up to 12 hours for an Obsidian or CLI response before returning a JSON-RPC timeout.
- Historical thread data is read with `thread/read` and `includeTurns`; each returned turn carries `itemsView`.
- `thread/goal/set`, `thread/goal/updated`, and `thread/goal/cleared` are first-class goal lifecycle events.
- Live threads pick up config changes without a restart, so Jarvis should keep durable launch state in tickets and runtime metadata instead of restarting sessions to refresh every Codex config change.

## Ticket frontmatter

Core launch fields already supported:

```yaml
codex_driver: app-server
codex_model: gpt-5.4
codex_reasoning_effort: high
codex_reasoning_summary: concise
codex_sandbox_mode: workspace-write
codex_approval_policy: on-request
codex_search: true
codex_add_dirs:
  - /home/rootster/codex
```

New app-server protocol fields:

```yaml
codex_model_provider: openai
codex_personality: pragmatic
codex_service_name: jarvisctl
codex_service_tier: default
codex_approvals_reviewer: user
codex_thread_source: user
codex_session_start_source: startup
codex_ephemeral: false
codex_developer_instructions: "Keep ticket progress updated in the vault."
codex_base_instructions: "Use the ticket as the execution contract."
codex_permission_profile: ":workspace"
codex_permission_additional_writable_roots:
  - /home/rootster/codex
codex_environments:
  - local
codex_goal: "Implement the ticket completely and validate it."
codex_goal_token_budget: 200000
codex_memory_mode: enabled
codex_enable_features:
  - remote-control
codex_disable_features:
  - apps
```

Escape hatches for fields that appear in newer app-server builds before Jarvis gets typed support:

```yaml
codex_app_thread_config:
  sessionStartSource: clear
codex_app_turn_config:
  outputSchema:
    type: object
```

`codex_app_thread_config` merges into `thread/start` or `thread/resume`; `codex_app_turn_config` merges into `turn/start`. Explicit escape-hatch values win over typed Jarvis fields.

## Runtime metadata

The Obsidian plugin can read `jarvisctl list --json` and use:

- `context.codex_settings` for launch/runtime settings.
- `context.codex_features` and `context.codex_disabled_features` for feature toggles.
- `context.codex_environments` for selected environment ids.
- `context.goal_objective` and `context.goal_status` for active goal state.
- `context.memory_mode` for memory mode.
- `context.remote_control_status` and `context.remote_environment_id` for remote-control state.
- `context.recent_events` for streamed goal, remote-control, assistant, command, plan, and subagent activity.

## Not mapped yet

- Historical thread reads are exposed through `jarvisctl history --namespace <name> --json`. The Obsidian plugin can use this for compact turn history without parsing transcript files.
- Approval and elicitation server requests are also surfaced through `jarvisctl operator-request list` and the Obsidian Mission Chain operator-request card. A linked request can be resolved from the dashboard or with `jarvisctl operator-request resolve`, which also responds to the waiting app-server request when the namespace/request id is still live.
- App/plugin/skill mention inputs are passed through prompt text today. A richer Obsidian composer can add structured `skill`, `mention`, and `localImage` input items later.
