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
    /// Disk limit in bytes; 0 means unlimited (wings `SetDiskLimit`).
    disk_limit: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// UID/GID new files are chowned to; -1 disables.
    chown_uid: std::sync::Arc<std::sync::atomic::AtomicI32>,
    chown_gid: std::sync::Arc<std::sync::atomic::AtomicI32>,
}

impl Filesystem {
    pub fn new(root: PathBuf, denylist: Vec<String>) -> Self {
        Self {
            root,
            denylist: std::sync::Arc::new(std::sync::RwLock::new(denylist)),
            disk_limit: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
            chown_uid: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-1)),
            chown_gid: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-1)),
        }
    }

    pub fn set_disk_limit(&self, bytes: i64) {
        self.disk_limit.store(bytes, std::sync::atomic::Ordering::SeqCst);
    }

    fn max_disk(&self) -> i64 {
        self.disk_limit.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether `size` more bytes would fit under the disk limit
    /// (wings `HasSpaceFor`, using the cached usage).
    pub fn has_space_for(&self, size: i64) -> bool {
        if self.max_disk() <= 0 {
            return true;
        }
        let used = self.disk_usage();
        (used as i64) + size <= self.max_disk()
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

    /// Resolve a file name relative to an already-resolved base directory,
    /// rejecting any `..` component (wings filepath.Join + safePath).
    fn resolve_under(&self, base: &Path, rel: &str) -> AppResult<PathBuf> {
        let joined = base.join(rel);
        let components: Vec<Component> = joined.components().collect();
        if components.iter().any(|c| matches!(c, Component::ParentDir)) {
            return Err(AppError::BadRequest("path escapes the server directory".into()));
        }
        Ok(joined)
    }

    /// Confine an operation to the server root: the nearest existing
    /// ancestor of `p` must canonicalize inside the (canonical) root, so
    /// symlinked parent directories can never reach outside the data
    /// directory (wings safePath + O_NOFOLLOW confinement).
    pub(crate) fn assert_contained(&self, p: &Path) -> AppResult<()> {
        let canon_root = self.root.canonicalize().map_err(|e| {
            AppError::Internal(anyhow::anyhow!("cannot canonicalize data dir: {e}"))
        })?;
        let mut ancestor = p.to_path_buf();
        loop {
            match ancestor.canonicalize() {
                Ok(canon) => {
                    if !canon.starts_with(&canon_root) {
                        return Err(AppError::BadRequest(
                            "path escapes the server directory".into(),
                        ));
                    }
                    break;
                }
                Err(_) => {
                    if !ancestor.pop() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Open a path inside the root with O_NOFOLLOW, so the final component
    /// can never resolve through a symlink (wings openat with NOFOLLOW).
    fn open_no_follow(&self, p: &Path, flags: nix::fcntl::OFlag, mode: nix::sys::stat::Mode) -> AppResult<std::fs::File> {
        use nix::fcntl::OFlag;
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.custom_flags((flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits());
        if flags.contains(OFlag::O_RDONLY) {
            opts.read(true);
        }
        if flags.contains(OFlag::O_WRONLY) {
            opts.write(true);
        }
        if flags.contains(OFlag::O_CREAT) {
            opts.create(true);
        }
        if flags.contains(OFlag::O_TRUNC) {
            opts.truncate(true);
        }
        if flags.contains(OFlag::O_EXCL) {
            opts.create_new(true);
        }
        opts.mode(mode.bits());
        opts.open(p)
            .map_err(|e| AppError::BadRequest(format!("cannot open {}: {e}", p.display())))
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
        denylist.iter().any(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return false;
            }
            // Wings matches the denylist with gitignore semantics: a
            // pattern without a slash matches any path component, a
            // pattern with a slash matches relative to the server root.
            if entry.contains('/') {
                glob_match(entry, &normalized)
            } else {
                normalized.split('/').any(|comp| glob_match(entry, comp))
            }
        })
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
        self.assert_contained(&p)?;
        // Wings refuses to read named pipes (FIFOs) — they can block or
        // dump unbounded data. O_NONBLOCK guarantees the open itself never
        // blocks; the fstat below rejects anything that is not a regular
        // file (O_NOFOLLOW already refused symlinks at open time).
        use nix::fcntl::OFlag;
        let mut file = self.open_no_follow(&p, OFlag::O_RDONLY | OFlag::O_NONBLOCK, nix::sys::stat::Mode::empty())?;
        let meta = file
            .metadata()
            .map_err(|e| AppError::BadRequest(format!("cannot stat {path}: {e}")))?;
        if !meta.is_file() {
            return Err(AppError::BadRequest(format!(
                "refusing to read {path}: not a regular file"
            )));
        }
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut buf)
            .map_err(|e| AppError::BadRequest(format!("cannot read {path}: {e}")))?;
        Ok(buf)
    }

    pub fn write(&self, path: &str, bytes: &[u8]) -> AppResult<()> {
        let p = self.resolve(path)?;
        self.check_denied(path)?;
        self.assert_contained(&p)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| AppError::BadRequest(format!("cannot create parent dir: {e}")))?;
        }
        // Wings Write: the amount of new data must fit before writing.
        let current = fs::symlink_metadata(&p).map(|m| m.len() as i64).unwrap_or(0);
        if !self.has_space_for(bytes.len() as i64 - current) {
            return Err(AppError::BadRequest(
                "filesystem: not enough disk space".into(),
            ));
        }
        // O_NOFOLLOW: the final component must not be a symlink (an open
        // through a symlink would write outside the server directory).
        use nix::fcntl::OFlag;
        let mut file = self.open_no_follow(
            &p,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )?;
        std::io::Write::write_all(&mut file, bytes)
            .map_err(|e| AppError::BadRequest(format!("cannot write {path}: {e}")))?;
        self.chown(&p);
        Ok(())
    }

    /// Chown a single path to the container user (wings `chownFile`).
    pub fn chown(&self, p: &Path) {
        use std::os::unix::fs::chown as unix_chown;
        let uid = self.chown_uid.load(std::sync::atomic::Ordering::SeqCst);
        let gid = self.chown_gid.load(std::sync::atomic::Ordering::SeqCst);
        if uid >= 0 && gid >= 0 {
            let _ = unix_chown(p, Some(uid as u32), Some(gid as u32));
        }
    }

    /// Set the UID/GID new files are chowned to (wings defaults: the
    /// configured container user, or 988/988).
    pub fn set_chown_ids(&self, uid: i32, gid: i32) {
        self.chown_uid.store(uid, std::sync::atomic::Ordering::SeqCst);
        self.chown_gid.store(gid, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn create_directory(&self, name: &str, path: &str) -> AppResult<()> {
        let p = self.resolve(path)?.join(name);
        self.assert_contained(&p)?;
        fs::create_dir_all(&p)
            .map_err(|e| AppError::BadRequest(format!("cannot create directory {name}: {e}")))
    }

    pub fn rename(&self, root: &str, files: &[(String, String)]) -> AppResult<()> {
        let base = self.resolve(root)?;
        for (to, from) in files {
            self.check_denied(from)?;
            self.check_denied(to)?;
            let src = self.resolve_under(&base, from)?;
            let dst = self.resolve_under(&base, to)?;
            self.assert_contained(&src)?;
            self.assert_contained(&dst)?;
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
        use nix::fcntl::OFlag;
        let src = self.resolve(location)?;
        self.check_denied(location)?;
        self.assert_contained(&src)?;
        let mut src_file =
            self.open_no_follow(&src, OFlag::O_RDONLY, nix::sys::stat::Mode::empty())?;
        let parent = src.parent().unwrap_or(&self.root);
        let name = src.file_name().unwrap_or_default().to_string_lossy();
        let mut dest = parent.join(format!("copy of {name}"));
        let mut i = 0;
        loop {
            match self.open_no_follow(
                &dest,
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
                nix::sys::stat::Mode::from_bits_truncate(0o644),
            ) {
                Ok(mut out) => {
                    std::io::copy(&mut src_file, &mut out)
                        .map_err(|e| AppError::BadRequest(format!("cannot copy {location}: {e}")))?;
                    break;
                }
                Err(_) if dest.exists() => {
                    i += 1;
                    dest = parent.join(format!("copy of {name} ({i})"));
                }
                Err(e) => return Err(e),
            }
        }
        self.stat(&dest)
    }

    pub fn delete(&self, root: &str, files: &[String]) -> AppResult<()> {
        let base = self.resolve(root)?;
        for file in files {
            self.check_denied(file)?;
            let p = self.resolve_under(&base, file)?;
            self.assert_contained(&p)?;
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
            let p = self.resolve_under(&base, file)?;
            self.assert_contained(&p)?;
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
        let base = self.resolve(root)?;
        for f in files {
            let p = self.resolve_under(&base, f)?;
            self.assert_contained(&p)?;
        }
        let dest = uuids::archive(&self.root, root, files)
            .map_err(|e| AppError::BadRequest(format!("cannot compress: {e}")))?;
        // Wings: if the archive does not fit under the limit, remove it and
        // report a disk space error.
        let size = fs::metadata(&dest).map(|m| m.len() as i64).unwrap_or(0);
        if !self.has_space_for(size) {
            let _ = fs::remove_file(&dest);
            return Err(AppError::BadRequest(
                "filesystem: not enough disk space".into(),
            ));
        }
        self.stat(&dest)
    }

    /// Decompress a `.zip`, `.tar.gz`, `.tar` (or plain `.gz`) archive in
    /// `root` into `root` (wings `DecompressFile`).
    pub fn decompress(&self, root: &str, file: &str) -> AppResult<()> {
        let base = self.resolve(root)?;
        self.check_denied(file)?;
        let archive = self.resolve_under(&base, file)?;
        self.assert_contained(&archive)?;
        let lower = file.to_lowercase();
        if !(lower.ends_with(".zip")
            || lower.ends_with(".tar.gz")
            || lower.ends_with(".tgz")
            || lower.ends_with(".tar")
            || lower.ends_with(".gz"))
        {
            return Err(AppError::BadRequest(
                "filesystem: unknown archive format".into(),
            ));
        }
        // Wings SpaceAvailableForDecompression: refuse to start when the
        // archive's uncompressed contents would exceed the disk limit.
        if let Ok(total) = uuids::uncompressed_size(&archive) {
            if !self.has_space_for(total as i64) {
                return Err(AppError::BadRequest(
                    "filesystem: not enough disk space".into(),
                ));
            }
        }
        uuids::extract_user_archive(&archive, &base)
            .map_err(|e| AppError::BadRequest(format!("cannot decompress {file}: {e}")))?;
        Ok(())
    }

    /// Extract an arbitrary archive (e.g. a backup restore) into the root.
    pub fn decompress_archive(&self, archive: &Path) -> AppResult<()> {
        uuids::extract(archive, &self.root)
            .map_err(|e| AppError::BadRequest(format!("cannot extract {}: {e}", archive.display())))
    }

    /// Used disk space of the server data directory, computed with `du`
    /// semantics: a non-recursive walk that does not follow symlinks
    /// (wings `DirectorySize` — filepath.Walk uses Lstat, so symlinks are
    /// counted by their own size, never traversed).
    pub fn disk_usage(&self) -> u64 {
        let mut total: u64 = 0;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let meta = match fs::symlink_metadata(entry.path()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    stack.push(entry.path());
                } else {
                    total += meta.len();
                }
            }
        }
        total
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

    /// Extract a tar.gz archive into `base`, wing-style: entry paths are
    /// used as-is (never stripped — wings uses mholt/archives which does
    /// not unwrap a top-level folder) and path traversal is blocked.
    pub fn extract(archive: &Path, base: &Path) -> std::io::Result<()> {
        let file = std::fs::File::open(archive)?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(gz);
        tar.set_unpack_xattrs(false);
        tar.set_preserve_permissions(false);
        tar.entries()?
            .filter_map(|e| e.ok())
            .try_for_each(|mut entry| -> std::io::Result<()> {
                let entry_path = entry.path()?.into_owned();
                if entry_path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    return Ok(());
                }
                let dest = base.join(&entry_path);
                // Guard against path traversal inside the archive.
                if !dest.starts_with(base) {
                    return Ok(());
                }
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                entry.unpack(&dest).map(|_| ())
            })
    }

    /// Whether the file is a tar archive (checks the ustar magic at
    /// offset 257, like archives.Identify does for wings).
    fn is_tar(file: &mut std::fs::File) -> std::io::Result<bool> {
        use std::io::{Read, Seek, SeekFrom};
        let mut buf = [0u8; 512];
        file.seek(SeekFrom::Start(0))?;
        let n = file.read(&mut buf)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(n >= 262 && &buf[257..262] == b"ustar")
    }

    /// Total uncompressed size of a .tar.gz/.tar/.zip archive (wings
    /// `SpaceAvailableForDecompression`). Returns Ok(0) for unknown types.
    pub fn uncompressed_size(archive: &Path) -> std::io::Result<u64> {
        let name = archive
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut total: u64 = 0;

        if name.ends_with(".zip") {
            let file = std::fs::File::open(archive)?;
            let mut zip = zip::ZipArchive::new(file)
                .map_err(|e| std::io::Error::other(format!("invalid zip: {e}")))?;
            for i in 0..zip.len() {
                let entry = zip
                    .by_index(i)
                    .map_err(|e| std::io::Error::other(format!("zip entry: {e}")))?;
                total += entry.size();
            }
            return Ok(total);
        }

        if name.ends_with(".gz") || name.ends_with(".tgz") {
            let mut file = std::fs::File::open(archive)?;
            if is_tar(&mut file)? {
                let gz = flate2::read::GzDecoder::new(file);
                let mut tar = tar::Archive::new(gz);
                for entry in tar.entries()?.filter_map(|e| e.ok()) {
                    total += entry.size();
                }
            } else {
                // Single-file compression: size is the compressed size.
                total = std::fs::metadata(archive)?.len();
            }
            return Ok(total);
        }

        if name.ends_with(".tar") {
            let file = std::fs::File::open(archive)?;
            let mut tar = tar::Archive::new(file);
            for entry in tar.entries()?.filter_map(|e| e.ok()) {
                total += entry.size();
            }
            return Ok(total);
        }

        Ok(0)
    }

    /// Extract a user-facing archive (zip / tar / tar.gz / plain gz) into
    /// `base`, wing-style: paths are used as-is, symlinks inside tar
    /// archives are honored, and path traversal is blocked.
    pub fn extract_user_archive(archive: &Path, base: &Path) -> std::io::Result<()> {
        let name = archive
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if name.ends_with(".zip") {
            let file = std::fs::File::open(archive)?;
            let mut zip = zip::ZipArchive::new(file)
                .map_err(|e| std::io::Error::other(format!("invalid zip: {e}")))?;
            zip.extract(base)
                .map_err(|e| std::io::Error::other(format!("cannot extract zip: {e}")))?;
            return Ok(());
        }

        if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            let file = std::fs::File::open(archive)?;
            let gz = flate2::read::GzDecoder::new(file);
            return extract_tar(gz, base);
        }

        if name.ends_with(".tar") {
            let file = std::fs::File::open(archive)?;
            return extract_tar(file, base);
        }

        if name.ends_with(".gz") {
            let mut file = std::fs::File::open(archive)?;
            if is_tar(&mut file)? {
                let gz = flate2::read::GzDecoder::new(file);
                return extract_tar(gz, base);
            }
            // Single-file compression: write `<name>` minus the .gz suffix.
            let stripped = archive
                .file_name()
                .map(|s| s.to_string_lossy().replace(".gz", ""))
                .unwrap_or_default();
            let dest = base.join(stripped);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut decoder = flate2::read::GzDecoder::new(file);
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut decoder, &mut out)?;
            return Ok(());
        }

        Err(std::io::Error::other("filesystem: unknown archive format"))
    }

    fn extract_tar<R: std::io::Read>(reader: R, base: &Path) -> std::io::Result<()> {
        let mut tar = tar::Archive::new(reader);
        tar.set_unpack_xattrs(false);
        tar.set_preserve_permissions(false);
        tar.entries()?
            .filter_map(|e| e.ok())
            .try_for_each(|mut entry| -> std::io::Result<()> {
                let entry_path = entry.path()?.into_owned();
                let dest = base.join(&entry_path);
                // Guard against path traversal inside the archive.
                if !dest.starts_with(base) {
                    return Ok(());
                }
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                entry.unpack(&dest).map(|_| ())
            })
    }
}

/// Minimal glob matcher supporting `*`, `**` and `?`, used for the file
/// denylist (wings matches it with gitignore semantics via
/// ignore.CompileIgnoreLines + MatchesPath).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn m(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') if p.get(1) == Some(&'*') => {
                let rest = &p[2..];
                (0..=t.len()).any(|i| m(rest, &t[i..]))
            }
            Some('*') => {
                let rest = &p[1..];
                (0..=t.len()).any(|i| {
                    if t.get(i) == Some(&'/') {
                        return false;
                    }
                    m(rest, &t[i..])
                })
            }
            Some('?') => !t.is_empty() && m(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && m(&p[1..], &t[1..]),
        }
    }
    m(&p, &t)
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