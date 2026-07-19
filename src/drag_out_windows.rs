//! Native Windows file drag source.

use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Once, OnceLock,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

const NO_POINTER_POSITION: i64 = i64::MIN;
static POINTER_POSITION: AtomicI64 = AtomicI64::new(NO_POINTER_POSITION);

pub fn pointer_position() -> Option<(i32, i32)> {
    let packed = POINTER_POSITION.load(Ordering::Relaxed);
    (packed != NO_POINTER_POSITION).then_some(((packed >> 32) as i32, packed as i32))
}

pub fn release_pointer_grab(_frame: &eframe::Frame) {
    // Windows OLE takes over pointer tracking when start_drag is called.
}

pub fn start(frame: &eframe::Frame, paths: Vec<PathBuf>, temporary_directory: TempDir) {
    cleanup_abandoned_drag_directories();
    let Some(preview) = paths.first().cloned() else {
        return;
    };
    POINTER_POSITION.store(NO_POINTER_POSITION, Ordering::Relaxed);
    let result = drag::start_drag(
        frame,
        drag::DragItem::Files(paths),
        drag::Image::File(preview),
        |_result, position| {
            POINTER_POSITION.store(
                ((position.x as i64) << 32) | position.y as u32 as i64,
                Ordering::Relaxed,
            );
        },
        drag::Options::default(),
    );
    if let Err(error) = result {
        eprintln!("Could not start outgoing file drag: {error}");
    }

    // Explorer can begin its copy job just after the OLE drag loop returns.
    // A single bounded cleanup worker keeps recent exports alive without
    // creating one sleeping thread for every drag operation.
    retain_temporary_directory(temporary_directory);
}

fn cleanup_abandoned_drag_directories() {
    const MINIMUM_AGE: Duration = Duration::from_secs(12 * 60 * 60);
    static CLEANUP: Once = Once::new();
    CLEANUP.call_once(|| {
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("ahe-drag-") {
                continue;
            }
            let old_enough = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                .is_ok_and(|age| age >= MINIMUM_AGE);
            if old_enough {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    });
}

fn retain_temporary_directory(directory: TempDir) {
    const RETENTION: Duration = Duration::from_secs(60);
    const MAX_RETAINED_EXPORTS: usize = 4;
    static CLEANUP: OnceLock<mpsc::Sender<TempDir>> = OnceLock::new();
    let sender = CLEANUP.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<TempDir>();
        thread::spawn(move || {
            let mut retained = VecDeque::<(Instant, TempDir)>::new();
            loop {
                let timeout = retained
                    .front()
                    .map(|(deadline, _)| deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_secs(3600));
                match receiver.recv_timeout(timeout) {
                    Ok(directory) => {
                        retained.push_back((Instant::now() + RETENTION, directory));
                        while retained.len() > MAX_RETAINED_EXPORTS {
                            retained.pop_front();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let now = Instant::now();
                        while retained
                            .front()
                            .is_some_and(|(deadline, _)| *deadline <= now)
                        {
                            retained.pop_front();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        sender
    });
    crate::drag_cleanup::register(directory.path(), RETENTION);
    if let Err(error) = sender.send(directory) {
        drop(error.0);
    }
}
