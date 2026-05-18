use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

static JARVIS_CODEX_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub struct TempJarvisCodexGuard {
    _guard: MutexGuard<'static, ()>,
    original_dir: Option<std::ffi::OsString>,
    root: PathBuf,
}

impl TempJarvisCodexGuard {
    pub fn new(prefix: &str) -> Self {
        let guard = JARVIS_CODEX_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "{}-{}",
            prefix,
            chrono::Utc::now().timestamp_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        let original_dir = std::env::var_os("JARVIS_CODEX_DIR");
        unsafe {
            std::env::set_var("JARVIS_CODEX_DIR", &root);
        }
        Self {
            _guard: guard,
            original_dir,
            root,
        }
    }
}

impl Drop for TempJarvisCodexGuard {
    fn drop(&mut self) {
        match &self.original_dir {
            Some(path) => unsafe {
                std::env::set_var("JARVIS_CODEX_DIR", path);
            },
            None => unsafe {
                std::env::remove_var("JARVIS_CODEX_DIR");
            },
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
