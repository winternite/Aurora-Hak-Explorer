use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const HELPER_ARGUMENT: &str = "--drag-cleanup-helper";
pub const UNCONFIRMED_DRAG_RETENTION: Duration = Duration::from_secs(15 * 60);
const ABANDONED_DRAG_RETENTION: Duration = Duration::from_secs(15 * 60);

struct HelperProcess {
    _child: Child,
    stdin: ChildStdin,
}

static HELPER: OnceLock<Mutex<Option<HelperProcess>>> = OnceLock::new();

pub fn register(path: &Path, retention: Duration) {
    if !is_safe_temp_directory(path) {
        eprintln!(
            "Refusing to register an unsafe temporary cleanup path: {}",
            path.display()
        );
        return;
    }
    let deadline = SystemTime::now()
        .checked_add(retention)
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let record = format!("{deadline}\t{}\n", path.display());
    let helper = HELPER.get_or_init(|| Mutex::new(None));
    let Ok(mut helper) = helper.lock() else {
        return;
    };
    let needs_restart = helper
        .as_mut()
        .is_none_or(|process| process.stdin.write_all(record.as_bytes()).is_err());
    if needs_restart {
        *helper = spawn_helper().ok();
        if let Some(process) = helper.as_mut() {
            let _ = process.stdin.write_all(record.as_bytes());
            let _ = process.stdin.flush();
        }
    } else if let Some(process) = helper.as_mut() {
        let _ = process.stdin.flush();
    }
}

pub fn recover_abandoned() {
    let temporary_root = std::env::temp_dir();
    let Ok(entries) = fs::read_dir(&temporary_root) else {
        return;
    };
    let now = SystemTime::now();
    let mut pending = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_recoverable_drag_directory(&path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let remaining = metadata
            .modified()
            .map(|modified| remaining_abandoned_retention(modified, now))
            .unwrap_or(ABANDONED_DRAG_RETENTION);
        if remaining.is_zero() {
            remove_safe_temp_directory(&path);
        } else {
            register(&path, remaining);
            pending.push((path, remaining));
        }
    }
    schedule_in_process(pending);
}

fn remaining_abandoned_retention(modified: SystemTime, now: SystemTime) -> Duration {
    let age = now.duration_since(modified).unwrap_or_default();
    ABANDONED_DRAG_RETENTION.saturating_sub(age)
}

fn schedule_in_process(pending: Vec<(PathBuf, Duration)>) {
    if pending.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut pending = pending
            .into_iter()
            .map(|(path, delay)| (started + delay, path))
            .collect::<Vec<_>>();
        pending.sort_by_key(|(deadline, _)| *deadline);
        for (deadline, path) in pending {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            remove_safe_temp_directory(&path);
        }
    });
}

fn spawn_helper() -> io::Result<HelperProcess> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg(HELPER_ARGUMENT)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("cleanup helper has no input pipe"))?;
    Ok(HelperProcess {
        _child: child,
        stdin,
    })
}

pub fn run_helper() -> io::Result<()> {
    process_records(BufReader::new(io::stdin()))
}

fn process_records(reader: impl BufRead) -> io::Result<()> {
    let mut deadlines = BTreeMap::<PathBuf, SystemTime>::new();
    for line in reader.lines() {
        let line = line?;
        let Some((deadline, path)) = line.split_once('\t') else {
            continue;
        };
        let Ok(deadline) = deadline.parse::<u128>() else {
            continue;
        };
        let Ok(deadline) = u64::try_from(deadline) else {
            continue;
        };
        let path = PathBuf::from(path);
        if !is_safe_temp_directory(&path) {
            continue;
        }
        deadlines.insert(path, UNIX_EPOCH + Duration::from_millis(deadline));
    }

    let mut deadlines = deadlines.into_iter().collect::<Vec<_>>();
    deadlines.sort_by_key(|(_, deadline)| *deadline);
    for (path, deadline) in deadlines {
        if let Ok(delay) = deadline.duration_since(SystemTime::now()) {
            std::thread::sleep(delay);
        }
        remove_safe_temp_directory(&path);
    }
    Ok(())
}

fn is_safe_temp_directory(path: &Path) -> bool {
    path.is_absolute()
        && path.parent() == Some(std::env::temp_dir().as_path())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                [
                    "ahe-drag-",
                    "ahe-clipboard-",
                    "ahe-model-compiler-",
                    "ahe-model-compile-",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix) && name.len() > prefix.len())
            })
}

fn is_recoverable_drag_directory(path: &Path) -> bool {
    is_safe_temp_directory(path)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ahe-drag-"))
}

fn remove_safe_temp_directory(path: &Path) {
    if !is_safe_temp_directory(path) {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_removes_only_registered_drag_directories() {
        let drag = tempfile::Builder::new()
            .prefix("ahe-drag-")
            .tempdir()
            .unwrap();
        let drag_path = drag.path().to_path_buf();
        fs::write(drag_path.join("resource.txt"), b"temporary").unwrap();
        let record = format!("0\t{}\n", drag_path.display());
        process_records(BufReader::new(record.as_bytes())).unwrap();
        assert!(!drag_path.exists());

        let unrelated = tempfile::tempdir().unwrap();
        let unrelated_path = unrelated.path().to_path_buf();
        let record = format!("0\t{}\n", unrelated_path.display());
        process_records(BufReader::new(record.as_bytes())).unwrap();
        assert!(unrelated_path.exists());
    }

    #[test]
    fn startup_recovery_filters_names_and_calculates_remaining_age() {
        let drag = tempfile::Builder::new()
            .prefix("ahe-drag-")
            .tempdir()
            .unwrap();
        let clipboard = tempfile::Builder::new()
            .prefix("ahe-clipboard-")
            .tempdir()
            .unwrap();
        assert!(is_recoverable_drag_directory(drag.path()));
        assert!(!is_recoverable_drag_directory(clipboard.path()));

        let now = SystemTime::now();
        assert_eq!(
            remaining_abandoned_retention(now - Duration::from_secs(5 * 60), now),
            Duration::from_secs(10 * 60)
        );
        assert!(remaining_abandoned_retention(now - Duration::from_secs(16 * 60), now).is_zero());
    }

    #[test]
    fn startup_recovery_timer_removes_a_scheduled_directory() {
        let drag = tempfile::Builder::new()
            .prefix("ahe-drag-")
            .tempdir()
            .unwrap();
        let path = drag.path().to_path_buf();
        schedule_in_process(vec![(path.clone(), Duration::from_millis(10))]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!path.exists());
    }
}
