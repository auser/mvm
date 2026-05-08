use tempfile::TempDir;
use tokio::process::Child;

pub struct ProcessHandle {
    pid: u32,

    sandbox_name: String,
    child: Child,
    detached: bool,
    _file_mounts_temp: Option<TempDir>,
}

impl ProcessHandle {
    pub(crate) fn new(
        pid: u32,
        sandbox_name: String,
        child: Child,
        file_mounts_temp: Option<TempDir>,
    ) -> Self {
        Self {
            pid,
            sandbox_name,
            child,
            detached: false,
            _file_mounts_temp: file_mounts_temp,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn sandbox_name(&self) -> &str {
        &self.sandbox_name
    }

    // pub fn kill(&self) -> std::io::Result<()> {

    // }
    //
}
