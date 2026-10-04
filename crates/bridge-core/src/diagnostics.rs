use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Small, bounded diagnostic history shared by the UI and worker threads.
pub struct Diagnostics {
    entries: Mutex<VecDeque<String>>,
    writer: std::sync::mpsc::SyncSender<String>,
    path: Option<PathBuf>,
}

impl Diagnostics {
    pub fn new(name: &str) -> Self {
        let path = dirs::config_dir().map(|base| {
            base.join("airbridge")
                .join("logs")
                .join(format!("{name}.log"))
        });
        let file = path.as_ref().and_then(|path| {
            std::fs::create_dir_all(path.parent()?).ok()?;
            if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 5_000_000) {
                let _ = std::fs::rename(path, path.with_extension("log.1"));
            }
            OpenOptions::new().create(true).append(true).open(path).ok()
        });
        let (writer, receiver) = std::sync::mpsc::sync_channel::<String>(200);
        let _ = std::thread::Builder::new()
            .name("airbridge-log-writer".into())
            .spawn(move || {
                let mut file = file;
                while let Ok(line) = receiver.recv() {
                    eprintln!("{line}");
                    if let Some(file) = file.as_mut() {
                        let _ = writeln!(file, "{line}");
                    }
                }
            });
        Self {
            entries: Mutex::new(VecDeque::new()),
            writer,
            path,
        }
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    pub fn record(&self, source: &str, message: impl AsRef<str>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let time = now.as_secs() % 86_400;
        let line = format!(
            "{:02}:{:02}:{:02} [{source}] {}",
            time / 3_600,
            (time / 60) % 60,
            time % 60,
            message.as_ref()
        );
        let _ = self.writer.try_send(line.clone());
        if let Ok(mut entries) = self.entries.lock() {
            entries.push_back(line);
            if entries.len() > 200 {
                entries.pop_front();
            }
        }
    }

    pub fn recent(&self) -> Vec<String> {
        self.entries
            .lock()
            .map(|entries| entries.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new("airbridge")
    }
}
