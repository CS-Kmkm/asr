//! Reports a failed startup instead of panicking without any visible sign.
//!
//! The app lives in the tray, so a setup failure would otherwise end the
//! process silently at every sign-in. The failure is appended to
//! [`LOG_FILE_NAME`] in the app log directory, a native message box names
//! the log, and the process exits. The box shows only fixed text and the log
//! path; the log holds only error descriptions, never transcript,
//! dictionary, or settings content.

use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use crate::storage::StorageError;

/// File name of the startup failure log inside the app log directory.
pub(crate) const LOG_FILE_NAME: &str = "startup-error.log";
/// A log larger than this is cleared before the next entry, so repeated
/// failed sign-in launches cannot grow it without bound.
const MAX_LOG_BYTES: u64 = 256 * 1024;
const TITLE: &str = "Local Voice Input";

/// Logs `error`, shows a message box naming the log, and exits with code 1.
/// Without `log_dir` (the failure came before the app paths were known) the
/// log is written to the temporary directory.
pub(crate) fn report_and_exit(error: &(dyn Error + 'static), log_dir: Option<&Path>) -> ! {
    let directory = log_dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
    let log_path = write_log(&directory, error).ok();
    let message = message_text(error, log_path.as_deref());
    eprintln!("{message}");
    show_message_box(&message);
    std::process::exit(1);
}

fn summary(error: &(dyn Error + 'static)) -> &'static str {
    let newer_schema = std::iter::successors(Some(error), |&error| error.source()).any(|error| {
        matches!(
            error.downcast_ref(),
            Some(StorageError::IncompatibleSchema { .. })
        )
    });
    if newer_schema {
        "Local Voice Input could not start because its data was saved by a newer version. \
         Use that version to keep using this data."
    } else {
        "Local Voice Input could not start."
    }
}

fn message_text(error: &(dyn Error + 'static), log_path: Option<&Path>) -> String {
    let location = match log_path {
        Some(path) => format!("Details were written to:\n{}", path.display()),
        None => "Details could not be written to a log file.".to_owned(),
    };
    format!("{}\n\n{location}", summary(error))
}

fn write_log(directory: &Path, error: &(dyn Error + 'static)) -> std::io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(LOG_FILE_NAME);
    let oversized = fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES);
    let mut file = OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(&path)?;
    let mut entry = format!(
        "--- startup failed {} ---\n{error}\n",
        chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    );
    for cause in std::iter::successors(error.source(), |&error| error.source()) {
        entry.push_str(&format!("caused by: {cause}\n"));
    }
    file.write_all(entry.as_bytes())?;
    Ok(path)
}

#[cfg(windows)]
fn show_message_box(message: &str) {
    use windows::{
        core::{HSTRING, PCWSTR},
        Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{
                MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
            },
        },
    };
    let text = HSTRING::from(message);
    let caption = HSTRING::from(TITLE);
    // SAFETY: both strings are NUL-terminated and outlive the modal call.
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

#[cfg(not(windows))]
fn show_message_box(_message: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Wrapped(StorageError);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("setup failed")
        }
    }

    impl Error for Wrapped {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn message_names_the_log_and_recognizes_a_newer_database() {
        let generic = StorageError::Lock;
        let message = message_text(&generic, Some(Path::new("C:/logs/startup-error.log")));
        assert!(message.starts_with("Local Voice Input could not start.\n\n"));
        assert!(message.ends_with("C:/logs/startup-error.log"));
        assert!(message_text(&generic, None).contains("could not be written"));

        let newer = Wrapped(StorageError::IncompatibleSchema {
            version: 9,
            min_reader_version: 5,
            supported: 2,
        });
        assert!(message_text(&newer, None).contains("saved by a newer version"));
    }

    #[test]
    fn log_appends_the_error_chain_and_resets_when_oversized() {
        let directory = tempfile::tempdir().unwrap();
        let error = Wrapped(StorageError::Lock);
        let path = write_log(directory.path(), &error).unwrap();
        write_log(directory.path(), &error).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("--- startup failed").count(), 2);
        assert!(text.contains("setup failed\ncaused by: storage lock is unavailable\n"));

        fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        write_log(directory.path(), &error).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("--- startup failed"));
    }
}
