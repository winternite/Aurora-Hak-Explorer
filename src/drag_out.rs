//! Native X11 file drag source used by the AppImage build.
//!
//! eframe/winit receives file drops but does not currently expose an outgoing
//! data-drag API on Linux.  This small XDND source owns a helper X11 window,
//! advertises `text/uri-list`, and serves the exported temporary files when a
//! desktop or file manager requests them.

use std::{
    collections::VecDeque,
    fs,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::{
        Once, OnceLock,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const NO_POINTER_POSITION: i64 = i64::MIN;
static POINTER_POSITION: AtomicI64 = AtomicI64::new(NO_POINTER_POSITION);

pub fn pointer_position() -> Option<(i32, i32)> {
    let packed = POINTER_POSITION.load(Ordering::Relaxed);
    (packed != NO_POINTER_POSITION).then_some(((packed >> 32) as i32, packed as i32))
}

fn set_pointer_position(x: i32, y: i32) {
    POINTER_POSITION.store(((x as i64) << 32) | y as u32 as i64, Ordering::Relaxed);
}

use crate::archive::{self, Entry};
use tempfile::TempDir;
use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ButtonReleaseEvent, ChangeWindowAttributesAux, ClientMessageData,
            ClientMessageEvent, ConnectionExt, CreateWindowAux, Cursor, EventMask, GrabMode,
            GrabStatus, PropMode, Property, PropertyNotifyEvent, SELECTION_NOTIFY_EVENT,
            SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

#[derive(Clone, Copy)]
struct Atoms {
    xdnd_aware: Atom,
    xdnd_enter: Atom,
    xdnd_leave: Atom,
    xdnd_position: Atom,
    xdnd_status: Atom,
    xdnd_drop: Atom,
    xdnd_finished: Atom,
    xdnd_selection: Atom,
    xdnd_action_copy: Atom,
    text_uri_list: Atom,
    targets: Atom,
    utf8_string: Atom,
    incr: Atom,
    ark_service: Atom,
    ark_path: Atom,
}

impl Atoms {
    fn new(connection: &RustConnection) -> Result<Self, String> {
        Ok(Self {
            xdnd_aware: atom(connection, b"XdndAware")?,
            xdnd_enter: atom(connection, b"XdndEnter")?,
            xdnd_leave: atom(connection, b"XdndLeave")?,
            xdnd_position: atom(connection, b"XdndPosition")?,
            xdnd_status: atom(connection, b"XdndStatus")?,
            xdnd_drop: atom(connection, b"XdndDrop")?,
            xdnd_finished: atom(connection, b"XdndFinished")?,
            xdnd_selection: atom(connection, b"XdndSelection")?,
            xdnd_action_copy: atom(connection, b"XdndActionCopy")?,
            text_uri_list: atom(connection, b"text/uri-list")?,
            targets: atom(connection, b"TARGETS")?,
            utf8_string: atom(connection, b"UTF8_STRING")?,
            incr: atom(connection, b"INCR")?,
            ark_service: atom(connection, b"application/x-kde-ark-dndextract-service")?,
            ark_path: atom(connection, b"application/x-kde-ark-dndextract-path")?,
        })
    }
}

#[derive(Clone)]
pub struct ArchiveExtractOffer {
    pub service: String,
    pub path: String,
}

fn atom(connection: &RustConnection, name: &[u8]) -> Result<Atom, String> {
    connection
        .intern_atom(false, name)
        .map_err(|error| error.to_string())?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| error.to_string())
}

/// Release winit's implicit pointer grab so the helper XDND connection can
/// take ownership of the remainder of the current mouse drag.
pub fn release_pointer_grab(frame: &eframe::Frame) {
    use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

    let Ok(display_handle) = frame.display_handle() else {
        return;
    };
    let RawDisplayHandle::Xlib(handle) = display_handle.as_raw() else {
        return;
    };
    let Some(display) = handle.display else {
        return;
    };
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
        return;
    };
    // Use winit's own Xlib connection: implicit pointer grabs are scoped to
    // the X client connection that received the original button press.
    unsafe {
        let display = display.as_ptr().cast::<x11_dl::xlib::Display>();
        (xlib.XUngrabPointer)(display, CURRENT_TIME.into());
        (xlib.XFlush)(display);
    }
}

pub fn start(
    _frame: &eframe::Frame,
    paths: Vec<PathBuf>,
    resources: Vec<(Entry, PathBuf)>,
    temporary_directory: TempDir,
    archive_offer: Option<ArchiveExtractOffer>,
) {
    cleanup_abandoned_drag_directories();
    let file_count = paths.len();
    thread::spawn(move || {
        if let Err(error) = run(paths, resources, archive_offer) {
            eprintln!("Could not start outgoing file drag: {error}");
        } else {
            // File managers commonly acknowledge XDND before their copy job
            // opens the source URI. Keep the exported files alive while that
            // asynchronous job starts. A single bounded cleanup worker owns
            // successful exports instead of leaving one sleeping thread per drag.
            retain_temporary_directory(temporary_directory, file_count);
            return;
        }
        drop(temporary_directory);
    });
}

fn cleanup_abandoned_drag_directories() {
    const MINIMUM_AGE: Duration = Duration::from_secs(12 * 60 * 60);
    static CLEANUP: Once = Once::new();
    CLEANUP.call_once(|| {
        let temporary_root = std::env::temp_dir();
        let Ok(entries) = fs::read_dir(&temporary_root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.as_bytes().starts_with(b"ahe-drag-") {
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

fn retain_temporary_directory(directory: TempDir, file_count: usize) {
    const MAX_RETAINED_EXPORTS: usize = 4;
    static CLEANUP: OnceLock<mpsc::Sender<(TempDir, Duration)>> = OnceLock::new();
    let sender = CLEANUP.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<(TempDir, Duration)>();
        thread::spawn(move || {
            let mut retained = VecDeque::<(Instant, TempDir)>::new();
            loop {
                let timeout = retained
                    .front()
                    .map(|(deadline, _)| deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_secs(3600));
                match receiver.recv_timeout(timeout) {
                    Ok((directory, retention)) => {
                        retained.push_back((Instant::now() + retention, directory));
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
    let retention = drag_retention(file_count);
    crate::drag_cleanup::register(directory.path(), retention);
    if let Err(error) = sender.send((directory, retention)) {
        drop(error.0.0);
    }
}

fn drag_retention(_file_count: usize) -> Duration {
    // Some file managers acknowledge XDND before their asynchronous copy job
    // has opened every source URI. Keep completed drags available for a short,
    // fixed grace period; the durable helper guarantees cleanup after exit.
    Duration::from_secs(60)
}

const INCR_THRESHOLD_BYTES: usize = 256 * 1024;
const INCR_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct IncrementalTransfer {
    requestor: Window,
    property: Atom,
    target: Atom,
    offset: usize,
}

struct LazyStaging {
    resources: Option<Vec<(Entry, PathBuf)>>,
}

impl LazyStaging {
    fn new(resources: Vec<(Entry, PathBuf)>) -> Self {
        Self {
            resources: Some(resources),
        }
    }

    fn ensure_ready(&mut self) -> Result<(), String> {
        let Some(resources) = self.resources.take() else {
            return Ok(());
        };
        archive::export_entries_parallel(&resources).map_err(|error| error.to_string())
    }
}

fn run(
    paths: Vec<PathBuf>,
    resources: Vec<(Entry, PathBuf)>,
    archive_offer: Option<ArchiveExtractOffer>,
) -> Result<(), String> {
    POINTER_POSITION.store(NO_POINTER_POSITION, Ordering::Relaxed);
    let (connection, screen_number) = x11rb::connect(None).map_err(|error| error.to_string())?;
    let root = connection.setup().roots[screen_number].root;
    let source = connection
        .generate_id()
        .map_err(|error| error.to_string())?;
    let atoms = Atoms::new(&connection)?;
    // winit does not expose an outgoing Linux drag API, so it also cannot
    // provide the drag-image/copy-badge normally shown by a toolkit.  While
    // our helper owns the pointer, use the desktop's themed copy cursor.
    // `dnd-copy` is the standard name used by KDE cursor themes; `copy` is a
    // useful fallback for themes that use the older name.
    let drag_cursor = load_drag_cursor(&connection, screen_number);

    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            source,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(
                EventMask::BUTTON_RELEASE
                    | EventMask::BUTTON_MOTION
                    | EventMask::POINTER_MOTION
                    | EventMask::PROPERTY_CHANGE,
            ),
        )
        .map_err(|error| error.to_string())?;
    connection
        .change_property32(
            PropMode::REPLACE,
            source,
            atoms.xdnd_aware,
            AtomEnum::ATOM,
            &[5],
        )
        .map_err(|error| error.to_string())?;
    connection
        .set_selection_owner(source, atoms.xdnd_selection, CURRENT_TIME)
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())?;

    let mut grabbed = false;
    for _ in 0..20 {
        let status = connection
            .grab_pointer(
                false,
                root,
                EventMask::BUTTON_RELEASE | EventMask::BUTTON_MOTION | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                drag_cursor.unwrap_or(NONE),
                CURRENT_TIME,
            )
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?
            .status;
        if status == GrabStatus::SUCCESS {
            grabbed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if !grabbed {
        return Err("the desktop would not release the pointer grab".into());
    }

    let uri_list = uri_list(&paths);
    let mut staging = LazyStaging::new(resources);
    let mut target = NONE;
    let mut accepted = false;
    let mut last_time = CURRENT_TIME;
    let mut transfers = Vec::<IncrementalTransfer>::new();

    loop {
        let event = connection
            .wait_for_event()
            .map_err(|error| error.to_string())?;
        match event {
            Event::MotionNotify(event) => {
                set_pointer_position(event.root_x.into(), event.root_y.into());
                last_time = event.time;
                let next = find_target(&connection, root, atoms.xdnd_aware)?;
                if next != target {
                    if target != NONE {
                        send_message(&connection, target, atoms.xdnd_leave, [source, 0, 0, 0, 0])?;
                    }
                    target = next;
                    accepted = false;
                    if target != NONE {
                        let advertised_types = archive_offer.as_ref().map_or(
                            [source, 5 << 24, atoms.text_uri_list, 0, 0],
                            |_| {
                                [
                                    source,
                                    5 << 24,
                                    atoms.ark_service,
                                    atoms.ark_path,
                                    atoms.text_uri_list,
                                ]
                            },
                        );
                        send_message(&connection, target, atoms.xdnd_enter, advertised_types)?;
                    }
                }
                if target != NONE {
                    let coordinates =
                        ((event.root_x as u16 as u32) << 16) | event.root_y as u16 as u32;
                    send_message(
                        &connection,
                        target,
                        atoms.xdnd_position,
                        [source, 0, coordinates, event.time, atoms.xdnd_action_copy],
                    )?;
                }
                connection.flush().map_err(|error| error.to_string())?;
            }
            Event::ClientMessage(event) if event.type_ == atoms.xdnd_status => {
                let data = event.data.as_data32();
                accepted = data[1] & 1 != 0;
            }
            Event::ButtonRelease(event) => {
                finish_drag(
                    &connection,
                    source,
                    target,
                    accepted,
                    event,
                    last_time,
                    atoms,
                    &uri_list,
                    archive_offer.as_ref(),
                    &mut staging,
                    transfers,
                )?;
                break;
            }
            Event::SelectionRequest(event) => {
                if let Some(transfer) = serve_selection(
                    &connection,
                    event,
                    atoms,
                    &uri_list,
                    archive_offer.as_ref(),
                    &mut staging,
                )? {
                    replace_transfer(&mut transfers, transfer);
                }
            }
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                advance_incremental_transfer(&connection, event, &uri_list, &mut transfers)?;
            }
            _ => {}
        }
    }

    let _ = connection.ungrab_pointer(CURRENT_TIME);
    let _ = connection.set_selection_owner(NONE, atoms.xdnd_selection, CURRENT_TIME);
    let _ = connection.destroy_window(source);
    if let Some(cursor) = drag_cursor {
        let _ = connection.free_cursor(cursor);
    }
    let _ = connection.flush();
    Ok(())
}

fn load_drag_cursor(connection: &RustConnection, screen_number: usize) -> Option<Cursor> {
    let database = x11rb::resource_manager::new_from_default(connection).ok()?;
    let handle = x11rb::cursor::Handle::new(connection, screen_number, &database)
        .ok()?
        .reply()
        .ok()?;
    ["dnd-copy", "copy"].into_iter().find_map(|name| {
        handle
            .load_cursor(connection, name)
            .ok()
            .filter(|cursor| *cursor != NONE)
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_drag(
    connection: &RustConnection,
    source: Window,
    target: Window,
    accepted: bool,
    event: ButtonReleaseEvent,
    last_time: u32,
    atoms: Atoms,
    uri_list: &[u8],
    archive_offer: Option<&ArchiveExtractOffer>,
    staging: &mut LazyStaging,
    mut transfers: Vec<IncrementalTransfer>,
) -> Result<(), String> {
    if target == NONE || !accepted {
        if target != NONE {
            send_message(connection, target, atoms.xdnd_leave, [source, 0, 0, 0, 0])?;
        }
        return Ok(());
    }
    let time = if event.time == CURRENT_TIME {
        last_time
    } else {
        event.time
    };
    send_message(connection, target, atoms.xdnd_drop, [source, 0, time, 0, 0])?;
    connection.flush().map_err(|error| error.to_string())?;

    let timeout = drop_timeout(uri_list.len());
    let deadline = Instant::now() + timeout;
    let mut target_finished = false;
    while Instant::now() < deadline {
        match connection
            .poll_for_event()
            .map_err(|error| error.to_string())?
        {
            Some(Event::SelectionRequest(event)) => {
                if let Some(transfer) =
                    serve_selection(connection, event, atoms, uri_list, archive_offer, staging)?
                {
                    replace_transfer(&mut transfers, transfer);
                }
            }
            Some(Event::PropertyNotify(event)) if event.state == Property::DELETE => {
                advance_incremental_transfer(connection, event, uri_list, &mut transfers)?;
            }
            Some(Event::ClientMessage(event)) if event.type_ == atoms.xdnd_finished => {
                target_finished = true;
                if transfers.is_empty() {
                    break;
                }
            }
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(5)),
        }
        if target_finished && transfers.is_empty() {
            break;
        }
    }
    Ok(())
}

fn replace_transfer(transfers: &mut Vec<IncrementalTransfer>, transfer: IncrementalTransfer) {
    transfers.retain(|existing| {
        existing.requestor != transfer.requestor || existing.property != transfer.property
    });
    transfers.push(transfer);
}

fn drop_timeout(uri_list_bytes: usize) -> Duration {
    // Cross-toolkit XWayland/Wayland drops can take a long time to parse tens
    // of thousands of URLs. Scale beyond one minute for large lists, while
    // retaining an upper bound for broken or abandoned targets.
    const MIN_SECONDS: u64 = 60;
    const MAX_SECONDS: u64 = 15 * 60;
    let extra = u64::try_from(uri_list_bytes / (32 * 1024)).unwrap_or(u64::MAX);
    Duration::from_secs(MIN_SECONDS.saturating_add(extra).min(MAX_SECONDS))
}

fn serve_selection(
    connection: &RustConnection,
    event: SelectionRequestEvent,
    atoms: Atoms,
    uri_list: &[u8],
    archive_offer: Option<&ArchiveExtractOffer>,
    staging: &mut LazyStaging,
) -> Result<Option<IncrementalTransfer>, String> {
    let property = if event.property == NONE {
        event.target
    } else {
        event.property
    };
    let mut transfer = None;
    let supported = if event.target == atoms.targets {
        let mut targets = vec![
            atoms.targets,
            atoms.text_uri_list,
            atoms.utf8_string,
            AtomEnum::STRING.into(),
        ];
        if archive_offer.is_some() {
            targets.push(atoms.ark_service);
            targets.push(atoms.ark_path);
        }
        connection
            .change_property32(
                PropMode::REPLACE,
                event.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )
            .map_err(|error| error.to_string())?;
        true
    } else if event.target == atoms.ark_service {
        let Some(offer) = archive_offer else {
            return notify_unsupported_selection(connection, event, property);
        };
        connection
            .change_property8(
                PropMode::REPLACE,
                event.requestor,
                property,
                event.target,
                offer.service.as_bytes(),
            )
            .map_err(|error| error.to_string())?;
        true
    } else if event.target == atoms.ark_path {
        let Some(offer) = archive_offer else {
            return notify_unsupported_selection(connection, event, property);
        };
        connection
            .change_property8(
                PropMode::REPLACE,
                event.requestor,
                property,
                event.target,
                offer.path.as_bytes(),
            )
            .map_err(|error| error.to_string())?;
        true
    } else if event.target == atoms.text_uri_list
        || event.target == atoms.utf8_string
        || event.target == AtomEnum::STRING.into()
    {
        staging.ensure_ready()?;
        if uri_list.len() >= INCR_THRESHOLD_BYTES {
            connection
                .change_window_attributes(
                    event.requestor,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )
                .map_err(|error| error.to_string())?;
            let byte_count = u32::try_from(uri_list.len())
                .map_err(|_| "the outgoing drag URI list is too large".to_owned())?;
            connection
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    atoms.incr,
                    &[byte_count],
                )
                .map_err(|error| error.to_string())?;
            transfer = Some(IncrementalTransfer {
                requestor: event.requestor,
                property,
                target: event.target,
                offset: 0,
            });
        } else {
            connection
                .change_property8(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    event.target,
                    uri_list,
                )
                .map_err(|error| error.to_string())?;
        }
        true
    } else {
        false
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: event.time,
        requestor: event.requestor,
        selection: event.selection,
        target: event.target,
        property: if supported { property } else { NONE },
    };
    connection
        .send_event(false, event.requestor, EventMask::NO_EVENT, notify)
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())?;
    Ok(transfer)
}

fn notify_unsupported_selection(
    connection: &RustConnection,
    event: SelectionRequestEvent,
    _property: Atom,
) -> Result<Option<IncrementalTransfer>, String> {
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: event.time,
        requestor: event.requestor,
        selection: event.selection,
        target: event.target,
        property: NONE,
    };
    connection
        .send_event(false, event.requestor, EventMask::NO_EVENT, notify)
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())?;
    Ok(None)
}

fn advance_incremental_transfer(
    connection: &RustConnection,
    event: PropertyNotifyEvent,
    uri_list: &[u8],
    transfers: &mut Vec<IncrementalTransfer>,
) -> Result<(), String> {
    let Some(index) = transfers
        .iter()
        .position(|transfer| transfer.requestor == event.window && transfer.property == event.atom)
    else {
        return Ok(());
    };

    let transfer = &mut transfers[index];
    if transfer.offset < uri_list.len() {
        let end = transfer
            .offset
            .saturating_add(INCR_CHUNK_BYTES)
            .min(uri_list.len());
        connection
            .change_property8(
                PropMode::REPLACE,
                transfer.requestor,
                transfer.property,
                transfer.target,
                &uri_list[transfer.offset..end],
            )
            .map_err(|error| error.to_string())?;
        transfer.offset = end;
    } else {
        // ICCCM terminates an INCR transfer with one zero-length property.
        connection
            .change_property8(
                PropMode::REPLACE,
                transfer.requestor,
                transfer.property,
                transfer.target,
                &[],
            )
            .map_err(|error| error.to_string())?;
        transfers.swap_remove(index);
    }
    connection.flush().map_err(|error| error.to_string())?;
    Ok(())
}

fn find_target(
    connection: &RustConnection,
    root: Window,
    xdnd_aware: Atom,
) -> Result<Window, String> {
    let mut window = root;
    let mut chain = Vec::new();
    loop {
        let pointer = connection
            .query_pointer(window)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        if pointer.child == NONE || pointer.child == window {
            break;
        }
        window = pointer.child;
        chain.push(window);
    }
    for candidate in chain.into_iter().rev() {
        let property = connection
            .get_property(false, candidate, xdnd_aware, AtomEnum::ATOM, 0, 1)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        if property.format == 32 && property.value_len > 0 {
            return Ok(candidate);
        }
    }
    Ok(NONE)
}

fn send_message(
    connection: &RustConnection,
    destination: Window,
    message_type: Atom,
    data: [u32; 5],
) -> Result<(), String> {
    let event =
        ClientMessageEvent::new(32, destination, message_type, ClientMessageData::from(data));
    connection
        .send_event(false, destination, EventMask::NO_EVENT, event)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut result = String::new();
    for path in paths {
        result.push_str("file://");
        for &byte in path.as_os_str().as_bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
                result.push(byte as char);
            } else {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
        result.push_str("\r\n");
    }
    result.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_uri_lists_use_incremental_transfer() {
        let paths = (0..500_000)
            .map(|index| PathBuf::from(format!("/tmp/ahe-drag-test/resource_{index:06}.mdl")))
            .collect::<Vec<_>>();
        let payload = uri_list(&paths);
        assert!(payload.len() >= INCR_THRESHOLD_BYTES);
        assert!(payload.len() < u32::MAX as usize);
        assert!(drop_timeout(payload.len()) > Duration::from_secs(60));
    }

    #[test]
    fn completed_drags_use_a_one_minute_retention() {
        assert_eq!(drag_retention(1), Duration::from_secs(60));
        assert_eq!(drag_retention(500_000), Duration::from_secs(60));
    }

    #[test]
    fn generic_uri_drag_stages_resources_only_when_requested() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let source_path = source.path().join("resource.txt");
        let staged_path = destination.path().join("resource.txt");
        fs::write(&source_path, b"lazy export").unwrap();
        let archive = crate::archive::Archive::new(
            crate::archive::ArchiveKind::Hak,
            crate::archive::ArchiveVersion::V1_0,
        );
        let entry = archive.prepare_incoming_file(&source_path).unwrap();
        let mut staging = LazyStaging::new(vec![(entry, staged_path.clone())]);
        assert!(!staged_path.exists());
        staging.ensure_ready().unwrap();
        assert_eq!(fs::read(staged_path).unwrap(), b"lazy export");
    }
}
