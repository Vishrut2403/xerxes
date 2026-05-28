use crate::db::XerxesDb;
use anyhow::Result;
use jwalk::WalkDir;
use std::path::Path;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".cache",
    "Trash",
    "proc",
    "sys",
    "dev",
    "tmp",
    "target",
    "dist",
    "build",
    "venv",
    "__pycache__",
    "Library",
];

pub fn scan_and_index<P: AsRef<Path>>(root: P, db: &XerxesDb) -> Result<()> {
    let walk = WalkDir::new(root).skip_hidden(true).process_read_dir(
        |_depth, _path, _read_dir_state, children| {
            children.retain(|dir_entry_result| {
                if let Ok(dir_entry) = dir_entry_result {
                    if !dir_entry.file_type().is_dir() {
                        return false;
                    }
                    let file_name = dir_entry.file_name();
                    let name_str = file_name.to_string_lossy();
                    if IGNORED_DIRS.contains(&name_str.as_ref()) {
                        return false;
                    }
                    true
                } else {
                    false
                }
            });
        },
    );

    let mut count = 0;
    let mut txn = db.begin_write()?;

    for entry in walk {
        if let Ok(dir_entry) = entry {
            let path = dir_entry.path();
            if let Err(e) = db.add_directory_txn(&txn, &path) {
                eprintln!("Failed to index directory {:?}: {}", path, e);
            }

            count += 1;
            if count % 1000 == 0 {
                txn.commit()?;
                txn = db.begin_write()?;
            }
        }
    }

    txn.commit()?;
    Ok(())
}
