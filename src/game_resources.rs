use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

const KEY_HEADER_SIZE: u64 = 64;
const KEY_FILE_ENTRY_SIZE: u64 = 12;
const KEY_RESOURCE_ENTRY_SIZE: u64 = 22;
const BIF_HEADER_SIZE: u64 = 20;
const BIF_RESOURCE_ENTRY_SIZE: u64 = 16;
const MAX_KEY_FILES: u32 = 100_000;
const MAX_KEY_RESOURCES: u32 = 5_000_000;
const MAX_BIF_RESOURCES: u32 = 5_000_000;
const MAX_MODEL_RESOURCE_SIZE: u64 = 128 * 1024 * 1024;

#[derive(Clone)]
struct ResourceLocation {
    bif_path: PathBuf,
    resource_index: u32,
}

pub struct GameResourceIndex {
    resources: BTreeMap<(String, u16), Vec<ResourceLocation>>,
}

impl GameResourceIndex {
    pub fn build(install_roots: &[PathBuf], resource_type: u16) -> Self {
        let mut key_files = Vec::new();
        let mut seen = BTreeMap::<PathBuf, ()>::new();
        // Resource lookup walks locations in reverse insertion order. Index
        // lower-priority roots first so the user's preferred installation
        // (the first root) wins when multiple installations contain a resref.
        for root in install_roots.iter().rev() {
            let mut root_key_files = Vec::new();
            collect_key_files(root, 0, &mut root_key_files);
            root_key_files.sort();
            root_key_files.dedup();
            for key_file in root_key_files {
                let canonical = fs::canonicalize(&key_file).unwrap_or(key_file);
                if seen.insert(canonical.clone(), ()).is_none() {
                    key_files.push(canonical);
                }
            }
        }
        let mut resources = BTreeMap::<(String, u16), Vec<ResourceLocation>>::new();
        for key_path in key_files {
            let _ = index_key_file(&key_path, resource_type, &mut resources);
        }
        Self { resources }
    }

    pub fn load(&self, name: &str, resource_type: u16) -> io::Result<Option<(Vec<u8>, PathBuf)>> {
        let key = (name.to_ascii_lowercase(), resource_type);
        let Some(locations) = self.resources.get(&key) else {
            return Ok(None);
        };
        for location in locations.iter().rev() {
            if let Ok(bytes) = load_bif_resource(location, resource_type) {
                return Ok(Some((bytes, location.bif_path.clone())));
            }
        }
        Ok(None)
    }
}

fn collect_key_files(directory: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > 4 || !directory.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if path.is_dir() {
            collect_key_files(&path, depth + 1, output);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("key"))
        {
            output.push(path);
        }
    }
}

fn index_key_file(
    key_path: &Path,
    wanted_type: u16,
    output: &mut BTreeMap<(String, u16), Vec<ResourceLocation>>,
) -> io::Result<()> {
    let file_len = fs::metadata(key_path)?.len();
    if file_len < KEY_HEADER_SIZE {
        return Err(invalid("KEY header is truncated"));
    }
    let mut input = BufReader::new(File::open(key_path)?);
    let mut signature = [0_u8; 8];
    input.read_exact(&mut signature)?;
    if &signature[..4] != b"KEY " || &signature[4..] != b"V1  " {
        return Err(invalid("unsupported KEY signature or version"));
    }
    let bif_count = read_u32(&mut input)?;
    let resource_count = read_u32(&mut input)?;
    let file_table_offset = read_u32(&mut input)? as u64;
    let resource_table_offset = read_u32(&mut input)? as u64;
    if bif_count > MAX_KEY_FILES || resource_count > MAX_KEY_RESOURCES {
        return Err(invalid("KEY table count is unreasonable"));
    }
    check_range(
        file_table_offset,
        u64::from(bif_count) * KEY_FILE_ENTRY_SIZE,
        file_len,
        "KEY file table",
    )?;
    check_range(
        resource_table_offset,
        u64::from(resource_count) * KEY_RESOURCE_ENTRY_SIZE,
        file_len,
        "KEY resource table",
    )?;

    input.seek(SeekFrom::Start(file_table_offset))?;
    let mut bif_paths = Vec::with_capacity(bif_count as usize);
    for _ in 0..bif_count {
        let _size = read_u32(&mut input)?;
        let filename_offset = read_u32(&mut input)? as u64;
        let filename_size = u64::from(read_u16(&mut input)?);
        let _drives = read_u16(&mut input)?;
        check_range(filename_offset, filename_size, file_len, "KEY BIF filename")?;
        let return_position = input.stream_position()?;
        input.seek(SeekFrom::Start(filename_offset))?;
        let mut filename = vec![0_u8; filename_size as usize];
        input.read_exact(&mut filename)?;
        input.seek(SeekFrom::Start(return_position))?;
        let end = filename
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(filename.len());
        let relative = String::from_utf8_lossy(&filename[..end]).replace('\\', "/");
        let relative = safe_relative_path(&relative)?;
        let resolved = key_path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take(6)
            .map(|ancestor| ancestor.join(&relative))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                key_path
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or_else(|| key_path.parent().unwrap_or(Path::new(".")))
                    .join(relative)
            });
        bif_paths.push(resolved);
    }

    input.seek(SeekFrom::Start(resource_table_offset))?;
    for _ in 0..resource_count {
        let mut resref = [0_u8; 16];
        input.read_exact(&mut resref)?;
        let resource_type = read_u16(&mut input)?;
        let resource_id = read_u32(&mut input)?;
        if resource_type != wanted_type {
            continue;
        }
        let end = resref
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(resref.len());
        let name = String::from_utf8_lossy(&resref[..end]).to_ascii_lowercase();
        let bif_index = (resource_id >> 20) as usize;
        let Some(bif_path) = bif_paths.get(bif_index) else {
            continue;
        };
        output
            .entry((name, resource_type))
            .or_default()
            .push(ResourceLocation {
                bif_path: bif_path.clone(),
                resource_index: resource_id & 0x000f_ffff,
            });
    }
    Ok(())
}

fn load_bif_resource(location: &ResourceLocation, wanted_type: u16) -> io::Result<Vec<u8>> {
    let file_len = fs::metadata(&location.bif_path)?.len();
    if file_len < BIF_HEADER_SIZE {
        return Err(invalid("BIF header is truncated"));
    }
    let mut input = BufReader::new(File::open(&location.bif_path)?);
    let mut signature = [0_u8; 8];
    input.read_exact(&mut signature)?;
    if &signature[..4] != b"BIFF" || &signature[4..] != b"V1  " {
        return Err(invalid("unsupported BIF signature or version"));
    }
    let resource_count = read_u32(&mut input)?;
    let _fixed_count = read_u32(&mut input)?;
    let table_offset = read_u32(&mut input)? as u64;
    if resource_count > MAX_BIF_RESOURCES {
        return Err(invalid("BIF resource count is unreasonable"));
    }
    check_range(
        table_offset,
        u64::from(resource_count) * BIF_RESOURCE_ENTRY_SIZE,
        file_len,
        "BIF resource table",
    )?;
    if location.resource_index >= resource_count {
        return Err(invalid("BIF resource index is out of range"));
    }
    input.seek(SeekFrom::Start(
        table_offset + u64::from(location.resource_index) * BIF_RESOURCE_ENTRY_SIZE,
    ))?;
    let resource_id = read_u32(&mut input)?;
    let offset = read_u32(&mut input)? as u64;
    let size = read_u32(&mut input)? as u64;
    let resource_type = read_u32(&mut input)?;
    if resource_id & 0x000f_ffff != location.resource_index
        || resource_type != u32::from(wanted_type)
    {
        return Err(invalid("BIF resource table does not match its KEY entry"));
    }
    if size > MAX_MODEL_RESOURCE_SIZE {
        return Err(invalid("BIF model resource exceeds the safe size limit"));
    }
    check_range(offset, size, file_len, "BIF resource data")?;
    let size = usize::try_from(size).map_err(|_| invalid("BIF resource is too large"))?;
    let mut bytes = vec![0_u8; size];
    input.seek(SeekFrom::Start(offset))?;
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn safe_relative_path(value: &str) -> io::Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid("KEY contains an unsafe BIF path"));
    }
    Ok(path.to_path_buf())
}

fn check_range(offset: u64, size: u64, file_len: u64, label: &str) -> io::Result<()> {
    if offset.checked_add(size).is_none_or(|end| end > file_len) {
        return Err(invalid(format!("{label} exceeds the file bounds")));
    }
    Ok(())
}

fn read_u16(input: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    input.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_installation(root: &Path, payload: &[u8]) -> PathBuf {
        let data_directory = root.join("data");
        fs::create_dir_all(&data_directory).unwrap();

        let mut bif = Vec::new();
        bif.extend_from_slice(b"BIFFV1  ");
        bif.extend_from_slice(&1_u32.to_le_bytes());
        bif.extend_from_slice(&0_u32.to_le_bytes());
        bif.extend_from_slice(&20_u32.to_le_bytes());
        bif.extend_from_slice(&0_u32.to_le_bytes());
        bif.extend_from_slice(&36_u32.to_le_bytes());
        bif.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bif.extend_from_slice(&0x07d2_u32.to_le_bytes());
        bif.extend_from_slice(payload);
        fs::write(data_directory.join("test.bif"), &bif).unwrap();

        let filename = b"data/test.bif\0";
        let resource_offset = 64 + 12 + filename.len() as u32;
        let mut key = Vec::new();
        key.extend_from_slice(b"KEY V1  ");
        key.extend_from_slice(&1_u32.to_le_bytes());
        key.extend_from_slice(&1_u32.to_le_bytes());
        key.extend_from_slice(&64_u32.to_le_bytes());
        key.extend_from_slice(&resource_offset.to_le_bytes());
        key.resize(64, 0);
        key.extend_from_slice(&(bif.len() as u32).to_le_bytes());
        key.extend_from_slice(&76_u32.to_le_bytes());
        key.extend_from_slice(&(filename.len() as u16).to_le_bytes());
        key.extend_from_slice(&0_u16.to_le_bytes());
        key.extend_from_slice(filename);
        let mut resref = [0_u8; 16];
        resref[..4].copy_from_slice(b"pfg2");
        key.write_all(&resref).unwrap();
        key.extend_from_slice(&0x07d2_u16.to_le_bytes());
        key.extend_from_slice(&0_u32.to_le_bytes());
        fs::write(data_directory.join("test.key"), key).unwrap();
        data_directory
    }

    #[test]
    fn indexes_and_reads_a_key_bif_resource() {
        let directory = tempfile::tempdir().unwrap();
        let payload = b"compiled model payload";
        let data_directory = write_installation(directory.path(), payload);

        let index = GameResourceIndex::build(&[directory.path().to_path_buf()], 0x07d2);
        let (loaded, origin) = index.load("PFG2", 0x07d2).unwrap().unwrap();
        assert_eq!(loaded, payload);
        assert_eq!(origin, data_directory.join("test.bif"));
    }

    #[test]
    fn preferred_installation_wins_duplicate_resource_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let preferred = directory.path().join("preferred");
        let fallback = directory.path().join("fallback");
        let preferred_data = write_installation(&preferred, b"preferred model");
        write_installation(&fallback, b"fallback model");

        let index = GameResourceIndex::build(&[preferred, fallback], 0x07d2);
        let (loaded, origin) = index.load("pfg2", 0x07d2).unwrap().unwrap();
        assert_eq!(loaded, b"preferred model");
        assert_eq!(origin, preferred_data.join("test.bif"));
    }
}
