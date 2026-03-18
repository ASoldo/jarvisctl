use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{BufReader, Read};
use std::sync::{Arc, Mutex};
use std::thread;

/// Holds agent process output and identifier.
pub struct Agent {
    pub name: String,
    pub output: Arc<Mutex<Vec<String>>>, // rolling log buffer
}

/// Spawn a process inside a pseudo-terminal, capturing its output asynchronously.
pub fn spawn_agent(name: &str, command: &[String]) -> anyhow::Result<Agent> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(&command[0]);
    if command.len() > 1 {
        cmd.args(&command[1..]);
    }

    let _child = pair.slave.spawn_command(cmd)?;
    let mut reader = BufReader::new(pair.master.try_clone_reader()?);
    let output = Arc::new(Mutex::new(Vec::new())); // ← Vec<String>
    let out_clone = Arc::clone(&output);

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }

            let chunk = String::from_utf8_lossy(&buf[..n]);
            let lines = chunk.lines().map(|s| s.to_string());

            let mut out = out_clone.lock().unwrap();
            out.extend(lines);

            // Limit buffer to last 1000 lines
            if out.len() > 1000 {
                let excess = out.len() - 1000;
                out.drain(0..excess);
            }
        }
    });

    Ok(Agent {
        name: name.to_string(),
        output,
    })
}
