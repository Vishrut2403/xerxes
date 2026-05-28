use anyhow::Result;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SEGMENT_ID_TO_STR: TableDefinition<u32, &str> = TableDefinition::new("segment_id_to_str");
const SEGMENT_STR_TO_ID: TableDefinition<&str, u32> = TableDefinition::new("segment_str_to_id");
const DIRECTORIES: TableDefinition<u64, &[u8]> = TableDefinition::new("directories");
const METADATA: TableDefinition<u64, &[u8]> = TableDefinition::new("metadata");
const STATE: TableDefinition<&str, u32> = TableDefinition::new("state");
const ALIASES: TableDefinition<&str, &str> = TableDefinition::new("aliases");
const SELECTION_MEMORY: TableDefinition<&str, &str> = TableDefinition::new("selection_memory");

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirectoryEntry {
    pub segments: Vec<u32>,
    pub depth: u16,
    pub hash: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DirMetadata {
    pub frecency: f32,
    pub last_access: u64,
    pub access_count: u32,
}

pub struct XerxesDb {
    db: Database,
    segment_cache: Mutex<HashMap<String, u32>>,
    id_to_segment_cache: Mutex<HashMap<u32, String>>,
    next_segment_id: Mutex<u32>,
    dir_cache: Mutex<Option<std::sync::Arc<Vec<(DirectoryEntry, DirMetadata)>>>>,
}

impl XerxesDb {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = Database::create(path)?;
        let next_segment_id;
        let mut segment_cache = HashMap::new();
        let mut id_to_segment_cache = HashMap::new();

        {
            let write_txn = db.begin_write()?;
            {
                write_txn.open_table(SEGMENT_ID_TO_STR)?;
                write_txn.open_table(SEGMENT_STR_TO_ID)?;
                write_txn.open_table(DIRECTORIES)?;
                write_txn.open_table(METADATA)?;
                write_txn.open_table(ALIASES)?;
                write_txn.open_table(SELECTION_MEMORY)?;
                let state_table = write_txn.open_table(STATE)?;
                next_segment_id = state_table
                    .get("next_segment_id")?
                    .map(|v| v.value())
                    .unwrap_or(0);

                if next_segment_id > 0 {
                    let id_table = write_txn.open_table(SEGMENT_ID_TO_STR)?;
                    for item in id_table.iter()? {
                        let (k, v) = item?;
                        let key = k.value();
                        let val = v.value().to_string();
                        segment_cache.insert(val.clone(), key);
                        id_to_segment_cache.insert(key, val);
                    }
                }
            }
            write_txn.commit()?;
        }

        Ok(Self {
            db,
            segment_cache: Mutex::new(segment_cache),
            id_to_segment_cache: Mutex::new(id_to_segment_cache),
            next_segment_id: Mutex::new(next_segment_id),
            dir_cache: Mutex::new(None),
        })
    }

    pub fn invalidate_cache(&self) {
        *self.dir_cache.lock().unwrap() = None;
    }

    pub fn begin_write(&self) -> Result<WriteTransaction> {
        Ok(self.db.begin_write()?)
    }

    pub fn intern_segment_txn(&self, txn: &WriteTransaction, segment: &str) -> Result<u32> {
        {
            let cache = self.segment_cache.lock().unwrap();
            if let Some(&id) = cache.get(segment) {
                return Ok(id);
            }
        }

        let mut id_to_str = txn.open_table(SEGMENT_ID_TO_STR)?;
        let mut str_to_id = txn.open_table(SEGMENT_STR_TO_ID)?;
        let mut state_table = txn.open_table(STATE)?;

        let mut next_id_guard = self.next_segment_id.lock().unwrap();
        let current_id = *next_id_guard;

        id_to_str.insert(current_id, segment)?;
        str_to_id.insert(segment, current_id)?;

        *next_id_guard += 1;
        state_table.insert("next_segment_id", *next_id_guard)?;

        self.segment_cache
            .lock()
            .unwrap()
            .insert(segment.to_string(), current_id);
        self.id_to_segment_cache
            .lock()
            .unwrap()
            .insert(current_id, segment.to_string());

        Ok(current_id)
    }

    pub fn intern_path_txn(&self, txn: &WriteTransaction, path: &Path) -> Result<Vec<u32>> {
        let mut segments = Vec::new();
        for component in path.components() {
            let comp_str = component.as_os_str().to_string_lossy();
            let id = self.intern_segment_txn(txn, &comp_str)?;
            segments.push(id);
        }
        Ok(segments)
    }

    pub fn add_directory_txn(&self, txn: &WriteTransaction, path: &Path) -> Result<()> {
        self.invalidate_cache();
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let hash = hasher.finish();

        let segments = self.intern_path_txn(txn, path)?;
        let entry = DirectoryEntry {
            depth: segments.len() as u16,
            segments,
            hash,
        };

        let mut dirs = txn.open_table(DIRECTORIES)?;
        let mut meta = txn.open_table(METADATA)?;

        let entry_bytes = bincode::serialize(&entry)?;
        dirs.insert(hash, entry_bytes.as_slice())?;

        if meta.get(hash)?.is_none() {
            let default_meta = DirMetadata::default();
            let meta_bytes = bincode::serialize(&default_meta)?;
            meta.insert(hash, meta_bytes.as_slice())?;
        }

        Ok(())
    }

    pub fn add_directory(&self, path: &Path) -> Result<()> {
        let write_txn = self.begin_write()?;
        self.add_directory_txn(&write_txn, path)?;
        write_txn.commit()?;
        Ok(())
    }

    pub fn resolve_path(&self, segments: &[u32]) -> PathBuf {
        let mut path = PathBuf::new();
        let cache = self.id_to_segment_cache.lock().unwrap();
        for id in segments {
            if let Some(s) = cache.get(id) {
                path.push(s);
            }
        }
        path
    }

    pub fn get_all_directories(
        &self,
    ) -> Result<std::sync::Arc<Vec<(DirectoryEntry, DirMetadata)>>> {
        if let Some(cache) = self.dir_cache.lock().unwrap().as_ref() {
            return Ok(std::sync::Arc::clone(cache));
        }

        let read_txn = self.db.begin_read()?;
        let dirs = read_txn.open_table(DIRECTORIES)?;
        let meta = read_txn.open_table(METADATA)?;

        let mut entries = Vec::new();
        for item in dirs.iter()? {
            let (k, v) = item?;
            let hash = k.value();
            let entry: DirectoryEntry = bincode::deserialize(v.value())?;

            let metadata = if let Some(m_v) = meta.get(hash)? {
                bincode::deserialize(m_v.value()).unwrap_or_default()
            } else {
                DirMetadata::default()
            };

            entries.push((entry, metadata));
        }

        let arc_entries = std::sync::Arc::new(entries);
        *self.dir_cache.lock().unwrap() = Some(std::sync::Arc::clone(&arc_entries));
        Ok(arc_entries)
    }

    pub fn update_dir_access(&self, path: &Path) -> Result<()> {
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let hash = hasher.finish();

        let write_txn = self.begin_write()?;
        {
            let mut meta = write_txn.open_table(METADATA)?;
            let mut metadata: DirMetadata = if let Some(v) = meta.get(hash)? {
                bincode::deserialize(v.value()).unwrap_or_default()
            } else {
                DirMetadata::default()
            };

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if metadata.last_access > 0 {
                let elapsed_days = (now.saturating_sub(metadata.last_access)) as f32 / 86400.0;
                metadata.frecency *= 0.5f32.powf(elapsed_days / 7.0);
            }
            metadata.frecency += 1.0;
            metadata.last_access = now;
            metadata.access_count += 1;

            let meta_bytes = bincode::serialize(&metadata)?;
            meta.insert(hash, meta_bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_stats(&self) -> Result<(u64, usize)> {
        let read_txn = self.db.begin_read()?;
        let dirs = read_txn.open_table(DIRECTORIES)?;
        let dir_count = dirs.len()?;
        let segment_count = self.segment_cache.lock().unwrap().len();
        Ok((dir_count, segment_count))
    }

    pub fn is_empty(&self) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        let dirs = read_txn.open_table(DIRECTORIES)?;
        Ok(dirs.len()? == 0)
    }

    pub fn add_alias(&self, nickname: &str, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ALIASES)?;
            table.insert(nickname, path_str.as_ref())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_alias(&self, nickname: &str) -> Result<Option<PathBuf>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ALIASES)?;
        if let Some(v) = table.get(nickname)? {
            return Ok(Some(PathBuf::from(v.value())));
        }
        Ok(None)
    }

    pub fn record_selection(&self, query: &str, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(SELECTION_MEMORY)?;
            table.insert(query, path_str.as_ref())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_selection_memory(&self, query: &str) -> Result<Option<PathBuf>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SELECTION_MEMORY)?;
        if let Some(v) = table.get(query)? {
            return Ok(Some(PathBuf::from(v.value())));
        }
        Ok(None)
    }

    pub fn list_aliases(&self) -> Result<Vec<(String, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ALIASES)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            results.push((k.value().to_string(), v.value().to_string()));
        }
        Ok(results)
    }

    pub fn remove_alias(&self, nickname: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ALIASES)?;
            table.remove(nickname)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn clear_aliases(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ALIASES)?;
            let keys: Vec<String> = table
                .iter()?
                .map(|i| i.unwrap().0.value().to_string())
                .collect();
            for key in keys {
                table.remove(key.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn clear_selection_memory(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(SELECTION_MEMORY)?;
            let keys: Vec<String> = table
                .iter()?
                .map(|i| i.unwrap().0.value().to_string())
                .collect();
            for key in keys {
                table.remove(key.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }
}
