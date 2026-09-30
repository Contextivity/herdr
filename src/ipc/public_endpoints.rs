//! Private endpoint-generation evidence, independent of startup-ticket authority.
use super::{remove_socket_file_if_owned, socket_file_identity, SocketFileIdentity};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    version: u8,
    path: PathBuf,
    identity: SocketFileIdentity,
    pid: u32,
    lifetime: Option<String>,
    boot: Option<String>,
}

fn companion(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

fn owner_path(path: &Path) -> PathBuf {
    companion(path, ".owner.json")
}

fn private_options() -> OpenOptions {
    crate::platform::private_endpoint_options()
}

fn validate_private_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("endpoint evidence must be a regular file"));
    }
    crate::platform::validate_private_endpoint_file(file)?;
    Ok(())
}

/// Both public endpoints share this OS-held lock. Never unlink its file, or
/// hold admission while waiting for a handoff replacement in another process.
pub(crate) struct Admission {
    _file: File,
}

impl Admission {
    pub(crate) fn lock(api_path: &Path) -> io::Result<Self> {
        let file = Self::open_lock(api_path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }

    pub(crate) fn try_lock(api_path: &Path) -> io::Result<Self> {
        let file = Self::open_lock(api_path)?;
        file.try_lock()
            .map_err(|err| io::Error::new(io::ErrorKind::AddrInUse, err.to_string()))?;
        Ok(Self { _file: file })
    }

    fn open_lock(api_path: &Path) -> io::Result<File> {
        if let Some(parent) = api_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = private_options()
            .create(true)
            .truncate(false)
            .open(companion(api_path, ".owner.lock"))?;
        validate_private_file(&file)?;
        Ok(file)
    }

    /// A live record reserves even an absent pathname during handoff. Only a
    /// replacement authenticated through the private handoff may use it.
    pub(crate) fn prepare(&self, path: &Path, handoff_import: bool) -> io::Result<()> {
        if let Some(identity) = self.check(path, handoff_import)? {
            self.reclaim(path, &identity)?;
        }
        Ok(())
    }

    pub(crate) fn prepare_pair(
        &self,
        api: &Path,
        client: &Path,
        handoff_import: bool,
    ) -> io::Result<()> {
        // Validate the entire pair before reclaiming either endpoint. A foreign
        // client path must not cause partial publication or session restoration.
        let api_identity = self.check(api, handoff_import)?;
        let client_identity = self.check(client, handoff_import)?;
        for (path, identity) in [(api, api_identity), (client, client_identity)] {
            if let Some(identity) = identity {
                self.reclaim(path, &identity)?;
            }
        }
        Ok(())
    }

    fn check(&self, path: &Path, handoff_import: bool) -> io::Result<Option<SocketFileIdentity>> {
        let busy = || {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "public socket ownership is not available at {}",
                    path.display()
                ),
            )
        };
        let owner = read_owner(path).map_err(|_| busy())?;
        let identity = match socket_file_identity(path) {
            Ok(identity) => Some(identity),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(busy()),
        };
        let Some(identity) = identity else {
            if let Some(owner) = owner {
                let own = owner.is_current_process();
                if !own
                    && !handoff_import
                    && !crate::platform::public_endpoint_owner_exited(owner.pid)
                {
                    return Err(busy());
                }
            }
            return Ok(None);
        };
        let Some(owner) = owner else {
            return Err(busy());
        };
        // Lifetime and boot evidence are mandatory for cold reclamation. On
        // platforms lacking that evidence, an existing pathname remains held.
        if owner.identity != identity
            || owner.lifetime.as_deref().is_none_or(str::is_empty)
            || owner.boot.is_none()
            || owner.boot != crate::platform::public_endpoint_boot_identity()
            || !crate::platform::public_endpoint_owner_exited(owner.pid)
        {
            return Err(busy());
        }
        Ok(Some(identity))
    }

    fn reclaim(&self, path: &Path, identity: &SocketFileIdentity) -> io::Result<()> {
        // The OS lock excludes cooperating starts. The final identity check
        // and non-reclaiming bind protect a foreign pathname replacement.
        remove_socket_file_if_owned(path, identity)?;
        tracing::info!(event="server.socket.reclaimed", path=%path.display(),
            "reclaimed socket matching a recorded exited owner");
        Ok(())
    }

    /// Publication follows non-reclaiming bind. A crash or error before
    /// publication leaves the unproven pathname held closed on the next start.
    pub(crate) fn publish(&self, path: &Path, identity: &SocketFileIdentity) -> io::Result<()> {
        let owner = Owner {
            version: 1,
            path: path.to_owned(),
            identity: identity.clone(),
            pid: std::process::id(),
            lifetime: crate::platform::startup_process_lifetime(std::process::id()),
            boot: crate::platform::public_endpoint_boot_identity(),
        };
        let destination = owner_path(path);
        let temporary = companion(
            path,
            &format!(
                ".owner.{}.{}.tmp",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(io::Error::other)?
                    .as_nanos()
            ),
        );
        let mut file = private_options().create_new(true).open(&temporary)?;
        let result = (|| {
            file.write_all(&serde_json::to_vec(&owner)?)?;
            file.sync_all()?;
            drop(file);
            if socket_file_identity(path)? != *identity {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "endpoint generation changed before publication",
                ));
            }
            fs::rename(&temporary, destination)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Retain the record while handoff temporarily vacates the pathname.
    pub(crate) fn unlink_owned(
        &self,
        path: &Path,
        identity: &SocketFileIdentity,
    ) -> io::Result<()> {
        let owner =
            read_owner(path)?.ok_or_else(|| io::Error::other("endpoint owner record missing"))?;
        if owner.identity != *identity
            || !owner.is_current_process()
            || socket_file_identity(path)? != *identity
        {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "endpoint generation ownership lost",
            ));
        }
        remove_socket_file_if_owned(path, identity)
    }

    /// An old handle cannot remove a replacement's record or pathname.
    pub(crate) fn cleanup(&self, path: &Path, identity: &SocketFileIdentity) -> io::Result<()> {
        let Some(owner) = read_owner(path)? else {
            return Ok(());
        };
        if owner.identity != *identity || !owner.is_current_process() {
            return Ok(());
        }
        // A mismatch means another generation owns the name. Leave its record.
        match socket_file_identity(path) {
            Ok(current) if current != *identity => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
            Ok(_) => remove_socket_file_if_owned(path, identity)?,
        }
        fs::remove_file(owner_path(path))
    }
}

impl Owner {
    fn is_current_process(&self) -> bool {
        self.pid == std::process::id()
            && self.lifetime == crate::platform::startup_process_lifetime(self.pid)
            && self.boot == crate::platform::public_endpoint_boot_identity()
    }
}

fn read_owner(path: &Path) -> io::Result<Option<Owner>> {
    let mut file = match private_options().open(owner_path(path)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    validate_private_file(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err(io::Error::other("endpoint owner record too large"));
    }
    let owner: Owner = serde_json::from_slice(&bytes)?;
    if owner.version != 1 || owner.path != path || owner.pid == 0 || owner.pid > i32::MAX as u32 {
        return Err(io::Error::other(
            "endpoint owner record identity is invalid",
        ));
    }
    Ok(Some(owner))
}
