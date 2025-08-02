<img width="135" height="25" alt="image" src="https://github.com/user-attachments/assets/53d4fea9-319d-48a3-a7aa-975e59a02855" />

# jarvisctl

`jarvisctl` is a CLI orchestrator for running, inspecting, and controlling CLI/TUI worker apps (such as Codex or LLMs) using **tmux** as a backend for process/session management. It gives you Kubernetes-inspired workflows for DevOps and agent management, locally.

---

## Features

* **Namespaces**: Use tmux sessions to isolate and manage groups of agents.
* **Agents**: Each agent is a tmux window within a session (namespace), running your CLI app.
* **Inspect**: View detailed process info for workers by name or PID.
* **Attach**: Attach to a whole namespace (tmux session) or a specific agent (window).
* **Tell**: Send (paste) file content to a running agent as input, as if you typed it.
* **Delete**: Cleanly remove (kill) a whole namespace (session).
* **List**: See all running namespaces and agents.

---

## Requirements

* [tmux](https://github.com/tmux/tmux)
* Rust (for building; uses `clap`, `sysinfo`, and `shell-words` crates)

---

## Installation

1. Install dependencies:

   ```sh
   sudo pacman -S tmux   # or: sudo apt install tmux
   cargo install shell-words
   cargo install sysinfo
   cargo install clap
   ```
2. Build the project:

   ```sh
   cargo build --release
   ```

   The binary will be in `target/release/jarvisctl`.

---

## Usage

### Start a namespace with N agents

```sh
jarvisctl run --namespace codexbots --agents 4 codex --full-auto
```

This starts 4 agent windows in the namespace `codexbots`.

### List all namespaces and agents

```sh
jarvisctl list
```

### List agents in a specific namespace

```sh
jarvisctl list --namespace codexbots
```

### Attach to a namespace (full tmux session)

```sh
jarvisctl attach --namespace codexbots
```

### Attach to a specific agent (window) in a namespace

```sh
jarvisctl exec --namespace codexbots --agent agent1
```

### Send a file as input to an agent ("tell")

```sh
jarvisctl tell --namespace codexbots --agent agent0 --file prompt.md
```

This will paste each line of `prompt.md` into the TUI of the running agent.

### Delete a namespace

```sh
jarvisctl delete --namespace codexbots
```

### Inspect running processes by name or PID

```sh
jarvisctl inspect --name codex
jarvisctl inspect --pid 12345
```

* Add `--exec-shell` to enter the process namespace via nsenter:

  ```sh
  jarvisctl inspect --name codex --exec-shell
  ```

---

## Concepts

* **Namespace**: A tmux session (like a Kubernetes namespace or OpenShift project).
* **Agent**: A tmux window within a session (like a pod/worker).
* **Tell**: Inject text or files directly into a running agent's TUI, like piping commands.
* **Attach**: Attach to the whole namespace/session.
* **Exec**: Attach to a specific agent window.

---

## Limitations

* **Hard dependency on tmux**: jarvisctl requires tmux for all orchestration.
* **Agent process duplication**: Some CLI apps (e.g., Codex) may spawn two processes per agent (parent and child); this is normal.
* **No true multiplexing**: jarvisctl does not reimplement tmux; it leverages tmux for all session/window handling.

---

## Example Workflows

**Start workers:**

```sh
jarvisctl run --namespace agents --agents 3 myworker --option foo
```

**List and inspect:**

```sh
jarvisctl list
jarvisctl inspect --name myworker
```

**Send prompt:**

```sh
jarvisctl tell --namespace agents --agent agent1 --file start.md
```

**Attach and manage:**

```sh
jarvisctl exec --namespace agents --agent agent2
```

**Clean up:**

```sh
jarvisctl delete --namespace agents
```

---

## Credits

* Inspired by tmux, kubectl, and CLI DevOps automation.

---

## License

MIT
