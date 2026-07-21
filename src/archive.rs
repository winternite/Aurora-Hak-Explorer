use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use crate::resource_types::{extension_for, type_for};

const HEADER_SIZE: u64 = 160;
const BIF_HEADER_SIZE: u64 = 20;
const BIF_RESOURCE_ENTRY_SIZE: u64 = 16;
const MAX_ARCHIVE_ENTRIES: usize = 1_000_000;
const MAX_LOCALIZED_STRING_TABLE_SIZE: u64 = 64 * 1024 * 1024;
static NEXT_ARCHIVE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveVersion {
    V1_0,
    V1_1,
}

impl ArchiveVersion {
    pub fn label(self) -> &'static str {
        match self {
            Self::V1_0 => "V1.0 (NWN/EE)",
            Self::V1_1 => "V1.1 (NWN2)",
        }
    }
    fn bytes(self) -> &'static [u8; 4] {
        match self {
            Self::V1_0 => b"V1.0",
            Self::V1_1 => b"V1.1",
        }
    }
    fn name_len(self) -> usize {
        match self {
            Self::V1_0 => 16,
            Self::V1_1 => 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveKind {
    Hak,
    Erf,
    Mod,
    Sav,
    Bif,
}

impl ArchiveKind {
    pub fn signature(self) -> &'static [u8; 4] {
        match self {
            Self::Hak => b"HAK ",
            Self::Erf => b"ERF ",
            Self::Mod | Self::Sav => b"MOD ",
            Self::Bif => b"BIFF",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Hak => "hak",
            Self::Erf => "erf",
            Self::Mod => "mod",
            Self::Sav => "sav",
            Self::Bif => "bif",
        }
    }

    pub fn is_editable(self) -> bool {
        self != Self::Bif
    }
}

#[derive(Clone, Debug)]
pub enum EntryData {
    ArchiveSlice {
        path: PathBuf,
        offset: u64,
        size: u64,
    },
    ExternalFile(PathBuf),
    #[allow(dead_code)]
    Memory(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub type_id: u16,
    pub data: EntryData,
    model_compiled: OnceLock<Option<bool>>,
}

impl Entry {
    pub fn extension(&self) -> String {
        extension_for(self.type_id)
    }
    pub fn is_new(&self) -> bool {
        matches!(self.data, EntryData::ExternalFile(_) | EntryData::Memory(_))
    }
    pub fn filename(&self) -> String {
        format!("{}.{}", self.name, self.extension())
    }
    pub fn safe_filename(&self) -> io::Result<String> {
        validate_archive_resource_name(&self.name, usize::MAX)?;
        Ok(self.filename())
    }
    pub fn size(&self) -> io::Result<u64> {
        match &self.data {
            EntryData::ArchiveSlice { size, .. } => Ok(*size),
            EntryData::ExternalFile(path) => Ok(fs::metadata(path)?.len()),
            EntryData::Memory(data) => Ok(data.len() as u64),
        }
    }
    pub fn model_compiled(&self) -> Option<bool> {
        *self.model_compiled.get_or_init(|| {
            if self.extension() != "mdl" {
                return None;
            }
            self.read_prefix(4)
                .ok()
                .filter(|prefix| prefix.len() == 4)
                .map(|prefix| prefix == [0, 0, 0, 0])
        })
    }
    fn copy_to(&self, output: &mut impl Write) -> io::Result<u64> {
        match &self.data {
            EntryData::ArchiveSlice { path, offset, size } => {
                let mut file = BufReader::new(File::open(path)?);
                file.seek(SeekFrom::Start(*offset))?;
                io::copy(&mut file.take(*size), output)
            }
            EntryData::ExternalFile(path) => {
                io::copy(&mut BufReader::new(File::open(path)?), output)
            }
            EntryData::Memory(data) => {
                output.write_all(data)?;
                Ok(data.len() as u64)
            }
        }
    }

    pub fn export_to(&self, output: impl AsRef<Path>) -> io::Result<()> {
        let mut writer = BufWriter::new(File::create(output)?);
        self.copy_to(&mut writer)?;
        writer.flush()
    }

    pub fn read_prefix(&self, limit: u64) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        match &self.data {
            EntryData::ArchiveSlice { path, offset, size } => {
                let mut file = BufReader::new(File::open(path)?);
                file.seek(SeekFrom::Start(*offset))?;
                file.take((*size).min(limit)).read_to_end(&mut data)?;
            }
            EntryData::ExternalFile(path) => {
                BufReader::new(File::open(path)?)
                    .take(limit)
                    .read_to_end(&mut data)?;
            }
            EntryData::Memory(bytes) => {
                data.extend_from_slice(&bytes[..bytes.len().min(limit as usize)])
            }
        }
        Ok(data)
    }
}

#[derive(Clone, Debug)]
struct LocalizedString {
    language_id: u32,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Archive {
    pub kind: ArchiveKind,
    pub version: ArchiveVersion,
    pub path: Option<PathBuf>,
    pub entries: Vec<Entry>,
    localized: Vec<LocalizedString>,
    description_strref: u32,
    identity: u64,
    view_revision: u64,
}

impl Archive {
    pub fn new(kind: ArchiveKind, version: ArchiveVersion) -> Self {
        let description = if kind == ArchiveKind::Hak {
            b"Aurora Hak Explorer\nCreated with AHE".to_vec()
        } else {
            Vec::new()
        };
        Self {
            kind,
            version,
            path: None,
            entries: Vec::new(),
            localized: vec![LocalizedString {
                language_id: 0,
                bytes: description,
            }],
            description_strref: u32::MAX,
            identity: NEXT_ARCHIVE_ID.fetch_add(1, Ordering::Relaxed),
            view_revision: 0,
        }
    }

    pub fn view_key(&self) -> (u64, u64) {
        (self.identity, self.view_revision)
    }

    pub fn version_label(&self) -> &'static str {
        if self.kind == ArchiveKind::Bif {
            "V1 (BioWare BIFF)"
        } else {
            self.version.label()
        }
    }

    pub fn mark_resources_changed(&mut self) {
        self.view_revision = self.view_revision.wrapping_add(1);
    }

    pub fn description(&self) -> String {
        self.localized
            .first()
            .map(|s| String::from_utf8_lossy(&s.bytes).into_owned())
            .unwrap_or_default()
    }

    pub fn set_description(&mut self, text: String) {
        if let Some(first) = self.localized.first_mut() {
            first.bytes = text.into_bytes();
        } else {
            self.localized.push(LocalizedString {
                language_id: 0,
                bytes: text.into_bytes(),
            });
        }
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file_len = fs::metadata(&path)?.len();
        if file_len < 4 {
            return Err(invalid("file is too small to be an NWN archive"));
        }
        let mut input = BufReader::new(File::open(&path)?);
        let mut signature = [0; 4];
        input.read_exact(&mut signature)?;
        if &signature == b"BIFF" {
            return Self::open_bif(path, file_len, input);
        }
        if file_len < HEADER_SIZE {
            return Err(invalid("file is too small to be an ERF archive"));
        }
        let kind = match &signature {
            b"HAK " => ArchiveKind::Hak,
            b"ERF " => ArchiveKind::Erf,
            b"MOD " => {
                if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("sav"))
                {
                    ArchiveKind::Sav
                } else {
                    ArchiveKind::Mod
                }
            }
            _ => return Err(invalid("unsupported archive signature")),
        };
        let mut version_bytes = [0; 4];
        input.read_exact(&mut version_bytes)?;
        let version = match &version_bytes {
            b"V1.0" => ArchiveVersion::V1_0,
            b"V1.1" => ArchiveVersion::V1_1,
            _ => return Err(invalid("unsupported ERF version")),
        };
        let language_count = read_u32(&mut input)? as usize;
        let localized_size = read_u32(&mut input)? as u64;
        let entry_count = read_u32(&mut input)? as usize;
        if localized_size > MAX_LOCALIZED_STRING_TABLE_SIZE {
            return Err(invalid("localized string table is too large"));
        }
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err(invalid(format!(
                "archive contains more than {MAX_ARCHIVE_ENTRIES} resources"
            )));
        }
        let localized_offset = read_u32(&mut input)? as u64;
        let key_offset = read_u32(&mut input)? as u64;
        let resource_offset = read_u32(&mut input)? as u64;
        let _build_year = read_u32(&mut input)?;
        let _build_day = read_u32(&mut input)?;
        let description_strref = read_u32(&mut input)?;

        let name_len = version.name_len();
        check_range(
            localized_offset,
            localized_size,
            file_len,
            "localized strings",
        )?;
        let minimum_localized_size = (language_count as u64)
            .checked_mul(8)
            .ok_or_else(|| invalid("localized string table is too large"))?;
        if minimum_localized_size > localized_size {
            return Err(invalid(
                "localized string count exceeds the localized string table",
            ));
        }
        check_range(
            key_offset,
            (entry_count as u64) * (name_len as u64 + 8),
            file_len,
            "key list",
        )?;
        check_range(
            resource_offset,
            (entry_count as u64) * 8,
            file_len,
            "resource list",
        )?;

        input.seek(SeekFrom::Start(localized_offset))?;
        let mut localized = Vec::new();
        localized
            .try_reserve(language_count)
            .map_err(|_| invalid("localized string table is too large"))?;
        let localized_end = localized_offset + localized_size;
        for index in 0..language_count {
            let language_id = read_u32(&mut input)?;
            let declared_size = read_u32(&mut input)? as u64;
            let string_start = input.stream_position()?;
            let remaining = localized_end.saturating_sub(string_start);
            // Some archives produced by Gareth Hughes' old command-line `erf`
            // utility store the size of the entire localized-string table in
            // the sole string's length field. In that specific layout the
            // declared length is eight bytes too large because it includes the
            // language-id and length fields themselves. Accept only this exact
            // legacy shape; other out-of-range strings remain errors.
            let size = if declared_size > remaining
                && language_count == 1
                && index == 0
                && declared_size == localized_size
                && string_start == localized_offset + 8
            {
                remaining
            } else if declared_size > remaining {
                return Err(invalid("localized string exceeds its table"));
            } else {
                declared_size
            };
            let size =
                usize::try_from(size).map_err(|_| invalid("localized string is too large"))?;
            let mut bytes = vec![0; size];
            input.read_exact(&mut bytes)?;
            localized.push(LocalizedString { language_id, bytes });
        }

        #[derive(Debug)]
        struct Key {
            name: String,
            type_id: u16,
        }
        input.seek(SeekFrom::Start(key_offset))?;
        let mut keys = Vec::new();
        keys.try_reserve(entry_count)
            .map_err(|_| invalid("archive key list is too large"))?;
        for _ in 0..entry_count {
            let mut name_bytes = vec![0; name_len];
            input.read_exact(&mut name_bytes)?;
            let end = name_bytes.iter().position(|b| *b == 0).unwrap_or(name_len);
            let name = String::from_utf8_lossy(&name_bytes[..end])
                .trim_end()
                .to_owned();
            let _resource_id = read_u32(&mut input)?;
            let type_id = read_u16(&mut input)?;
            let _unused = read_u16(&mut input)?;
            if name.is_empty() {
                return Err(invalid("archive contains an empty resource name"));
            }
            validate_archive_resource_name(&name, name_len)?;
            keys.push(Key { name, type_id });
        }

        input.seek(SeekFrom::Start(resource_offset))?;
        let mut entries = Vec::new();
        entries
            .try_reserve(entry_count)
            .map_err(|_| invalid("archive resource list is too large"))?;
        for key in keys {
            let offset = read_u32(&mut input)? as u64;
            let size = read_u32(&mut input)? as u64;
            check_range(offset, size, file_len, "resource data")?;
            entries.push(Entry {
                name: key.name,
                type_id: key.type_id,
                data: EntryData::ArchiveSlice {
                    path: path.clone(),
                    offset,
                    size,
                },
                model_compiled: OnceLock::new(),
            });
        }
        // Populate the model-kind cache using this already-open file. The
        // Resource Tree can then count compiled and uncompiled models without
        // opening the HAK once for every MDL on its first frame.
        if let Some(model_type) = type_for("mdl") {
            for entry in &entries {
                if entry.type_id != model_type {
                    continue;
                }
                let EntryData::ArchiveSlice { offset, size, .. } = &entry.data else {
                    continue;
                };
                let compiled = if *size < 4 {
                    None
                } else {
                    input.seek(SeekFrom::Start(*offset))?;
                    let mut prefix = [0; 4];
                    input.read_exact(&mut prefix)?;
                    Some(prefix == [0, 0, 0, 0])
                };
                let _ = entry.model_compiled.set(compiled);
            }
        }
        entries.sort_by_key(|e| (e.name.to_ascii_lowercase(), e.type_id));
        Ok(Self {
            kind,
            version,
            path: Some(path),
            entries,
            localized,
            description_strref,
            identity: NEXT_ARCHIVE_ID.fetch_add(1, Ordering::Relaxed),
            view_revision: 0,
        })
    }

    fn open_bif(path: PathBuf, file_len: u64, mut input: BufReader<File>) -> io::Result<Self> {
        if file_len < BIF_HEADER_SIZE {
            return Err(invalid("BIF header is truncated"));
        }
        let mut version = [0_u8; 4];
        input.read_exact(&mut version)?;
        if &version != b"V1  " {
            return Err(invalid("unsupported BIF version"));
        }
        let entry_count = read_u32(&mut input)? as usize;
        let _fixed_resource_count = read_u32(&mut input)?;
        let table_offset = read_u32(&mut input)? as u64;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err(invalid(format!(
                "BIF contains more than {MAX_ARCHIVE_ENTRIES} resources"
            )));
        }
        let table_size = (entry_count as u64)
            .checked_mul(BIF_RESOURCE_ENTRY_SIZE)
            .ok_or_else(|| invalid("BIF resource table is too large"))?;
        check_range(table_offset, table_size, file_len, "BIF resource table")?;

        input.seek(SeekFrom::Start(table_offset))?;
        let mut entries = Vec::new();
        entries
            .try_reserve(entry_count)
            .map_err(|_| invalid("BIF resource list is too large"))?;
        for index in 0..entry_count {
            let resource_id = read_u32(&mut input)?;
            let offset = read_u32(&mut input)? as u64;
            let size = read_u32(&mut input)? as u64;
            let raw_type = read_u32(&mut input)?;
            let type_id = u16::try_from(raw_type)
                .map_err(|_| invalid("BIF resource type exceeds the supported 16-bit range"))?;
            if resource_id & 0x000f_ffff != index as u32 {
                return Err(invalid(
                    "BIF resource IDs do not match their table positions",
                ));
            }
            check_range(offset, size, file_len, "BIF resource data")?;
            entries.push(Entry {
                // Directly opened BIFs have no resrefs. Match NWN Explorer's
                // stable synthetic naming scheme so resources can be viewed
                // and extracted without a companion KEY file.
                name: format!("res{index}"),
                type_id,
                data: EntryData::ArchiveSlice {
                    path: path.clone(),
                    offset,
                    size,
                },
                model_compiled: OnceLock::new(),
            });
        }
        populate_model_kind_cache(&mut input, &entries)?;
        entries.sort_by_key(|entry| (entry.name.to_ascii_lowercase(), entry.type_id));
        Ok(Self {
            kind: ArchiveKind::Bif,
            version: ArchiveVersion::V1_0,
            path: Some(path),
            entries,
            localized: Vec::new(),
            description_strref: u32::MAX,
            identity: NEXT_ARCHIVE_ID.fetch_add(1, Ordering::Relaxed),
            view_revision: 0,
        })
    }

    #[cfg(test)]
    pub fn add_file(&mut self, path: impl AsRef<Path>) -> io::Result<bool> {
        let entry = self.entry_from_file(path.as_ref())?;
        let replacement = self.entries.iter().position(|existing| {
            existing.name.eq_ignore_ascii_case(&entry.name) && existing.type_id == entry.type_id
        });
        if let Some(index) = replacement {
            self.entries[index] = entry;
        } else {
            self.entries.push(entry);
        }
        self.entries
            .sort_by_key(|entry| (entry.name.to_ascii_lowercase(), entry.type_id));
        self.mark_resources_changed();
        Ok(replacement.is_some())
    }

    /// Returns the archive key and display filename an incoming resource would use.
    /// This lets the UI build one lookup table for a large import instead of
    /// repeatedly scanning every entry already in the archive.
    pub fn prepare_incoming_file(&self, path: impl AsRef<Path>) -> io::Result<Entry> {
        self.entry_from_file(path.as_ref())
    }

    pub fn incoming_entry_identity(entry: &Entry) -> (String, u16) {
        (entry.name.to_ascii_lowercase(), entry.type_id)
    }

    /// Adds or replaces one resource without sorting or bumping the view
    /// revision. Call `finish_bulk_add` once after the complete batch.
    #[cfg(test)]
    pub fn add_file_unsorted(
        &mut self,
        path: impl AsRef<Path>,
        replacement_index: Option<usize>,
    ) -> io::Result<bool> {
        let entry = self.entry_from_file(path.as_ref())?;
        self.add_prepared_entry_unsorted(entry, replacement_index)
    }

    pub fn add_prepared_entry_unsorted(
        &mut self,
        entry: Entry,
        replacement_index: Option<usize>,
    ) -> io::Result<bool> {
        if let Some(index) = replacement_index {
            let existing = self
                .entries
                .get_mut(index)
                .ok_or_else(|| invalid("bulk import replacement index is out of range"))?;
            if !existing.name.eq_ignore_ascii_case(&entry.name) || existing.type_id != entry.type_id
            {
                return Err(invalid("bulk import replacement no longer matches"));
            }
            *existing = entry;
            Ok(true)
        } else {
            if self.entries.len() >= MAX_ARCHIVE_ENTRIES {
                return Err(invalid(format!(
                    "archive entry count exceeds safety limit ({MAX_ARCHIVE_ENTRIES})"
                )));
            }
            self.entries.push(entry);
            Ok(false)
        }
    }

    pub fn finish_bulk_add(&mut self) {
        self.entries
            .sort_by_key(|entry| (entry.name.to_ascii_lowercase(), entry.type_id));
        self.mark_resources_changed();
    }

    #[cfg(test)]
    pub fn conflicting_filename(&self, path: impl AsRef<Path>) -> io::Result<Option<String>> {
        let incoming = self.entry_from_file(path.as_ref())?;
        Ok(self
            .entries
            .iter()
            .find(|existing| {
                existing.name.eq_ignore_ascii_case(&incoming.name)
                    && existing.type_id == incoming.type_id
            })
            .map(Entry::filename))
    }

    fn entry_from_file(&self, path: &Path) -> io::Result<Entry> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(invalid("resource is not a regular file"));
        }
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("resource filename is not valid UTF-8"))?;
        let (raw_name, extension) = file_name
            .rsplit_once('.')
            .ok_or_else(|| invalid("resource filename has no extension"))?;
        let type_id = type_for(extension)
            .ok_or_else(|| invalid(format!("unknown NWN resource extension: .{extension}")))?;
        let max = self.version.name_len();
        let name = sanitize_name(raw_name, max)?;
        Ok(Entry {
            name,
            type_id,
            data: EntryData::ExternalFile(path.to_path_buf()),
            model_compiled: OnceLock::new(),
        })
    }

    #[cfg(test)]
    pub fn merge(&mut self, other: &Archive) -> (usize, usize) {
        self.merge_entries(&other.entries)
    }

    pub fn merge_entries<'a>(
        &mut self,
        incoming_entries: impl IntoIterator<Item = &'a Entry>,
    ) -> (usize, usize) {
        let mut added = 0;
        let mut replaced = 0;
        for incoming in incoming_entries {
            if let Some(index) = self.entries.iter().position(|e| {
                e.name.eq_ignore_ascii_case(&incoming.name) && e.type_id == incoming.type_id
            }) {
                self.entries[index] = incoming.clone();
                replaced += 1;
            } else {
                self.entries.push(incoming.clone());
                added += 1;
            }
        }
        self.entries
            .sort_by_key(|e| (e.name.to_ascii_lowercase(), e.type_id));
        if added + replaced > 0 {
            self.mark_resources_changed();
        }
        (added, replaced)
    }

    pub fn export_entry(&self, index: usize, output: impl AsRef<Path>) -> io::Result<()> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| invalid("resource index is out of range"))?;
        entry.export_to(output)
    }

    pub fn extract_all(&self, directory: impl AsRef<Path>) -> io::Result<usize> {
        fs::create_dir_all(&directory)?;
        for (index, entry) in self.entries.iter().enumerate() {
            self.export_entry(index, directory.as_ref().join(entry.safe_filename()?))?;
        }
        Ok(self.entries.len())
    }

    pub fn save(&mut self, output: impl AsRef<Path>) -> io::Result<()> {
        if !self.kind.is_editable() {
            return Err(invalid("BIF archives are read-only"));
        }
        let output = output.as_ref();
        let existing_permissions = fs::metadata(output)
            .ok()
            .map(|metadata| metadata.permissions());
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let (mut temp, _recovery_record) = crate::save_cleanup::create_save_file(parent)?;
        self.write_to(temp.as_file_mut())?;
        temp.as_file_mut().sync_all()?;
        temp.persist(output).map_err(|e| e.error)?;
        let _ = sync_directory(parent);
        if let Some(permissions) = existing_permissions {
            fs::set_permissions(output, permissions)?;
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(output, fs::Permissions::from_mode(0o644))?;
            }
        }
        *self = Self::open(output)?;
        Ok(())
    }

    fn write_to(&self, file: &mut File) -> io::Result<()> {
        let count = self.entries.len();
        if count > u32::MAX as usize {
            return Err(invalid("too many resources"));
        }
        let localized_size: u64 = self
            .localized
            .iter()
            .map(|s| 8 + s.bytes.len() as u64)
            .sum();
        let key_offset = HEADER_SIZE
            .checked_add(localized_size)
            .ok_or_else(|| invalid("archive is too large"))?;
        let key_size = (count as u64)
            .checked_mul((self.version.name_len() + 8) as u64)
            .ok_or_else(|| invalid("archive is too large"))?;
        let resource_offset = key_offset
            .checked_add(key_size)
            .ok_or_else(|| invalid("archive is too large"))?;
        let data_offset = resource_offset
            .checked_add((count as u64) * 8)
            .ok_or_else(|| invalid("archive is too large"))?;
        for value in [localized_size, key_offset, resource_offset, data_offset] {
            u32::try_from(value).map_err(|_| invalid("ERF V1.x offsets cannot exceed 4 GiB"))?;
        }

        let mut output = BufWriter::new(file);
        output.write_all(self.kind.signature())?;
        output.write_all(self.version.bytes())?;
        write_u32(&mut output, self.localized.len() as u32)?;
        write_u32(&mut output, localized_size as u32)?;
        write_u32(&mut output, count as u32)?;
        write_u32(&mut output, HEADER_SIZE as u32)?;
        write_u32(&mut output, key_offset as u32)?;
        write_u32(&mut output, resource_offset as u32)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (build_year, build_day) = erf_build_date(now / 86_400);
        write_u32(&mut output, build_year)?;
        write_u32(&mut output, build_day)?;
        write_u32(&mut output, self.description_strref)?;
        output.write_all(&[0; 116])?;
        for string in &self.localized {
            write_u32(&mut output, string.language_id)?;
            write_u32(&mut output, string.bytes.len() as u32)?;
            output.write_all(&string.bytes)?;
        }
        for (id, entry) in self.entries.iter().enumerate() {
            let bytes = entry.name.as_bytes();
            let max = self.version.name_len();
            if bytes.len() > max {
                return Err(invalid(format!(
                    "resource name '{}' exceeds {max} bytes",
                    entry.name
                )));
            }
            output.write_all(bytes)?;
            output.write_all(&vec![0; max - bytes.len()])?;
            write_u32(&mut output, id as u32)?;
            write_u16(&mut output, entry.type_id)?;
            write_u16(&mut output, 0)?;
        }
        let mut offset = data_offset;
        for entry in &self.entries {
            let size = entry.size()?;
            let offset32 =
                u32::try_from(offset).map_err(|_| invalid("ERF V1.x archive exceeds 4 GiB"))?;
            let size32 = u32::try_from(size).map_err(|_| invalid("a resource exceeds 4 GiB"))?;
            write_u32(&mut output, offset32)?;
            write_u32(&mut output, size32)?;
            offset = offset
                .checked_add(size)
                .ok_or_else(|| invalid("archive is too large"))?;
        }
        for entry in &self.entries {
            entry.copy_to(&mut output)?;
        }
        output.flush()
    }
}

pub fn export_entries_parallel(resources: &[(Entry, PathBuf)]) -> io::Result<()> {
    if resources.is_empty() {
        return Ok(());
    }
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
        .min(resources.len());
    if workers == 1 || resources.len() < 128 {
        for (entry, output) in resources {
            entry.export_to(output)?;
        }
        return Ok(());
    }

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !failed.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((entry, output)) = resources.get(index) else {
                        break;
                    };
                    if let Err(error) = entry.export_to(output) {
                        failed.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = first_error.lock()
                            && slot.is_none()
                        {
                            *slot = Some(error);
                        }
                        break;
                    }
                }
            });
        }
    });
    match first_error.into_inner() {
        Ok(Some(error)) => Err(error),
        Ok(None) => Ok(()),
        Err(_) => Err(io::Error::other("parallel export worker failed")),
    }
}

fn populate_model_kind_cache(input: &mut (impl Read + Seek), entries: &[Entry]) -> io::Result<()> {
    let Some(model_type) = type_for("mdl") else {
        return Ok(());
    };
    for entry in entries {
        if entry.type_id != model_type {
            continue;
        }
        let EntryData::ArchiveSlice { offset, size, .. } = &entry.data else {
            continue;
        };
        let compiled = if *size < 4 {
            None
        } else {
            input.seek(SeekFrom::Start(*offset))?;
            let mut prefix = [0; 4];
            input.read_exact(&mut prefix)?;
            Some(prefix == [0, 0, 0, 0])
        };
        let _ = entry.model_compiled.set(compiled);
    }
    Ok(())
}

fn erf_build_date(mut days_since_epoch: u64) -> (u32, u32) {
    let mut year = 1970_u32;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days_since_epoch < days_in_year {
            return (year - 1900, days_since_epoch as u32 + 1);
        }
        days_since_epoch -= days_in_year;
        year += 1;
    }
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn sanitize_name(raw: &str, max: usize) -> io::Result<String> {
    let lowered = raw.to_ascii_lowercase();
    if lowered.is_empty()
        || lowered.len() > max
        || !lowered
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid(format!(
            "resource name must be 1-{max} ASCII letters, digits, or underscores"
        )));
    }
    Ok(lowered)
}

fn validate_archive_resource_name(name: &str, max: usize) -> io::Result<()> {
    if name.is_empty()
        || name.len() > max
        || name
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':'))
    {
        return Err(invalid(
            "archive contains an unsafe resource name with path or control characters",
        ));
    }
    Ok(())
}
fn check_range(offset: u64, size: u64, file_len: u64, label: &str) -> io::Result<()> {
    if offset.checked_add(size).is_none_or(|end| end > file_len) {
        Err(invalid(format!("{label} lies outside the file")))
    } else {
        Ok(())
    }
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn read_u16(input: &mut impl Read) -> io::Result<u16> {
    let mut b = [0; 2];
    input.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    input.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn write_u16(output: &mut impl Write, n: u16) -> io::Result<()> {
    output.write_all(&n.to_le_bytes())
}
fn write_u32(output: &mut impl Write, n: u32) -> io::Result<()> {
    output.write_all(&n.to_le_bytes())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_v10() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("test.hak");
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.entries.push(Entry {
            name: "sample".into(),
            type_id: 0x07e1,
            data: EntryData::Memory(b"2DA V2.0\n".to_vec()),
            model_compiled: OnceLock::new(),
        });
        archive.save(&output).unwrap();
        let loaded = Archive::open(&output).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].filename(), "sample.2da");
        let extracted = dir.path().join("sample.2da");
        loaded.export_entry(0, &extracted).unwrap();
        assert_eq!(fs::read(extracted).unwrap(), b"2DA V2.0\n");
    }

    #[test]
    fn opens_and_extracts_direct_bif_resources_with_nwn_explorer_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.bif");
        let payload = b"2DA V2.0\n";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BIFFV1  ");
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&20_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&36_u32.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0x07e1_u32.to_le_bytes());
        bytes.extend_from_slice(payload);
        fs::write(&path, bytes).unwrap();

        let mut archive = Archive::open(&path).unwrap();
        assert_eq!(archive.kind, ArchiveKind::Bif);
        assert_eq!(archive.entries.len(), 1);
        assert_eq!(archive.entries[0].filename(), "res0.2da");
        let extracted = dir.path().join("res0.2da");
        archive.export_entry(0, &extracted).unwrap();
        assert_eq!(fs::read(extracted).unwrap(), payload);
        assert!(archive.save(dir.path().join("rewritten.bif")).is_err());
    }
    #[test]
    fn rejects_bad_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.hak");
        fs::write(&path, b"HAK V1.0").unwrap();
        assert!(Archive::open(path).is_err());
    }

    #[test]
    fn accepts_legacy_erf_tool_localized_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.hak");
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.set_description("Created by the old erf utility".into());
        archive.save(&path).unwrap();

        // Reproduce the old utility's bug: the string length includes its own
        // eight-byte localized-table record header.
        let mut bytes = fs::read(&path).unwrap();
        let localized_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        bytes[164..168].copy_from_slice(&localized_size.to_le_bytes());
        fs::write(&path, bytes).unwrap();

        let loaded = Archive::open(&path).unwrap();
        assert_eq!(loaded.description(), "Created by the old erf utility");
    }

    #[test]
    fn rejects_localized_count_larger_than_its_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-count.hak");
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.save(&path).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&1_000_000_u32.to_le_bytes());
        fs::write(&path, bytes).unwrap();

        let error = Archive::open(path).unwrap_err();
        assert!(error.to_string().contains("count exceeds"));
    }

    #[test]
    fn rejects_unsafe_resource_names_before_they_can_escape_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsafe-name.hak");
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.entries.push(Entry {
            name: "sample".into(),
            type_id: 0x000a,
            data: EntryData::Memory(b"test".to_vec()),
            model_compiled: OnceLock::new(),
        });
        archive.save(&path).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        let key_offset = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;
        bytes[key_offset..key_offset + 16].fill(0);
        bytes[key_offset..key_offset + 9].copy_from_slice(b"../escape");
        fs::write(&path, bytes).unwrap();

        let error = Archive::open(path).unwrap_err();
        assert!(error.to_string().contains("unsafe resource name"));
    }

    #[test]
    fn extraction_rechecks_names_and_preserves_real_world_compatibility() {
        for name in ["Metal weathered", "cav_copper-01", "cat_range_+1"] {
            validate_archive_resource_name(name, 16).unwrap();
        }
        for name in ["../escape", r"C:\escape", "bad:name", "line\nbreak"] {
            assert!(validate_archive_resource_name(name, 16).is_err());
        }

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.entries.push(Entry {
            name: "../escape".into(),
            type_id: 0x000a,
            data: EntryData::Memory(b"must not escape".to_vec()),
            model_compiled: OnceLock::new(),
        });
        assert!(archive.extract_all(&output).is_err());
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[test]
    fn rejects_impractical_resource_counts_before_allocating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too-many.hak");
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.save(&path).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        bytes[16..20].copy_from_slice(&((MAX_ARCHIVE_ENTRIES as u32) + 1).to_le_bytes());
        fs::write(&path, bytes).unwrap();

        let error = Archive::open(path).unwrap_err();
        assert!(error.to_string().contains("more than"));
    }

    #[test]
    fn erf_build_dates_observe_leap_years() {
        assert_eq!(erf_build_date(0), (70, 1));
        assert_eq!(erf_build_date(365 + 365), (72, 1));
        assert_eq!(erf_build_date(365 + 365 + 365), (72, 366));
        assert_eq!(erf_build_date(365 + 365 + 366), (73, 1));
    }

    #[test]
    fn adds_common_and_enhanced_edition_resources() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        for (name, expected_type) in [
            ("model.mdl", 0x07d2),
            ("texture.dds", 0x07f1),
            ("table.2da", 0x07e1),
            ("material.mtr", 0x0818),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, b"test").unwrap();
            archive.add_file(path).unwrap();
            assert!(
                archive
                    .entries
                    .iter()
                    .any(|entry| entry.type_id == expected_type)
            );
        }
        assert_eq!(
            archive
                .conflicting_filename(dir.path().join("model.mdl"))
                .unwrap()
                .as_deref(),
            Some("model.mdl")
        );
        let different_type = dir.path().join("model.dds");
        fs::write(&different_type, b"test").unwrap();
        assert_eq!(archive.conflicting_filename(different_type).unwrap(), None);
    }

    #[test]
    fn resource_view_revision_tracks_entry_changes() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("sample.mdl");
        fs::write(&model, b"newmodel sample\nsetsupermodel sample NULL\n").unwrap();

        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let (identity, revision) = archive.view_key();
        archive.add_file(&model).unwrap();
        assert_eq!(archive.view_key(), (identity, revision + 1));

        fs::write(&model, b"newmodel sample\nsetsupermodel sample parent\n").unwrap();
        assert!(archive.add_file(&model).unwrap());
        assert_eq!(archive.view_key(), (identity, revision + 2));

        let mut other = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let texture = dir.path().join("sample.dds");
        fs::write(&texture, b"DDS test").unwrap();
        other.add_file(&texture).unwrap();
        assert_eq!(archive.merge(&other), (1, 0));
        assert_eq!(archive.view_key(), (identity, revision + 3));
    }

    #[test]
    fn bulk_add_defers_sorting_and_revision_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let second = dir.path().join("second.txt");
        let first = dir.path().join("first.txt");
        fs::write(&second, b"second").unwrap();
        fs::write(&first, b"first").unwrap();

        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let initial_key = archive.view_key();
        assert!(!archive.add_file_unsorted(&second, None).unwrap());
        assert!(!archive.add_file_unsorted(&first, None).unwrap());
        assert_eq!(archive.entries[0].name, "second");
        assert_eq!(archive.view_key(), initial_key);

        archive.finish_bulk_add();
        assert_eq!(archive.entries[0].name, "first");
        assert_eq!(archive.view_key(), (initial_key.0, initial_key.1 + 1));

        fs::write(&first, b"replacement").unwrap();
        assert!(archive.add_file_unsorted(&first, Some(0)).unwrap());
        archive.finish_bulk_add();
        assert_eq!(archive.entries[0].read_prefix(64).unwrap(), b"replacement");
    }

    #[test]
    fn parallel_export_writes_every_resource() {
        let output = tempfile::tempdir().unwrap();
        let resources = (0_usize..256)
            .map(|index| {
                let entry = Entry {
                    name: format!("item{index}"),
                    type_id: 0x000a,
                    data: EntryData::Memory(index.to_le_bytes().to_vec()),
                    model_compiled: OnceLock::new(),
                };
                let path = output.path().join(format!("item{index}.txt"));
                (entry, path)
            })
            .collect::<Vec<_>>();
        export_entries_parallel(&resources).unwrap();
        for (index, (_, path)) in resources.iter().enumerate() {
            assert_eq!(fs::read(path).unwrap(), index.to_le_bytes());
        }
    }
}
