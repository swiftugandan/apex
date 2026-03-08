use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::Result;
use crate::types::FsyncMode;

// This module requires Unix filesystem semantics (permissions, fsync, atomic
// rename). It will not compile on non-Unix platforms.
#[cfg(not(unix))]
compile_error!("rfbmq-core requires a Unix platform (Linux, macOS, *BSD)");

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

pub fn create_dir_with_mode(path: &Path, mode: u32) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)?;
    Ok(())
}

pub fn fsync_file_handle(f: &fs::File) -> std::io::Result<()> {
    f.sync_all()
}

pub fn fsync_dir(path: &Path, mode: FsyncMode) -> Result<()> {
    match mode {
        FsyncMode::Full => {
            let d = fs::File::open(path)?;
            d.sync_all()?;
            Ok(())
        }
        FsyncMode::Batch | FsyncMode::None => Ok(()),
    }
}

pub fn fsync_dir_force(path: &Path) -> Result<()> {
    let d = fs::File::open(path)?;
    d.sync_all()?;
    Ok(())
}

/// Atomically write content to a temporary file, then rename it to the
/// destination. Fsyncs file, source dir, and destination dir according to the
/// given `FsyncMode`.
pub fn durable_write_rename(
    tmp_dir: &Path,
    tmp_name: &str,
    dst_path: &Path,
    content: &[u8],
    file_mode: u32,
    fsync_mode: FsyncMode,
) -> Result<()> {
    let tmp_path = tmp_dir.join(tmp_name);
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(content)?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(file_mode))?;
        // Fsync the file handle before closing, so we don't lose error
        // information if sync_all would fail.
        match fsync_mode {
            FsyncMode::None => {}
            FsyncMode::Full | FsyncMode::Batch => {
                f.sync_all()?;
            }
        }
    }
    fsync_dir(tmp_dir, fsync_mode)?;

    fs::rename(&tmp_path, dst_path)?;

    if let Some(dst_dir) = dst_path.parent() {
        fsync_dir(dst_dir, fsync_mode)?;
    }
    fsync_dir(tmp_dir, fsync_mode)?; // source dir changed by rename

    Ok(())
}

/// Durably rename a file from `src` to `dst`, fsyncing both the source and
/// destination directories according to the given `FsyncMode`.
pub fn durable_rename(src: &Path, dst: &Path, fsync_mode: FsyncMode) -> Result<()> {
    fs::rename(src, dst)?;
    if let Some(dst_dir) = dst.parent() {
        fsync_dir(dst_dir, fsync_mode)?;
    }
    if let Some(src_dir) = src.parent() {
        fsync_dir(src_dir, fsync_mode)?;
    }
    Ok(())
}

pub fn collect_md_files(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".md") {
            names.push(name);
        }
    }
    Ok(names)
}

pub fn count_md_files(dir: &Path) -> Result<usize> {
    let mut count: usize = 0;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(".md") {
            count += 1;
        }
    }
    Ok(count)
}
