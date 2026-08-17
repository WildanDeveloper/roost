use chrono::DateTime;
use nix::sys::statvfs::statvfs;
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::error::{AppError, AppResult};
use sha1::{Digest, Sha1};
use walkdir::WalkDir;

/// JSON shape of a filesystem entry, matching wings `filesystem.Stat`
/// (v1.13): `{name, created, modified, mode, mode_bits, size, directory,
/// file, symlink, mime}`.
#[derive(Debug, Clone, Serialize)]
pub struct FileStat {
    pub name: String,
    pub created: String,
    pub modified: String,
    pub mode: String,
    pub mode_bits: String,
    pub size: i64,
    pub directory: bool,
    pub file: bool,
    pub symlink: bool,
    pub mime: String,
}

/// Managed filesystem for one server, rooted at the server's data volume.
#[derive(Clone)]
pub struct Filesystem {
    root: PathBuf,
    denylist: std::sync::Arc<std::sync::RwLock<Vec<String>>>,
}

impl Filesystem {
    pub fn new(root: PathBuf, denylist: Vec<String>) -> Self {
        Self {
            root,
            denylist: std::sync::Arc::new(std::sync::RwLock::new(denylist)),
        }
    }

    pub fn set_denylist(&self, denylist: Vec<String>) {
        if let Ok(mut guard) = self.denylist.write() {
            *guard = denylist;
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-supplied path against the root. Returns an error if
    /// the path escapes the root or is otherwise unsafe. Absolute paths are
    /// allowed but are treated as relative to the root, matching wings
    /// (filepath.Join(base, strings.TrimPrefix(path, base))).
    pub fn resolve(&self, path: &str) -> AppResult<PathBuf> {
        let cleaned = path.trim_start_matches('/');
        let joined = self.root.join(cleaned);
        let components: Vec<Component> = joined.components().collect();
        if components.iter().any(|c| matches!(c, Component::ParentDir)) {
            return Err(AppError::BadRequest("path escapes the server directory".into()));
        }
        Ok(joined)
    }

    fn is_denied(&self, path: &str) -> bool {
        let normalized = Path::new(path)
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");

        let denylist = self.denylist.read().map(|g| g.clone()).unwrap_or_default();
        denylist
            .iter()
            .any(|entry| entry == &normalized || path.contains(entry))
    }

    pub fn check_denied(&self, path: &str) -> AppResult<()> {
        if self.is_denied(path) {
            Err(AppError::BadRequest(format!("path is denied by the file denylist: {path}")))
        } else {
            Ok(())
        }
    }

    /// Relative path of `abs` against the root (or the raw path if it
    /// points outside, as used by archives).
    #[allow(dead_code)]
pub fn rel(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.root)
            .unwrap_or(abs)
            .to_string_lossy()
            .to_string()
    }

    // ---- directory listings -------------------------------------------------

    pub fn list_directory(&self, directory: &str) -> AppResult<Vec<FileStat>> {
        let dir = self.resolve(directory)?;
        let entries = fs::read_dir(&dir)
            .map_err(|e| AppError::BadRequest(format!("cannot list {directory}: {e}")))?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| AppError::BadRequest(format!("readdir error: {e}")))?;
            if let Ok(stat) = self.stat(&entry.path()) {
                out.push(stat);
            }
        }
        out.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    pub fn stat(&self, path: &Path) -> AppResult<FileStat> {
        let meta = fs::symlink_metadata(path)
            .map_err(|e| AppError::BadRequest(format!("cannot stat {}: {e}", path.display())))?;
        Ok(stat_from_meta(path, &meta))
    }

    // ---- files ---------------------------------------------------------------

    pub fn read(&self, path: &str) -> AppResult<Vec<u8>> {
        let p = self.resolve(path)?;
        self.check_denied(path)?;
        // Wings refuses to read named pipes (FIFOs) — they can block or
        // dump unbounded data.
        let meta = fs::symlink_metadata(&p)
            .map_err(|e| AppError::BadRequest(format!("cannot stat {path}: {e}")))?;
        if !meta.is_file() {
            return Err(AppError::BadRequest(format!(
                "refusing to read {path}: not a regular file"
            )));
        }
        fs::read(&p).map_err(|e| AppError::BadRequest(format!("cannot read {path}: {e}")))
    }

    pub fn write(&self, path: &str, bytes: &[u8]) -> AppResult<()> {
        let p = self.resolve(path)?;
        self.check_denied(path)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| AppError::BadRequest(format!("cannot create parent dir: {e}")))?;
        }
        fs::write(&p, bytes).map_err(|e| AppError::BadRequest(format!("cannot write {path}: {e}")))
    }

    pub fn create_directory(&self, name: &str, path: &str) -> AppResult<()> {
        let p = self.resolve(path)?.join(name);
        fs::create_dir_all(&p)
            .map_err(|e| AppError::BadRequest(format!("cannot create directory {name}: {e}")))
    }

    pub fn rename(&self, root: &str, files: &[(String, String)]) -> AppResult<()> {
        let base = self.resolve(root)?;
        for (to, from) in files {
            self.check_denied(from)?;
            self.check_denied(to)?;
            let src = base.join(from);
            let dst = base.join(to);
            if dst.parent().map(|p| p.as_os_str()) != src.parent().map(|p| p.as_os_str()) {
                return Err(AppError::BadRequest("rename source and target must be in the same directory".into()));
            }
            // Reject renames that would overwrite an existing directory.
            if dst.is_dir() && !src.is_dir() {
                return Err(AppError::BadRequest(format!("cannot move file over directory {to}")));
            }
            fs::rename(&src, &dst)
                .map_err(|e| AppError::BadRequest(format!("cannot rename {from} -> {to}: {e}")))?;
        }
        Ok(())
    }

    pub fn copy(&self, location: &str) -> AppResult<FileStat> {
        let src = self.resolve(location)?;
        self.check_denied(location)?;
        if !src.is_file() {
            return Err(AppError::BadRequest("only files can be copied".into()));
        }
        let parent = src.parent().unwrap_or(&self.root);
        let name = src.file_name().unwrap_or_default().to_string_lossy();
        let mut dest = parent.join(format!("copy of {name}"));
        let mut i = 0;
        while dest.exists() {
            i += 1;
            dest = parent.join(format!("copy of {name} ({i})"));
        }
        fs::copy(&src, &dest)
            .map_err(|e| AppError::BadRequest(format!("cannot copy {location}: {e}")))?;
        self.stat(&dest)
    }

    pub fn delete(&self, root: &str, files: &[String]) -> AppResult<()> {
        let base = self.resolve(root)?;
        for file in files {
            self.check_denied(file)?;
            let p = base.join(file);
            let meta = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                fs::remove_dir_all(&p)
                    .map_err(|e| AppError::BadRequest(format!("cannot delete {file}: {e}")))?;
            } else {
                fs::remove_file(&p)
                    .map_err(|e| AppError::BadRequest(format!("cannot delete {file}: {e}")))?;
            }
        }
        Ok(())
    }

    pub fn chmod(&self, root: &str, files: &[(String, u32)]) -> AppResult<()> {
        use std::os::unix::fs::PermissionsExt;
        let base = self.resolve(root)?;
        for (file, mode) in files {
            self.check_denied(file)?;
            let p = base.join(file);
            let meta = fs::symlink_metadata(&p)
                .map_err(|e| AppError::BadRequest(format!("cannot stat {file}: {e}")))?;
            if meta.is_symlink() {
                continue;
            }
            fs::set_permissions(&p, fs::Permissions::from_mode(*mode))
                .map_err(|e| AppError::BadRequest(format!("cannot chmod {file}: {e}")))?;
        }
        Ok(())
    }

    /// Compress the given files into `<random>.tar.gz` in `root`.
    pub fn compress(&self, root: &str, files: &[String]) -> AppResult<FileStat> {
        uuids::archive(&self.root, root, files)
            .map_err(|e| AppError::BadRequest(format!("cannot compress: {e}")))?;
        // find the created archive
        let base = self.resolve(root)?;
        let mut newest: Option<PathBuf> = None;
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("tar.gz") {
                    newest = Some(p);
                }
            }
        }
        let arch = newest.ok_or_else(|| AppError::BadRequest("archive was not created".into()))?;
        self.stat(&arch)
    }

    /// Decompress a `.tar.gz` archive in `root` into `root`.
    pub fn decompress(&self, root: &str, file: &str) -> AppResult<()> {
        let base = self.resolve(root)?;
        self.check_denied(file)?;
        let archive = base.join(file);
        if archive
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e != "gz")
            .unwrap_or(true)
        {
            return Err(AppError::BadRequest("only .tar.gz archives can be decompressed".into()));
        }
        uuids::extract(&archive, &base)
            .map_err(|e| AppError::BadRequest(format!("cannot decompress {file}: {e}")))?;
        Ok(())
    }

    /// Extract an arbitrary archive (e.g. a backup restore) into the root.
    pub fn decompress_archive(&self, archive: &Path) -> AppResult<()> {
        uuids::extract(archive, &self.root)
            .map_err(|e| AppError::BadRequest(format!("cannot extract {}: {e}", archive.display())))
    }

    /// Used disk space of the filesystem that holds the server data.
    /// Mirrors wings `DiskUsage` (statfs-based).
    pub fn disk_usage(&self) -> u64 {
        match statvfs(&self.root) {
            Ok(s) => (s.blocks() - s.blocks_available()) * s.block_size(),
            Err(_) => 0,
        }
    }

    /// Total available space on the data filesystem (used for disk limit).
    #[allow(dead_code)]
pub fn disk_available(&self) -> u64 {
        match statvfs(&self.root) {
            Ok(s) => s.blocks_available() * s.block_size(),
            Err(_) => 0,
        }
    }

    /// SHA1 checksum of a file relative to a root dir.
    pub fn checksum_sha1(&self, abs: &Path) -> AppResult<String> {
        let mut file = fs::File::open(abs)
            .map_err(|e| AppError::BadRequest(format!("cannot open {}: {e}", abs.display())))?;
        let mut hasher = Sha1::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| AppError::BadRequest(format!("read error: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let digest = hasher.finalize();
        Ok(hex::encode(digest.as_slice()))
    }

    /// Walk the data directory applying a gitignore-style ignore list
    /// (wings parses the panel's ignore patterns with go-gitignore). Only
    /// regular files and symlinks are returned; sockets are skipped
    /// (archive/tar does not support them) and ignored directories are
    /// pruned entirely. Used for backups.
    pub fn walk_files(&self, ignore: &str) -> Vec<PathBuf> {
        let patterns = crate::server::gitignore::compile(ignore);
        let mut out = Vec::new();
        for entry in WalkDir::new(&self.root).into_iter().filter_entry(|e| {
            let rel = e.path().strip_prefix(&self.root).unwrap_or(e.path());
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            !crate::server::gitignore::matches_path(&patterns, &rel_str)
        }) {
            if let Ok(entry) = entry {
                let ft = entry.file_type();
                if ft.is_file() || ft.is_symlink() {
                    out.push(entry.into_path());
                }
            }
        }
        out
    }

    /// Copy a directory recursively (used by restore).
    pub fn truncate_directory(&self) -> AppResult<()> {
        for entry in std::fs::read_dir(&self.root)
            .map_err(|e| AppError::BadRequest(format!("cannot read data dir: {e}")))?
        {
            let entry = entry.map_err(AppError::Io)?;
            let p = entry.path();
            if p.is_dir() {
                fs::remove_dir_all(&p).ok();
            } else {
                fs::remove_file(&p).ok();
            }
        }
        Ok(())
    }
}

fn stat_from_meta(path: &Path, meta: &fs::Metadata) -> FileStat {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let mode = meta.permissions().mode();
    let filetype = if meta.is_dir() {
        'd'
    } else if meta.file_type().is_symlink() {
        'l'
    } else {
        '-'
    };
    let mut mode_str = String::with_capacity(10);
    mode_str.push(filetype);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        mode_str.push(if bits & 4 != 0 { 'r' } else { '-' });
        mode_str.push(if bits & 2 != 0 { 'w' } else { '-' });
        mode_str.push(if bits & 1 != 0 { 'x' } else { '-' });
    }

    let is_dir = meta.is_dir();
    FileStat {
        name: path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        created: fmt_time(meta.ctime()),
        modified: fmt_time(meta.mtime()),
        mode: mode_str,
        mode_bits: format!("{:o}", mode & 0o7777),
        size: meta.size() as i64,
        directory: is_dir,
        file: !is_dir,
        symlink: meta.file_type().is_symlink(),
        mime: if is_dir {
            "inode/directory".to_string()
        } else {
            mime_for(path).to_string()
        },
    }
}

fn fmt_time(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

fn mime_for(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "txt" | "log" | "properties" => "text/plain",
        "json" => "application/json",
        "yml" | "yaml" => "text/yaml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "sh" | "bash" | "bat" => "text/x-sh",
        "md" => "text/markdown",
        "jar" => "application/java-archive",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        "dat" | "bin" | "db" => "application/octet-stream",
        "class" => "application/java-vm",
        "toml" => "text/toml",
        "ini" | "cfg" => "text/plain",
        "csv" => "text/csv",
        _ => "application/octet-stream",
    }
}

mod uuids {
    //! Small helpers to avoid confusion with the uuid crate name: these
    //! functions compress/extract tar.gz archives.
        use std::path::Path;

    pub fn archive(root: &Path, dir: &str, files: &[String]) -> std::io::Result<std::path::PathBuf> {
        let base = root.join(dir);
        let archive_name = format!("archive-{}.tar.gz", uuid::Uuid::new_v4());
        let dest = base.join(&archive_name);
        // Create the base directory if needed, so the archive lands in it.
        std::fs::create_dir_all(&base)?;

        let file = std::fs::File::create(&dest)?;
        let gz = flate2::GzBuilder::new()
            .filename(archive_name.as_bytes())
            .write(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for f in files {
            let src = base.join(f);
            let rel = Path::new(f);
            if src.is_dir() {
                tar.append_dir_all(rel, &src)?;
            } else {
                tar.append_path_with_name(&src, rel)?;
            }
        }
        let gz = tar.into_inner()?;
        let file = gz.finish()?;
        file.sync_all()?;
        Ok(dest)
    }

    pub fn extract(archive: &Path, base: &Path) -> std::io::Result<()> {
        let file = std::fs::File::open(archive)?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(gz);
        tar.set_unpack_xattrs(false);
        tar.set_preserve_permissions(false);
        // The archive entries may start with a top-level folder; extract
        // into base unwrapped (strip the first component).
        tar.entries()?
            .filter_map(|e| e.ok())
            .try_for_each(|mut entry| -> std::io::Result<()> {
                let entry_path = entry.path()?.into_owned();
                let mut parts = entry_path.components().peekable();
                // skip the root component if present
                if parts.peek().is_some() {
                    parts.next();
                }
                let rel: std::path::PathBuf = parts.collect();
                if rel.as_os_str().is_empty() {
                    return Ok(());
                }
                let dest = base.join(&rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Guard against path traversal inside the archive.
                if dest.starts_with(base) {
                    entry.unpack(&dest)?;
                }
                Ok(())
            })
    }
}

/// Minimal hex encoding (avoids an extra dependency for sha1 output).
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}