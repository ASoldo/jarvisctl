use clap::{Parser, Subcommand};
use std::ffi::OsStr;
use sysinfo::{Pid, System};

/// CLI tool to inspect and control workers (sysinfo 0.36.1)
#[derive(Parser, Debug)]
#[command(name = "jarvisctl")]
#[command(about = "Control and inspect worker CLI apps like codex")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect running processes
    Inspect {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short, long)]
        pid: Option<u32>,
        #[arg(long)]
        exec_shell: bool,
    },

    /// Run a worker in a new tmux namespace
    Run {
        #[arg(long)]
        namespace: String,
        #[arg(long, default_value_t = 1)]
        agents: usize,
        #[arg(required = true)]
        command: Vec<String>,
    },

    /// Attach to a running tmux namespace
    Attach {
        #[arg(long)]
        namespace: String,
    },

    /// Kill a tmux namespace by name
    Delete {
        #[arg(long)]
        namespace: String,
    },

    /// List all running namespaces (tmux sessions) and agents (windows)
    List {
        #[arg(long)]
        namespace: Option<String>,
    },

    /// Attach to a specific agent in a namespace
    Exec {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        agent: String,
    },

    /// Send file or text to a running agent's TUI
    Tell {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        file: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect {
            name,
            pid,
            exec_shell,
        } => {
            let mut sys = System::new_all();
            sys.refresh_all();

            match (name, pid) {
                (Some(name), _) => {
                    for process in sys.processes_by_name(OsStr::new(&name)) {
                        print_process_info(process);
                        if exec_shell {
                            exec_into_process(process.pid().as_u32());
                        }
                    }
                }
                (None, Some(pid_u32)) => {
                    let pid = Pid::from(pid_u32 as usize);
                    if let Some(process) = sys.process(pid) {
                        print_process_info(process);
                        if exec_shell {
                            exec_into_process(process.pid().as_u32());
                        }
                    }
                }
                _ => println!("⚠️ Please provide either --name or --pid. Use --help for details."),
            }
        }

        Command::Run {
            namespace,
            agents,
            command,
        } => {
            for i in 0..agents {
                let window_name = format!("agent{}", i);
                let joined_command = shell_words::join(command.clone());
                let full_command = format!("bash -c '{}'", joined_command);

                let args = if i == 0 {
                    vec![
                        "new-session",
                        "-d",
                        "-s",
                        &namespace,
                        "-n",
                        &window_name,
                        &full_command,
                    ]
                } else {
                    vec![
                        "new-window",
                        "-t",
                        &namespace,
                        "-n",
                        &window_name,
                        &full_command,
                    ]
                };
                let status = std::process::Command::new("tmux")
                    .args(&args)
                    .status()
                    .expect("failed to start tmux window");
                if !status.success() {
                    eprintln!("Failed to start window '{}'", window_name);
                }
            }
            println!(
                "Started {} agent(s) in namespace '{}'\nTo attach: jarvisctl attach --namespace {}",
                agents, namespace, namespace
            );
        }

        Command::Attach { namespace } => {
            let status = std::process::Command::new("tmux")
                .args(&["attach", "-t", &namespace])
                .status()
                .expect("failed to attach to tmux session");
            if !status.success() {
                eprintln!("Failed to attach to namespace");
            }
        }

        Command::Delete { namespace } => {
            let status = std::process::Command::new("tmux")
                .args(&["kill-session", "-t", &namespace])
                .status()
                .expect("failed to delete tmux session");
            if status.success() {
                println!("Deleted namespace '{}'", namespace);
            } else {
                eprintln!("Failed to delete namespace '{}'", namespace);
            }
        }

        Command::List { namespace } => {
            if let Some(ns) = namespace {
                let out = std::process::Command::new("tmux")
                    .args(&["list-windows", "-t", &ns])
                    .output()
                    .expect("failed to list tmux windows");
                println!("AGENTS in NAMESPACE '{}':", ns);
                print!("{}", String::from_utf8_lossy(&out.stdout));
            } else {
                let out = std::process::Command::new("tmux")
                    .arg("list-sessions")
                    .output()
                    .expect("failed to list tmux sessions");
                println!("NAMESPACES (tmux sessions):");
                print!("{}", String::from_utf8_lossy(&out.stdout));

                let out = std::process::Command::new("tmux")
                    .args(&["list-windows", "-a"])
                    .output()
                    .expect("failed to list tmux windows");
                println!("AGENTS (windows in namespaces):");
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
        }

        Command::Exec { namespace, agent } => {
            let status = std::process::Command::new("tmux")
                .args(&["select-window", "-t", &format!("{}:{}", namespace, agent)])
                .status()
                .expect("failed to select tmux window");
            if status.success() {
                let status = std::process::Command::new("tmux")
                    .args(&["attach", "-t", &namespace])
                    .status()
                    .expect("failed to attach to tmux session");
                if !status.success() {
                    eprintln!("Failed to attach to namespace");
                }
            } else {
                eprintln!("Failed to select agent '{}:{}'", namespace, agent);
            }
        }

        Command::Tell {
            namespace,
            agent,
            file,
        } => {
            let contents = std::fs::read_to_string(&file).expect("failed to read input file");
            for line in contents.lines() {
                let status = std::process::Command::new("tmux")
                    .args(&[
                        "send-keys",
                        "-t",
                        &format!("{}:{}", namespace, agent),
                        line,
                        "Enter",
                    ])
                    .status()
                    .expect("failed to send keys to tmux");
                if !status.success() {
                    eprintln!("Failed to send to {}:{}", namespace, agent);
                }
            }
            println!(
                "Sent file '{}' to namespace '{}' agent '{}'",
                file, namespace, agent
            );
        }
    }
}

fn exec_into_process(pid: u32) {
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let status = std::process::Command::new("sudo")
        .arg("nsenter")
        .arg("-t")
        .arg(pid.to_string())
        .arg("-a")
        .arg(shell)
        .status()
        .expect("failed to exec nsenter");
    std::process::exit(status.code().unwrap_or(1));
}

fn print_process_info(process: &sysinfo::Process) {
    println!("PID:             {}", process.pid());
    println!("Name:            {}", process.name().to_string_lossy());
    println!("Status:          {:?}", process.status());
    println!("CPU usage:       {:.2}%", process.cpu_usage());
    println!("Memory (RSS):    {} KB", process.memory());
    println!("Virtual memory:  {} KB", process.virtual_memory());
    println!("Start time (s):  {}", process.start_time());
    println!("Run time (s):    {}", process.run_time());
    println!(
        "Executable path: {}",
        process
            .exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    );
    println!("Command line:    {:?}", process.cmd());
    println!("Parent PID:      {:?}", process.parent());
    println!("----------------------------------------");
}
