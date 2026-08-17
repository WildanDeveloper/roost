use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::Algorithm;
use russh::keys::{encode_pkcs8_pem, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Config as SshConfig, Session as SshSession};
use russh::{Channel, ChannelId};

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode,
    Version,
};
use russh_sftp::server::{Handler as SftpFsHandler, StatusReply};

use crate::config::SftpConfig;
use crate::server::activity::ActivityCollector;
use crate::server::ServerManager;

/// Permissions the panel grants for the SFTP subsystem (wings sftp handler).
const PERM_READ: &str = "file.read";
const PERM_READ_CONTENT: &str = "file.read-content";
const PERM_CREATE: &str = "file.create";
const PERM_UPDATE: &str = "file.update";
const PERM_DELETE: &str = "file.delete";

/// Usernames must at least look like the panel format (`<uuid>.<8 chars>`);
/// wings applies the same pre-check before hitting the panel API.
fn valid_username(user: &str) -> bool {
    if let Some(idx) = user.rfind('.') {
        let suffix = &user[idx + 1..];
        return suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit());
    }
    false
}

#[derive(Clone)]
struct Authed {
    server: String,
    user: String,
    permissions: Vec<String>,
}

impl Authed {
    fn can(&self, perm: &str) -> bool {
        self.permissions.iter().any(|p| p == "*" || p == perm)
    }
}

/// Per-connection SSH handler: validates credentials against the panel,
/// then hands the channel over to the SFTP protocol handler.
struct SshHandler {
    srv: Arc<SftpServer>,
    peer: SocketAddr,
    authed: Option<Authed>,
    session_channel: Option<Channel<russh::server::Msg>>,
}

impl SshHandler {
    fn new(srv: Arc<SftpServer>, peer: SocketAddr) -> Self {
        Self {
            srv,
            peer,
            authed: None,
            session_channel: None,
        }
    }

    async fn validate(
        &mut self,
        auth_type: &str,
        user: &str,
        password: &str,
    ) -> Result<Auth, anyhow::Error> {
        if !valid_username(user) {
            tracing::debug!(username = user, "sftp username format invalid");
            return Ok(Auth::reject());
        }
        let resp = match self
            .srv
            .panel
            .read()
            .await
            .validate_sftp_credentials(
                auth_type,
                user,
                password,
                &self.peer.ip().to_string(),
                &[],
                &[],
            )
            .await
        {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(error = %e, "sftp credential validation failed");
                None
            }
        };

        let Some(server) = resp.as_ref().and_then(|r| r.server.clone()) else {
            return Ok(Auth::reject());
        };
        self.authed = Some(Authed {
            server,
            user: resp.as_ref().map(|r| r.user.clone()).unwrap_or_default(),
            permissions: resp
                .map(|r| r.permissions)
                .unwrap_or_default(),
        });
        tracing::info!(ip = %self.peer.ip(), username = user, "sftp credentials accepted");
        Ok(Auth::Accept)
    }
}

impl russh::server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<Auth, Self::Error> {
        self.validate("password", user, password).await
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let key = public_key.to_string();
        self.validate("public_key", user, &key).await
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<russh::server::Msg>,
        reply: ChannelOpenHandle,
        _session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        self.session_channel = Some(channel);
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel);
            return Ok(());
        }
        session.channel_success(channel);

        let Some(authed) = self.authed.take() else {
            return Ok(());
        };
        let Some(channel) = self.session_channel.take() else {
            return Ok(());
        };

        let stream = channel.into_stream();
        let data_dir = if uuid::Uuid::parse_str(&authed.server).is_ok() {
            self.srv.manager.shared().daemon.read().await.data_dir(&authed.server)
        } else {
            self.srv.data_dir.join(&authed.server)
        };
        let handler = SftpFs::new(self.srv.clone(), authed, data_dir);
        tokio::spawn(russh_sftp::server::run(stream, handler));
        Ok(())
    }
}

/// SFTP file system backed by the authenticated server's data directory.
/// Path access is confined to that directory (mirrors wings unsafePath).
struct SftpFs {
    srv: Arc<SftpServer>,
    authed: Authed,
    root: PathBuf,
    handles: HashMap<String, HandleEntry>,
    next_handle: u64,
}

enum HandleEntry {
    File(std::fs::File),
    Dir { entries: Vec<File>, next: usize },
}

fn sr(code: StatusCode, msg: impl Into<String>) -> StatusReply {
    StatusReply::new(code).with_message(msg)
}

fn status_ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}

fn io_err(e: std::io::Error) -> StatusReply {
    match e.kind() {
        std::io::ErrorKind::NotFound => sr(StatusCode::NoSuchFile, e.to_string()),
        std::io::ErrorKind::PermissionDenied => sr(StatusCode::PermissionDenied, e.to_string()),
        _ => sr(StatusCode::Failure, e.to_string()),
    }
}

fn attrs_of(meta: &std::fs::Metadata) -> FileAttributes {
    FileAttributes {
        size: Some(meta.len()),
        uid: Some(meta.uid()),
        gid: Some(meta.gid()),
        user: None,
        group: None,
        permissions: Some(meta.mode()),
        atime: Some(meta.atime().max(0) as u32),
        mtime: Some(meta.mtime().max(0) as u32),
    }
}

fn longname_of(meta: &std::fs::Metadata, name: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    let mode = meta.mode();
    let file_type = if mode & 0o170000 == 0o040000 {
        'd'
    } else if mode & 0o170000 == 0o120000 {
        'l'
    } else {
        '-'
    };
    let mut perms = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        perms.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        perms.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        perms.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let t = meta.mtime();
    let (year, month, day, hour, min) = epoch_parts(t);
    let date = if year == epoch_parts(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
    .0
    {
        format!("{} {:2} {:02}:{:02}", months[(month - 1) as usize], day, hour, min)
    } else {
        format!("{} {:2}  {:4}", months[(month - 1) as usize], day, year)
    };
    format!(
        "{}{} {:>3} {:<8} {:<8} {:>8} {} {}",
        file_type,
        perms,
        meta.nlink(),
        meta.uid(),
        meta.gid(),
        meta.size(),
        date,
        name
    )
}

fn epoch_parts(t: i64) -> (i64, i64, i64, i64, i64) {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (hour, min) = (secs / 3600, (secs % 3600) / 60);
    let mut rem = days;
    let mut y2 = 1970i64;
    loop {
        let leap = (y2 % 4 == 0 && y2 % 100 != 0) || y2 % 400 == 0;
        let ydays = if leap { 366 } else { 365 };
        if rem < ydays {
            break;
        }
        rem -= ydays;
        y2 += 1;
    }
    let mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let leap = (y2 % 4 == 0 && y2 % 100 != 0) || y2 % 400 == 0;
    let mut month = 1i64;
    for (i, &d) in mdays.iter().enumerate() {
        let dim = if i == 1 && leap { d + 1 } else { d };
        if rem < dim {
            break;
        }
        rem -= dim;
        month += 1;
    }
    (y2, month, rem + 1, hour, min)
}

impl SftpFs {
    fn new(srv: Arc<SftpServer>, authed: Authed, root: PathBuf) -> Self {
        Self {
            srv,
            authed,
            root,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn deny_read_only(&self) -> Result<(), StatusReply> {
        if self.srv.read_only {
            Err(sr(StatusCode::OpUnsupported, "server is in read-only mode"))
        } else {
            Ok(())
        }
    }

    /// Resolve an SFTP path inside the server root, rejecting traversal.
    fn resolve(&self, path: &str) -> Result<PathBuf, StatusReply> {
        let mut out = self.root.clone();
        for comp in path.trim_start_matches('/').split('/') {
            match comp {
                "" | "." => continue,
                ".." => return Err(sr(StatusCode::NoSuchFile, "invalid path")),
                c => out.push(c),
            }
        }
        Ok(out)
    }

    fn insert_handle(&mut self, entry: HandleEntry) -> String {
        self.next_handle += 1;
        let id = self.next_handle.to_string();
        self.handles.insert(id.clone(), entry);
        id
    }

    fn take_handle(&mut self, handle: &str) -> Result<HandleEntry, StatusReply> {
        self.handles
            .remove(handle)
            .ok_or_else(|| sr(StatusCode::NoSuchFile, "invalid handle"))
    }

    fn log_activity(&self, event: &str, path: &str) {
        self.srv.activity.push(
            crate::models::Activity::new(&self.authed.server, event)
                .with_user(Some(self.authed.user.clone()))
                .with_ip(String::new())
                .with_metadata(serde_json::json!({ "path": path })),
        );
    }
}

impl SftpFsHandler for SftpFs {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        sr(StatusCode::OpUnsupported, "operation not supported")
    }

    fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> impl std::future::Future<Output = Result<Version, Self::Error>> + Send {
        async { Ok(Version::new()) }
    }

    fn open(
        &mut self,
        _id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> impl std::future::Future<Output = Result<Handle, Self::Error>> + Send {
        let read = pflags.contains(OpenFlags::READ);
        let write = pflags.contains(OpenFlags::WRITE)
            || pflags.contains(OpenFlags::APPEND)
            || pflags.contains(OpenFlags::CREATE)
            || pflags.contains(OpenFlags::TRUNCATE);
        let create = pflags.contains(OpenFlags::CREATE);
        let truncate = pflags.contains(OpenFlags::TRUNCATE);
        let append = pflags.contains(OpenFlags::APPEND);
        let result = (|| -> Result<String, StatusReply> {
            if write {
                if let Err(e) = self.deny_read_only() {
                    return Err(e);
                }
                let perm = if create { PERM_CREATE } else { PERM_UPDATE };
                if !self.authed.can(perm) {
                    return Err(sr(StatusCode::PermissionDenied, "missing file.create/file.update"));
                }
            }
            let path = self.resolve(&filename)?;
            let mut opts = std::fs::OpenOptions::new();
            opts.read(read).write(write).append(append);
            if create {
                opts.create(true);
            }
            if truncate {
                opts.truncate(true);
            }
            if create && pflags.contains(OpenFlags::EXCLUDE) {
                opts.create_new(true);
            }
            let f = opts.open(&path).map_err(io_err)?;
            self.log_activity(
                if create { "server:sftp.create" } else { "server:sftp.write" },
                &filename,
            );
            Ok(self.insert_handle(HandleEntry::File(f)))
        })();
        async move {
            match result {
                Ok(handle) => Ok(Handle { id: _id, handle }),
                Err(e) => Err(e),
            }
        }
    }

    fn close(
        &mut self,
        _id: u32,
        handle: String,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let _ = self.take_handle(&handle);
        async move { Ok(status_ok(_id)) }
    }

    fn read(
        &mut self,
        _id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> impl std::future::Future<Output = Result<Data, Self::Error>> + Send {
        use std::io::{Read, Seek, SeekFrom};
        let result = (|| -> Result<Data, StatusReply> {
            if !self.authed.can(PERM_READ_CONTENT) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.read-content"));
            }
            match self.handles.get_mut(&handle) {
                Some(HandleEntry::File(f)) => {
                    f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
                    let mut buf = vec![0u8; len as usize];
                    let n = f.read(&mut buf).map_err(io_err)?;
                    buf.truncate(n);
                    if n == 0 {
                        Err(sr(StatusCode::Eof, "end of file"))
                    } else {
                        Ok(Data { id: _id, data: buf })
                    }
                }
                _ => Err(sr(StatusCode::NoSuchFile, "invalid handle")),
            }
        })();
        async move { result }
    }

    fn write(
        &mut self,
        _id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        use std::io::{Seek, SeekFrom, Write};
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_UPDATE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.update"));
            }
            match self.handles.get_mut(&handle) {
                Some(HandleEntry::File(f)) => {
                    f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
                    f.write_all(&data).map_err(io_err)?;
                    Ok(status_ok(_id))
                }
                _ => Err(sr(StatusCode::NoSuchFile, "invalid handle")),
            }
        })();
        async move { result }
    }

    fn lstat(
        &mut self,
        _id: u32,
        path: String,
    ) -> impl std::future::Future<Output = Result<Attrs, Self::Error>> + Send {
        let result = (|| -> Result<Attrs, StatusReply> {
            if !self.authed.can(PERM_READ) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.read"));
            }
            let meta = std::fs::symlink_metadata(self.resolve(&path)?).map_err(io_err)?;
            Ok(Attrs { id: _id, attrs: attrs_of(&meta) })
        })();
        async move { result }
    }

    fn fstat(
        &mut self,
        _id: u32,
        handle: String,
    ) -> impl std::future::Future<Output = Result<Attrs, Self::Error>> + Send {
        let result = match self.handles.get(&handle) {
            Some(HandleEntry::File(f)) => f
                .metadata()
                .map(|m| Attrs { id: _id, attrs: attrs_of(&m) })
                .map_err(io_err),
            _ => Err(sr(StatusCode::NoSuchFile, "invalid handle")),
        };
        async move { result }
    }

    fn setstat(
        &mut self,
        _id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_UPDATE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.update"));
            }
            let p = self.resolve(&path)?;
            if let Some(perms) = attrs.permissions {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(perms)).map_err(io_err)?;
            }
            if let Some(size) = attrs.size {
                let f = std::fs::OpenOptions::new().write(true).open(&p).map_err(io_err)?;
                f.set_len(size).map_err(io_err)?;
            }
            Ok(status_ok(_id))
        })();
        async move { result }
    }

    fn fsetstat(
        &mut self,
        _id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_UPDATE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.update"));
            }
            match self.handles.get_mut(&handle) {
                Some(HandleEntry::File(f)) => {
                    if let Some(perms) = attrs.permissions {
                        f.set_permissions(std::fs::Permissions::from_mode(perms)).map_err(io_err)?;
                    }
                    if let Some(size) = attrs.size {
                        f.set_len(size).map_err(io_err)?;
                    }
                    Ok(status_ok(_id))
                }
                _ => Err(sr(StatusCode::NoSuchFile, "invalid handle")),
            }
        })();
        async move { result }
    }

    fn opendir(
        &mut self,
        _id: u32,
        path: String,
    ) -> impl std::future::Future<Output = Result<Handle, Self::Error>> + Send {
        let result = (|| -> Result<String, StatusReply> {
            if !self.authed.can(PERM_READ) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.read"));
            }
            let dir = std::fs::read_dir(self.resolve(&path)?).map_err(io_err)?;
            let mut entries = Vec::new();
            for entry in dir {
                let entry = entry.map_err(io_err)?;
                let meta = entry.metadata().map_err(io_err)?;
                let fname = entry.file_name().to_string_lossy().to_string();
                entries.push(File {
                    longname: longname_of(&meta, &fname),
                    filename: fname,
                    attrs: attrs_of(&meta),
                });
            }
            entries.sort_by(|a, b| a.filename.cmp(&b.filename));
            Ok(self.insert_handle(HandleEntry::Dir { entries, next: 0 }))
        })();
        async move {
            match result {
                Ok(handle) => Ok(Handle { id: _id, handle }),
                Err(e) => Err(e),
            }
        }
    }

    fn readdir(
        &mut self,
        _id: u32,
        handle: String,
    ) -> impl std::future::Future<Output = Result<Name, Self::Error>> + Send {
        let result = match self.handles.get_mut(&handle) {
            Some(HandleEntry::Dir { entries, next }) => {
                if *next >= entries.len() {
                    Err(sr(StatusCode::Eof, "no more entries"))
                } else {
                    let remaining = entries[*next..].to_vec();
                    *next = entries.len();
                    Ok(Name {
                        id: _id,
                        files: remaining,
                    })
                }
            }
            _ => Err(sr(StatusCode::NoSuchFile, "invalid handle")),
        };
        async move { result }
    }

    fn remove(
        &mut self,
        _id: u32,
        filename: String,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_DELETE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.delete"));
            }
            std::fs::remove_file(self.resolve(&filename)?).map_err(io_err)?;
            self.log_activity("server:sftp.delete", &filename);
            Ok(status_ok(_id))
        })();
        async move { result }
    }

    fn mkdir(
        &mut self,
        _id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_CREATE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.create"));
            }
            std::fs::create_dir(self.resolve(&path)?).map_err(io_err)?;
            Ok(status_ok(_id))
        })();
        async move { result }
    }

    fn rmdir(
        &mut self,
        _id: u32,
        path: String,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_DELETE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.delete"));
            }
            std::fs::remove_dir(self.resolve(&path)?).map_err(io_err)?;
            Ok(status_ok(_id))
        })();
        async move { result }
    }

    fn realpath(
        &mut self,
        _id: u32,
        path: String,
    ) -> impl std::future::Future<Output = Result<Name, Self::Error>> + Send {
        let result = (|| -> Result<Name, StatusReply> {
            let p = self.resolve(&path)?;
            let meta = std::fs::metadata(&p).map_err(io_err)?;
            let canonical = if p == self.root {
                "/".to_string()
            } else {
                format!("/{}", p.strip_prefix(&self.root).unwrap_or(&p).display())
            };
            Ok(Name {
                id: _id,
                files: vec![File {
                    filename: canonical,
                    longname: String::new(),
                    attrs: attrs_of(&meta),
                }],
            })
        })();
        async move { result }
    }

    fn stat(
        &mut self,
        _id: u32,
        path: String,
    ) -> impl std::future::Future<Output = Result<Attrs, Self::Error>> + Send {
        let result = (|| -> Result<Attrs, StatusReply> {
            if !self.authed.can(PERM_READ) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.read"));
            }
            let meta = std::fs::metadata(self.resolve(&path)?).map_err(io_err)?;
            Ok(Attrs { id: _id, attrs: attrs_of(&meta) })
        })();
        async move { result }
    }

    fn rename(
        &mut self,
        _id: u32,
        oldpath: String,
        newpath: String,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_UPDATE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.update"));
            }
            let src = self.resolve(&oldpath)?;
            let dst = self.resolve(&newpath)?;
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::rename(&src, &dst).map_err(io_err)?;
            Ok(status_ok(_id))
        })();
        async move { result }
    }

    fn readlink(
        &mut self,
        _id: u32,
        path: String,
    ) -> impl std::future::Future<Output = Result<Name, Self::Error>> + Send {
        let result = (|| -> Result<Name, StatusReply> {
            if !self.authed.can(PERM_READ) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.read"));
            }
            let target = std::fs::read_link(self.resolve(&path)?).map_err(io_err)?;
            Ok(Name {
                id: _id,
                files: vec![File {
                    filename: target.to_string_lossy().to_string(),
                    longname: String::new(),
                    attrs: FileAttributes::default(),
                }],
            })
        })();
        async move { result }
    }

    fn symlink(
        &mut self,
        _id: u32,
        linkpath: String,
        targetpath: String,
    ) -> impl std::future::Future<Output = Result<Status, Self::Error>> + Send {
        let result = (|| -> Result<Status, StatusReply> {
            self.deny_read_only()?;
            if !self.authed.can(PERM_CREATE) {
                return Err(sr(StatusCode::PermissionDenied, "missing file.create"));
            }
            let link = self.resolve(&linkpath)?;
            std::os::unix::fs::symlink(&targetpath, &link).map_err(io_err)?;
            Ok(status_ok(_id))
        })();
        async move { result }
    }

    fn extended(
        &mut self,
        _id: u32,
        _request: String,
        _data: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Packet, Self::Error>> + Send {
        async { Err(sr(StatusCode::OpUnsupported, "extension not supported")) }
    }
}

/// The SFTP subsystem server: binds an SSH listener and validates every
/// connection against the panel (wings sftp/server.go).
pub struct SftpServer {
    manager: Arc<ServerManager>,
    activity: Arc<ActivityCollector>,
    panel: Arc<tokio::sync::RwLock<crate::remote::PanelClient>>,
    read_only: bool,
    data_dir: PathBuf,
}

impl SftpServer {
    pub fn new(
        manager: Arc<ServerManager>,
        activity: Arc<ActivityCollector>,
        cfg: &SftpConfig,
        data_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let panel = manager.shared().panel.clone();
        Ok(Self {
            manager,
            activity,
            panel,
            read_only: cfg.read_only,
            data_dir,
        })
    }

    /// Path to the SFTP host key (wings: `{data}/.sftp/id_ed25519`).
    fn host_key_path(&self) -> PathBuf {
        self.data_dir.join(".sftp").join("id_ed25519")
    }

    fn load_or_create_host_key(&self) -> anyhow::Result<PrivateKey> {
        let path = self.host_key_path();
        if let Ok(pem) = std::fs::read(&path) {
            let key = russh::keys::decode_secret_key(
                std::str::from_utf8(&pem).map_err(|e| anyhow::anyhow!("host key is not utf8: {e}"))?,
                None,
            )
            .map_err(|e| anyhow::anyhow!("cannot parse host key: {e}"))?;
            return Ok(key);
        }

        let mut rng = russh::keys::key::safe_rng();
        let key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
            .map_err(|e| anyhow::anyhow!("cannot generate host key: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("cannot create sftp key dir: {e}"))?;
        }
        let mut pem = Vec::new();
        encode_pkcs8_pem(&key, &mut pem)
            .map_err(|e| anyhow::anyhow!("cannot encode host key: {e}"))?;
        std::fs::write(&path, &pem)
            .map_err(|e| anyhow::anyhow!("cannot write host key: {e}"))?;
        tracing::info!(path = %path.display(), "generated sftp ed25519 host key");
        Ok(key)
    }

    pub async fn run(self: Arc<Self>, bind: String) -> anyhow::Result<()> {
        let host_key = self.load_or_create_host_key()?;

        let config = Arc::new(SshConfig {
            auth_rejection_time: Duration::from_secs(3),
            keys: vec![host_key],
            ..Default::default()
        });

        let listener = tokio::net::TcpListener::bind(&bind).await
            .map_err(|e| anyhow::anyhow!("cannot bind sftp listener on {bind}: {e}"))?;
        tracing::info!("sftp server listening on {bind}");

        loop {
            let (stream, peer) = listener.accept().await?;
            let srv = self.clone();
            let config = config.clone();
            tokio::spawn(async move {
                let handler = SshHandler::new(srv, peer);
                match russh::server::run_stream(config, stream, handler).await {
                    Ok(session) => {
                        let _ = session.await;
                    }
                    Err(e) => {
                        tracing::debug!(ip = %peer.ip(), error = %e, "sftp handshake failed");
                    }
                }
            });
        }
    }
}