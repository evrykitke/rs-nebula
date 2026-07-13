//! Logging bootstrap built on `tracing`.
//!
//! The filter comes from `logging.level` in configuration, extended by the
//! per-area overrides `logging.http` (request tracing) and
//! `logging.database` (SQL statements) so operators never need to know
//! internal target names. A `RUST_LOG` environment variable, when present,
//! takes precedence over all of it so developers can turn diagnostics up
//! without touching config files.
//!
//! Logs always go to the console. When `logging.file` is set they are also
//! appended to that file (without ANSI colour), rolled over at
//! `logging.max_file_bytes` so a long-running server never fills the disk.

use crate::config::{LogFormat, LoggingConfig};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::prelude::*;

/// Errors raised while initializing logging.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("invalid logging filter {directive:?}: {source}")]
    InvalidFilter {
        directive: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    #[error("could not open log file {path:?}: {source}")]
    LogFile {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("logging already initialized")]
    AlreadyInitialized,
}

/// Install the global tracing subscriber according to configuration.
/// Call once at boot (the kernel does this); a second call fails.
pub fn init(config: &LoggingConfig) -> Result<(), LoggingError> {
    let directive = std::env::var("RUST_LOG").unwrap_or_else(|_| directives(config));
    let filter = EnvFilter::try_new(&directive).map_err(|source| LoggingError::InvalidFilter {
        directive: directive.clone(),
        source,
    })?;

    // Console plus, when configured, a size-rolled file. Layers are boxed
    // so the per-format branches share one type.
    let mut layers = Vec::new();
    layers.push(fmt_layer(config.format, io::stdout, true));

    if !config.file.is_empty() {
        let file = RotatingFile::open(&config.file, config.max_file_bytes).map_err(|source| {
            LoggingError::LogFile {
                path: config.file.clone(),
                source,
            }
        })?;
        let writer = FileWriter(Arc::new(Mutex::new(file)));
        layers.push(fmt_layer(config.format, writer, false));
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(layers)
        .try_init()
        .map_err(|_| LoggingError::AlreadyInitialized)
}

/// The base level plus the per-area overrides, as one filter directive.
/// The overrides map onto the targets that actually emit those logs:
/// request tracing lives in this crate's `web::trace` module (plus
/// tower-http's own layer), SQL statements come from sqlx/SeaORM.
fn directives(config: &LoggingConfig) -> String {
    let mut directive = config.level.clone();
    if !config.http.is_empty() {
        let level = &config.http;
        directive.push_str(&format!(",nebula::web::trace={level},tower_http={level}"));
    }
    if !config.database.is_empty() {
        let level = &config.database;
        directive.push_str(&format!(",sqlx={level},sea_orm={level}"));
    }
    directive
}

/// One formatting layer over a writer, in the configured format. ANSI
/// colour is left on for the console and off for files.
fn fmt_layer<S, W>(
    format: LogFormat,
    writer: W,
    ansi: bool,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .boxed(),
    }
}

/// A log file that rolls itself over once it passes a size threshold: the
/// current file is moved to `<path>.1` (replacing any earlier roll) and a
/// fresh file is opened. Keeps one previous file, so disk use is bounded
/// at roughly twice the threshold.
struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    written: u64,
    file: Option<File>,
}

impl RotatingFile {
    fn open(path: impl Into<PathBuf>, max_bytes: u64) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            // A zero threshold would rotate on every write; treat it as
            // "never roll" rather than thrash.
            max_bytes: max_bytes.max(1),
            written,
            file: Some(file),
        })
    }

    /// Roll over when the next write would exceed the threshold, but never
    /// on an empty file (a single record larger than the threshold still
    /// has to land somewhere).
    fn roll_if_needed(&mut self, incoming: usize) -> io::Result<()> {
        if self.written == 0 || self.written + incoming as u64 <= self.max_bytes {
            return Ok(());
        }
        // Close the current file before renaming — Windows will not rename
        // an open handle.
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }
        let backup = self.backup_path();
        let _ = fs::remove_file(&backup);
        fs::rename(&self.path, &backup)?;
        self.file = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
        self.written = 0;
        Ok(())
    }

    fn backup_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }
}

/// Remove ANSI escape sequences. The file layer formats without colour,
/// but span fields are formatted once by whichever layer records them
/// first — the coloured console layer — and cached, so colour codes can
/// still reach this writer. A log file must stay grep-able.
fn strip_ansi(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut bytes = buf.iter().copied().peekable();
    while let Some(b) = bytes.next() {
        if b != 0x1b {
            out.push(b);
            continue;
        }
        // CSI sequence: `ESC [` then parameter bytes until a final byte
        // in `@`..`~`. A bare ESC is dropped either way.
        if bytes.peek() == Some(&b'[') {
            bytes.next();
            while let Some(n) = bytes.next() {
                if (0x40..=0x7e).contains(&n) {
                    break;
                }
            }
        }
    }
    out
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let clean = strip_ansi(buf);
        self.roll_if_needed(clean.len())?;
        let Some(file) = self.file.as_mut() else {
            return Ok(buf.len());
        };
        file.write_all(&clean)?;
        self.written += clean.len() as u64;
        // The caller's buffer was consumed in full even though fewer
        // bytes landed on disk.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// Makes the shared rotating file usable as a tracing writer: each write
/// takes the lock, so records never interleave.
#[derive(Clone)]
struct FileWriter(Arc<Mutex<RotatingFile>>);

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriterGuard(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

struct FileWriterGuard<'a>(MutexGuard<'a, RotatingFile>);

impl Write for FileWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_area_overrides_extend_the_base_directive() {
        let config = LoggingConfig {
            http: "debug".into(),
            database: "debug".into(),
            ..LoggingConfig::default()
        };
        assert_eq!(
            directives(&config),
            "info,nebula::web::trace=debug,tower_http=debug,sqlx=debug,sea_orm=debug"
        );
        assert_eq!(directives(&LoggingConfig::default()), "info");
    }

    #[test]
    fn ansi_sequences_never_reach_the_file() {
        let dir = std::env::temp_dir().join(format!("nebula-log-{}", uuid::Uuid::new_v4()));
        let path = dir.join("app.log");

        let mut file = RotatingFile::open(&path, 1024).expect("open");
        file.write_all(b"request{\x1b[3mmethod\x1b[0m\x1b[2m=\x1b[0mGET}: done\n")
            .expect("write");
        file.flush().expect("flush");
        assert_eq!(fs::read_to_string(&path).unwrap(), "request{method=GET}: done\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rolls_over_at_the_threshold_keeping_one_backup() {
        let dir = std::env::temp_dir().join(format!("nebula-log-{}", uuid::Uuid::new_v4()));
        let path = dir.join("app.log");
        let backup = dir.join("app.log.1");

        // Threshold 10 bytes: the first line fits, the second rolls it over.
        let mut file = RotatingFile::open(&path, 10).expect("open");
        file.write_all(b"first\n").expect("write 1"); // 6 bytes
        file.write_all(b"second\n").expect("write 2"); // would exceed 10 -> roll
        file.flush().expect("flush");

        assert_eq!(fs::read_to_string(&backup).unwrap(), "first\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");

        // A subsequent write rolls again, and the backup is replaced (only
        // ever one previous file is kept).
        file.write_all(b"third-and-more\n").expect("write 3");
        file.flush().expect("flush");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "second\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_record_still_lands_in_a_fresh_file() {
        let dir = std::env::temp_dir().join(format!("nebula-log-{}", uuid::Uuid::new_v4()));
        let path = dir.join("app.log");

        let mut file = RotatingFile::open(&path, 4).expect("open");
        // Larger than the threshold, but the file is empty: it must not be
        // dropped.
        file.write_all(b"a-very-long-line\n").expect("write");
        file.flush().expect("flush");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a-very-long-line\n");

        let _ = fs::remove_dir_all(&dir);
    }
}
