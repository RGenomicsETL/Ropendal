use std::collections::VecDeque;
use std::fmt;
use std::io::SeekFrom;
use std::sync::Arc;

use opendal::raw::{self, oio};
use opendal::{
    Buffer, Capability, EntryMode, Error, ErrorKind, Metadata, Operator, OperatorBuilder,
};
use russh::client;
use russh::keys::known_hosts;
use russh_sftp::client::{SftpSession, error::Error as SftpError};
use russh_sftp::protocol::{OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const SFTP_SCHEME: &str = "sftp";

/// Build the Ropendal password-authenticated SFTP adapter when `password` is
/// supplied. Key/agent-authenticated SFTP remains delegated to OpenDAL.
pub(crate) fn operator_from_pairs(
    scheme: &str,
    pairs: &[(String, String)],
) -> opendal::Result<Option<Operator>> {
    if scheme != SFTP_SCHEME || !pairs.iter().any(|(key, _)| key == "password") {
        return Ok(None);
    }

    let mut endpoint = None;
    let mut root = None;
    let mut user = None;
    let mut password = None;
    let mut known_hosts_strategy = None;
    for (key, value) in pairs {
        match key.as_str() {
            "endpoint" => endpoint = Some(value.as_str()),
            "root" => root = Some(value.as_str()),
            "user" => user = Some(value.as_str()),
            "password" => password = Some(value.as_str()),
            "known_hosts_strategy" => known_hosts_strategy = Some(value.as_str()),
            "key" => {
                return Err(config_error(
                    "SFTP password authentication is mutually exclusive with key",
                ));
            }
            _ => {
                return Err(config_error(
                    "unsupported password SFTP configuration entry",
                ));
            }
        }
    }

    let endpoint = endpoint.ok_or_else(|| config_error("SFTP endpoint is required"))?;
    let endpoint = parse_endpoint(endpoint)?;
    let user = user
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or(endpoint.user)
        .ok_or_else(|| config_error("SFTP user is required"))?;
    let password = password
        .map(str::to_owned)
        .ok_or_else(|| config_error("SFTP password is required"))?;
    // An absent root and an explicit empty root both mean the SFTP login
    // directory. Preserve all other path bytes; remote paths may contain spaces.
    let root = root.unwrap_or_default().to_owned();
    // Strict is an intentional security default, not a convenience fallback.
    let host_key_policy = HostKeyPolicy::parse(known_hosts_strategy.unwrap_or("strict"))?;

    let backend = PasswordSftpBackend::new(
        endpoint.host,
        endpoint.port,
        user,
        password,
        root,
        host_key_policy,
    );
    let op = OperatorBuilder::new(backend).finish();
    Ok(Some(op))
}

fn config_error(message: &str) -> Error {
    Error::new(ErrorKind::ConfigInvalid, message)
}

struct Endpoint {
    host: String,
    port: u16,
    user: Option<String>,
}

fn parse_endpoint(value: &str) -> opendal::Result<Endpoint> {
    let mut authority = value.trim().strip_prefix("ssh://").unwrap_or(value.trim());
    if authority.is_empty() || authority.contains('/') || authority.contains('?') {
        return Err(config_error(
            "SFTP endpoint must be an OpenSSH-style host or ssh:// URL",
        ));
    }

    let user = authority.rsplit_once('@').and_then(|(user, host)| {
        authority = host;
        (!user.is_empty()).then(|| user.to_owned())
    });
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| config_error("SFTP endpoint has an invalid IPv6 host"))?;
        let port = match rest.strip_prefix(':') {
            Some(port) if !port.is_empty() => port
                .parse::<u16>()
                .map_err(|_| config_error("SFTP endpoint has an invalid port"))?,
            Some(_) => return Err(config_error("SFTP endpoint has an invalid port")),
            None => 22,
        };
        (host.to_owned(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return Err(config_error("IPv6 SFTP endpoints must use brackets"));
        }
        if port.is_empty() {
            return Err(config_error("SFTP endpoint has an invalid port"));
        }
        (
            host.to_owned(),
            port.parse::<u16>()
                .map_err(|_| config_error("SFTP endpoint has an invalid port"))?,
        )
    } else {
        (authority.to_owned(), 22)
    };
    if host.is_empty() {
        return Err(config_error("SFTP endpoint host is required"));
    }
    Ok(Endpoint { host, port, user })
}

#[derive(Clone, Copy, Debug)]
enum HostKeyPolicy {
    Strict,
    Accept,
    Add,
}

impl HostKeyPolicy {
    fn parse(value: &str) -> opendal::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "accept" => Ok(Self::Accept),
            "add" => Ok(Self::Add),
            _ => Err(config_error(
                "known_hosts_strategy must be strict, accept, or add",
            )),
        }
    }
}

#[derive(Debug)]
struct HostKeyHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
}

impl client::Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match known_hosts::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true),
            Ok(false) => match self.policy {
                HostKeyPolicy::Strict => Ok(false),
                // Match OpenSSH's `StrictHostKeyChecking=no`: this is an
                // explicit opt-out, not a trust-on-first-use policy.
                HostKeyPolicy::Accept => Ok(true),
                HostKeyPolicy::Add => {
                    Ok(
                        known_hosts::learn_known_hosts(&self.host, self.port, server_public_key)
                            .is_ok(),
                    )
                }
            },
            // `accept` deliberately skips verification. A changed, malformed,
            // or unreadable matching entry is never accepted by `strict`/`add`.
            Err(_) => Ok(matches!(self.policy, HostKeyPolicy::Accept)),
        }
    }
}

struct ConnectedSftp {
    // Keep the SSH transport alive for the lifetime of the SFTP subsystem.
    _handle: client::Handle<HostKeyHandler>,
    sftp: SftpSession,
}

#[derive(Clone)]
struct PasswordSftpBackend {
    host: String,
    port: u16,
    user: String,
    password: String,
    root: String,
    host_key_policy: HostKeyPolicy,
    info: Arc<raw::AccessorInfo>,
}

impl fmt::Debug for PasswordSftpBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordSftpBackend")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("root", &self.root)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl PasswordSftpBackend {
    fn new(
        host: String,
        port: u16,
        user: String,
        password: String,
        root: String,
        host_key_policy: HostKeyPolicy,
    ) -> Self {
        let info = raw::AccessorInfo::default();
        info.set_scheme(SFTP_SCHEME)
            .set_name(&host)
            .set_root(&display_root(&root))
            .set_native_capability(Capability {
                stat: true,
                read: true,
                write: true,
                write_can_multi: true,
                write_can_append: true,
                write_with_if_not_exists: true,
                create_dir: true,
                delete: true,
                list: true,
                list_with_recursive: true,
                copy: true,
                rename: true,
                shared: true,
                ..Default::default()
            });
        Self {
            host,
            port,
            user,
            password,
            root,
            host_key_policy,
            info: Arc::new(info),
        }
    }

    fn remote_path(&self, path: &str) -> String {
        if self.root == "/" {
            return if path.is_empty() {
                "/".to_owned()
            } else {
                format!("/{path}")
            };
        }
        let root = self.root.trim_end_matches('/');
        match (root, path) {
            ("", "") => ".".to_owned(),
            ("", path) => path.to_owned(),
            (root, "") => root.to_owned(),
            (root, path) => format!("{root}/{path}"),
        }
    }

    async fn ensure_directory(&self, sftp: &SftpSession, path: &str) -> opendal::Result<()> {
        let absolute = path.starts_with('/');
        let mut current = if absolute {
            "/".to_owned()
        } else {
            String::new()
        };
        for component in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            current = if current.is_empty() || current == "/" {
                if absolute {
                    format!("/{component}")
                } else {
                    component.to_owned()
                }
            } else {
                format!("{current}/{component}")
            };
            if let Err(err) = sftp.create_dir(current.clone()).await {
                let metadata = sftp
                    .metadata(current.clone())
                    .await
                    .map_err(|_| sftp_error(err, "mkdir", &current))?;
                if !metadata.is_dir() {
                    return Err(Error::new(
                        ErrorKind::NotADirectory,
                        "SFTP path is not a directory",
                    )
                    .with_operation("mkdir")
                    .with_context("path", &current));
                }
            }
        }
        Ok(())
    }

    async fn read_bytes(
        &self,
        path: &str,
        offset: u64,
        size: Option<u64>,
    ) -> opendal::Result<Buffer> {
        let connection = self.connect_with_policy().await?;
        let remote = self.remote_path(path);
        let mut file = connection
            .sftp
            .open(remote.clone())
            .await
            .map_err(|err| sftp_error(err, "read", path))?;
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|err| io_error(err, "read", path))?;
        let mut out = Vec::new();
        let mut remaining = size;
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let wanted = remaining
                .map(|value| value.min(buf.len() as u64) as usize)
                .unwrap_or(buf.len());
            if wanted == 0 {
                break;
            }
            let n = file
                .read(&mut buf[..wanted])
                .await
                .map_err(|err| io_error(err, "read", path))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if let Some(value) = &mut remaining {
                *value = value.saturating_sub(n as u64);
            }
        }
        file.shutdown()
            .await
            .map_err(|err| io_error(err, "read", path))?;
        Ok(Buffer::from(out))
    }

    async fn write_buffer(
        &self,
        path: &str,
        data: Vec<u8>,
        args: &raw::OpWrite,
    ) -> opendal::Result<Metadata> {
        let connection = self.connect_with_policy().await?;
        let remote = self.remote_path(path);
        if let Some((parent, _)) = remote.rsplit_once('/') {
            if !parent.is_empty() {
                self.ensure_directory(&connection.sftp, parent).await?;
            }
        }
        let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
        if args.append() {
            flags |= OpenFlags::APPEND;
        } else if args.if_not_exists() {
            flags |= OpenFlags::EXCLUDE;
        } else {
            flags |= OpenFlags::TRUNCATE;
        }
        let mut file = connection
            .sftp
            .open_with_flags(remote, flags)
            .await
            .map_err(|err| sftp_error(err, "write", path))?;
        file.write_all(&data)
            .await
            .map_err(|err| io_error(err, "write", path))?;
        file.shutdown()
            .await
            .map_err(|err| io_error(err, "write", path))?;
        Ok(Metadata::new(EntryMode::FILE).with_content_length(data.len() as u64))
    }

    async fn list_entries(&self, path: &str, recursive: bool) -> opendal::Result<Vec<oio::Entry>> {
        let connection = self.connect_with_policy().await?;
        let remote = self.remote_path(path);
        let mut pending = vec![(remote, path.trim_end_matches('/').to_owned())];
        let mut entries = Vec::new();
        while let Some((remote_dir, logical_dir)) = pending.pop() {
            let dir = connection
                .sftp
                .read_dir(remote_dir.clone())
                .await
                .map_err(|err| sftp_error(err, "list", &logical_dir))?;
            for entry in dir {
                let name = entry.file_name();
                let source = entry.metadata();
                let is_dir = source.is_dir();
                let mut logical_path = if logical_dir.is_empty() {
                    name
                } else {
                    format!("{logical_dir}/{name}")
                };
                if is_dir {
                    logical_path.push('/');
                }
                entries.push(oio::Entry::new(
                    &logical_path,
                    metadata_from_sftp(
                        &source,
                        if is_dir {
                            EntryMode::DIR
                        } else {
                            EntryMode::FILE
                        },
                    ),
                ));
                if recursive && is_dir {
                    pending.push((
                        format!(
                            "{remote_dir}/{}",
                            logical_path
                                .trim_end_matches('/')
                                .rsplit('/')
                                .next()
                                .unwrap_or_default()
                        ),
                        logical_path.trim_end_matches('/').to_owned(),
                    ));
                }
            }
        }
        Ok(entries)
    }

    async fn copy_file(&self, from: &str, to: &str) -> opendal::Result<()> {
        let bytes = self.read_bytes(from, 0, None).await?;
        self.write_buffer(to, bytes.to_vec(), &raw::OpWrite::default())
            .await
            .map(|_| ())
    }

    async fn rename_path(&self, from: &str, to: &str) -> opendal::Result<()> {
        let connection = self.connect_with_policy().await?;
        let remote_from = self.remote_path(from);
        let remote_to = self.remote_path(to);
        if let Some((parent, _)) = remote_to.rsplit_once('/') {
            if !parent.is_empty() {
                self.ensure_directory(&connection.sftp, parent).await?;
            }
        }
        connection
            .sftp
            .rename(remote_from, remote_to)
            .await
            .map_err(|err| sftp_error(err, "rename", from))
    }

    async fn connect_with_policy(&self) -> opendal::Result<ConnectedSftp> {
        // `HostKeyHandler` holds only endpoint and policy, never credentials.
        let handler = HostKeyHandler {
            host: self.host.clone(),
            port: self.port,
            policy: self.host_key_policy(),
        };
        let config = Arc::new(client::Config::default());
        let mut handle = client::connect(config, (self.host.as_str(), self.port), handler)
            .await
            .map_err(|err| ssh_error(err, "connect"))?;
        let auth = handle
            .authenticate_password(self.user.clone(), self.password.clone())
            .await
            .map_err(|err| ssh_error(err, "authenticate"))?;
        if !auth.success() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "SFTP password authentication failed",
            )
            .with_operation("authenticate"));
        }
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|err| ssh_error(err, "open SFTP channel"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|err| ssh_error(err, "start SFTP subsystem"))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|err| sftp_error(err, "start SFTP subsystem", ""))?;
        Ok(ConnectedSftp {
            _handle: handle,
            sftp,
        })
    }

    fn host_key_policy(&self) -> HostKeyPolicy {
        self.host_key_policy
    }
}

impl raw::Access for PasswordSftpBackend {
    type Reader = Buffer;
    type Writer = PasswordSftpWriter;
    type Lister = PasswordSftpLister;
    type Deleter = oio::OneShotDeleter<PasswordSftpDeleter>;
    type Copier = ();

    fn info(&self) -> Arc<raw::AccessorInfo> {
        self.info.clone()
    }

    async fn create_dir(
        &self,
        path: &str,
        _: raw::OpCreateDir,
    ) -> opendal::Result<raw::RpCreateDir> {
        let connection = self.connect_with_policy().await?;
        self.ensure_directory(&connection.sftp, &self.remote_path(path))
            .await?;
        Ok(raw::RpCreateDir::default())
    }

    async fn stat(&self, path: &str, _: raw::OpStat) -> opendal::Result<raw::RpStat> {
        let connection = self.connect_with_policy().await?;
        let remote = self.remote_path(path);
        let source = connection
            .sftp
            .metadata(remote)
            .await
            .map_err(|err| sftp_error(err, "stat", path))?;
        let fallback = if path.ends_with('/') || path.is_empty() {
            EntryMode::DIR
        } else {
            EntryMode::FILE
        };
        Ok(raw::RpStat::new(metadata_from_sftp(&source, fallback)))
    }

    async fn read(
        &self,
        path: &str,
        args: raw::OpRead,
    ) -> opendal::Result<(raw::RpRead, Self::Reader)> {
        let connection = self.connect_with_policy().await?;
        let remote = self.remote_path(path);
        let source = connection
            .sftp
            .metadata(remote)
            .await
            .map_err(|err| sftp_error(err, "read", path))?;
        drop(connection);
        let range = args.range();
        let bytes = self.read_bytes(path, range.offset(), range.size()).await?;
        Ok((
            raw::RpRead::new(metadata_from_sftp(&source, EntryMode::FILE)),
            bytes,
        ))
    }

    async fn write(
        &self,
        path: &str,
        args: raw::OpWrite,
    ) -> opendal::Result<(raw::RpWrite, Self::Writer)> {
        Ok((
            raw::RpWrite::default(),
            PasswordSftpWriter {
                backend: self.clone(),
                path: path.to_owned(),
                args,
                buffers: Vec::new(),
            },
        ))
    }

    async fn delete(&self) -> opendal::Result<(raw::RpDelete, Self::Deleter)> {
        Ok((
            raw::RpDelete::default(),
            oio::OneShotDeleter::new(PasswordSftpDeleter {
                backend: self.clone(),
            }),
        ))
    }

    async fn list(
        &self,
        path: &str,
        args: raw::OpList,
    ) -> opendal::Result<(raw::RpList, Self::Lister)> {
        let entries = self.list_entries(path, args.recursive()).await?;
        Ok((raw::RpList::default(), PasswordSftpLister::new(entries)))
    }

    async fn copy(
        &self,
        from: &str,
        to: &str,
        _: raw::OpCopy,
        _: raw::OpCopier,
    ) -> opendal::Result<(raw::RpCopy, Self::Copier)> {
        self.copy_file(from, to).await?;
        Ok((raw::RpCopy::default(), ()))
    }

    async fn rename(
        &self,
        from: &str,
        to: &str,
        _: raw::OpRename,
    ) -> opendal::Result<raw::RpRename> {
        self.rename_path(from, to).await?;
        Ok(raw::RpRename::default())
    }
}

struct PasswordSftpWriter {
    backend: PasswordSftpBackend,
    path: String,
    args: raw::OpWrite,
    buffers: Vec<Buffer>,
}

impl oio::Write for PasswordSftpWriter {
    async fn write(&mut self, bs: Buffer) -> opendal::Result<()> {
        self.buffers.push(bs);
        Ok(())
    }

    async fn close(&mut self) -> opendal::Result<Metadata> {
        let mut data = Vec::new();
        for buffer in std::mem::take(&mut self.buffers) {
            data.extend_from_slice(&buffer.to_bytes());
        }
        self.backend
            .write_buffer(&self.path, data, &self.args)
            .await
    }

    async fn abort(&mut self) -> opendal::Result<()> {
        self.buffers.clear();
        Ok(())
    }
}

struct PasswordSftpLister {
    entries: VecDeque<oio::Entry>,
}

impl PasswordSftpLister {
    fn new(entries: Vec<oio::Entry>) -> Self {
        Self {
            entries: entries.into(),
        }
    }
}

impl oio::List for PasswordSftpLister {
    async fn next(&mut self) -> opendal::Result<Option<oio::Entry>> {
        Ok(self.entries.pop_front())
    }
}

#[derive(Clone)]
struct PasswordSftpDeleter {
    backend: PasswordSftpBackend,
}

impl oio::OneShotDelete for PasswordSftpDeleter {
    async fn delete_once(&self, path: String, _: raw::OpDelete) -> opendal::Result<()> {
        let connection = self.backend.connect_with_policy().await?;
        let remote = self.backend.remote_path(&path);
        let result = if path.ends_with('/') {
            connection.sftp.remove_dir(remote).await
        } else {
            connection.sftp.remove_file(remote).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                Ok(())
            }
            Err(err) => Err(sftp_error(err, "delete", &path)),
        }
    }
}

fn display_root(root: &str) -> String {
    if root.is_empty() || root == "/" {
        "/".to_owned()
    } else if root.starts_with('/') {
        format!("{}/", root.trim_end_matches('/'))
    } else {
        format!("/{}/", root.trim_matches('/'))
    }
}

fn metadata_from_sftp(source: &russh_sftp::client::fs::Metadata, fallback: EntryMode) -> Metadata {
    let mode = if source.is_dir() {
        EntryMode::DIR
    } else if source.is_regular() {
        EntryMode::FILE
    } else {
        fallback
    };
    let mut metadata = Metadata::new(mode).with_content_length(source.len());
    if let Ok(modified) = source.modified() {
        if let Ok(timestamp) = raw::Timestamp::try_from(modified) {
            metadata.set_last_modified(timestamp);
        }
    }
    metadata
}

fn sftp_error(error: SftpError, operation: &'static str, path: &str) -> Error {
    let kind = match &error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => ErrorKind::NotFound,
            StatusCode::PermissionDenied => ErrorKind::PermissionDenied,
            StatusCode::OpUnsupported => ErrorKind::Unsupported,
            _ => ErrorKind::Unexpected,
        },
        _ => ErrorKind::Unexpected,
    };
    Error::new(kind, error.to_string())
        .with_operation(operation)
        .with_context("path", path)
}

fn ssh_error(error: russh::Error, operation: &'static str) -> Error {
    Error::new(ErrorKind::Unexpected, error.to_string()).with_operation(operation)
}

fn io_error(error: std::io::Error, operation: &'static str, path: &str) -> Error {
    Error::new(ErrorKind::Unexpected, error.to_string())
        .with_operation(operation)
        .with_context("path", path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_ssh_endpoints() {
        let endpoint = parse_endpoint("alice@sftp.example.test:2222").unwrap();
        assert_eq!(endpoint.host, "sftp.example.test");
        assert_eq!(endpoint.port, 2222);
        assert_eq!(endpoint.user.as_deref(), Some("alice"));

        let endpoint = parse_endpoint("ssh://[2001:db8::1]:2222").unwrap();
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.port, 2222);
        assert_eq!(endpoint.user, None);

        assert!(parse_endpoint("2001:db8::1").is_err());
        assert!(parse_endpoint("ssh://sftp.example.test/path").is_err());
    }

    #[test]
    fn password_backend_debug_is_redacted() {
        let password = "test-only-password-value".to_owned();
        let backend = PasswordSftpBackend::new(
            "sftp.example.test".to_owned(),
            22,
            "alice".to_owned(),
            password.clone(),
            String::new(),
            HostKeyPolicy::Strict,
        );
        let debug = format!("{backend:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&password));
    }

    #[test]
    fn password_and_key_are_rejected_together() {
        let error = operator_from_pairs(
            "sftp",
            &[
                ("endpoint".to_owned(), "sftp.example.test".to_owned()),
                ("user".to_owned(), "alice".to_owned()),
                ("password".to_owned(), "test-only-password-value".to_owned()),
                ("key".to_owned(), "/tmp/id_ed25519".to_owned()),
            ],
        )
        .expect_err("password and key must be mutually exclusive");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
    }
}
