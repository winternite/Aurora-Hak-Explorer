//! KDE's archive-extraction drag protocol.
//!
//! KIO deliberately avoids receiving enormous `text/uri-list` payloads from
//! archive applications.  Ark instead advertises a small D-Bus endpoint and
//! KIO asks that endpoint to extract the current selection into the drop
//! directory.  Offering the same protocol keeps very large AHE drags reliable
//! while the ordinary URI-list target remains available to other desktops.

use std::{
    path::Path,
    sync::{Arc, RwLock},
};

use crate::archive::Entry;

const OBJECT_PATH: &str = "/DndExtract/1";

#[derive(Clone, Default)]
struct ExtractService {
    entries: Arc<RwLock<Vec<Entry>>>,
}

#[zbus::interface(name = "org.kde.ark.DndExtract")]
impl ExtractService {
    #[zbus(name = "extractSelectedFilesTo")]
    fn extract_selected_files_to(&self, destination: &str) -> zbus::fdo::Result<()> {
        let destination = Path::new(destination);
        if !destination.is_dir() {
            return Err(zbus::fdo::Error::Failed(format!(
                "The drop destination is not a local directory: {}",
                destination.display()
            )));
        }

        let entries = self
            .entries
            .read()
            .map_err(|_| zbus::fdo::Error::Failed("The drag selection is unavailable".into()))?
            .clone();
        if entries.is_empty() {
            return Err(zbus::fdo::Error::Failed(
                "The drag selection is empty".into(),
            ));
        }

        let resources = entries
            .into_iter()
            .map(|entry| {
                let filename = entry.safe_filename().map_err(|error| {
                    zbus::fdo::Error::Failed(format!("The resource has no safe filename: {error}"))
                })?;
                Ok((entry, destination.join(filename)))
            })
            .collect::<zbus::fdo::Result<Vec<_>>>()?;
        crate::archive::export_entries_parallel(&resources).map_err(|error| {
            zbus::fdo::Error::Failed(format!(
                "Could not extract the drag selection to {}: {error}",
                destination.display()
            ))
        })?;
        Ok(())
    }
}

pub struct Bridge {
    _connection: zbus::blocking::Connection,
    service: String,
    entries: Arc<RwLock<Vec<Entry>>>,
}

impl Bridge {
    pub fn new() -> Result<Self, String> {
        let connection =
            zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
        let service = connection
            .unique_name()
            .ok_or_else(|| "The D-Bus session did not assign a unique service name".to_owned())?
            .to_string();
        let interface = ExtractService::default();
        let entries = Arc::clone(&interface.entries);
        connection
            .object_server()
            .at(OBJECT_PATH, interface)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            _connection: connection,
            service,
            entries,
        })
    }

    pub fn set_entries(&self, entries: Vec<Entry>) {
        if let Ok(mut current) = self.entries.write() {
            *current = entries;
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn path(&self) -> &'static str {
        OBJECT_PATH
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn kde_extract_request_copies_the_entire_offer() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let first = source.path().join("first.mdl");
        let second = source.path().join("second.dds");
        fs::write(&first, b"model").unwrap();
        fs::write(&second, b"texture").unwrap();

        let bridge = Bridge::new().unwrap();
        let archive = crate::archive::Archive::new(
            crate::archive::ArchiveKind::Hak,
            crate::archive::ArchiveVersion::V1_0,
        );
        bridge.set_entries(vec![
            archive.prepare_incoming_file(&first).unwrap(),
            archive.prepare_incoming_file(&second).unwrap(),
        ]);
        let caller = zbus::blocking::Connection::session().unwrap();
        let proxy = zbus::blocking::Proxy::new(
            &caller,
            bridge.service(),
            bridge.path(),
            "org.kde.ark.DndExtract",
        )
        .unwrap();
        let destination = destination.path().to_string_lossy().into_owned();
        proxy
            .call_method("extractSelectedFilesTo", &(destination.as_str(),))
            .unwrap();

        assert_eq!(
            fs::read(Path::new(&destination).join("first.mdl")).unwrap(),
            b"model"
        );
        assert_eq!(
            fs::read(Path::new(&destination).join("second.dds")).unwrap(),
            b"texture"
        );
    }
}
