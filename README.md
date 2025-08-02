<img width="73" height="25" alt="image" src="https://github.com/user-attachments/assets/7a880a0a-7ad9-4e8f-a8ac-08931f53089d" />


# jarvisctl

> Enterprise-grade orchestrator for CLI/TUI worker apps using tmux

`jarvisctl` is a powerful orchestration tool designed to run, inspect, and control CLI or TUI applications in isolated `tmux`-based namespaces. It allows you to launch multiple interactive agents, inspect their processes, inject commands, and exec into their environments for debugging.

---

## Features

* **Namespaces**: Isolated `tmux` sessions for agent groups
* **Agents**: Each agent runs in a dedicated `tmux` window
* **Process Inspection**: Query live data (CPU, memory, status, etc.) by name or PID
* **Namespace Shell Access**: Use `nsenter` to exec into target process namespace
* **Structured Logging**: Enable `RUST_LOG=info` or `debug` for detailed logs
* **Agent Command Injection**: Paste text/scripts into running agents via `tmux send-keys`
* **Attach/Exec**: Attach to full namespace or specific agent window
* **Clean Deletion**: Gracefully shut down sessions via `jarvisctl delete`

---

## Install

```bash
cargo install --path .
```

---

## Usage Examples

### Launch a new namespace with multiple agents

```bash
jarvisctl run --namespace botfarm --agents 2 --working-directory /home/rootster/Pictures -- codex --full-auto
```

### Attach to the full namespace

```bash
jarvisctl attach --namespace botfarm
```

### Exec into a single agent window

```bash
jarvisctl exec --namespace botfarm --agent agent0
```

### Send a file to an agent (one line per command)

```bash
jarvisctl tell --namespace botfarm --agent agent0 --file prompt.md 
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

* `tmux` is used for spawning and managing agents via sessions/windows
* `@jarvisctl=1` is tagged on sessions for filtering
* Process info is gathered using `sysinfo`
* Shell access via `nsenter -t PID -a /bin/bash` (requires `sudo`)
* Command injection uses `tmux send-keys` with `Ctrl+J` for newline

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
