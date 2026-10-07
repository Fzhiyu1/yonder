//! Local sockets shared by all supervisor kinds (PTY and chat).
//!
//! Unix: domain socket `<dir>/sup.sock`; the session dir is chmod 0700 because not every
//! platform supports fchmod on sockets. Windows: named pipe `yonder-sup-<id>` whose DACL
//! only admits the owner and SYSTEM.

use std::path::Path;

use anyhow::{Context, Result};
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::ListenerOptions;

/// Bind the supervisor socket for session `id` in `dir`. Fails if a live supervisor
/// already owns it (a stale Unix socket file from a crashed supervisor is replaced).
pub fn bind(dir: &Path, id: &str) -> Result<Listener> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    bind_named(&crate::socket_name(dir, id))
}

/// Bind a private local socket. `name` is a filesystem path on Unix (its directory should
/// be 0700) and a pipe name on Windows (DACL: owner and SYSTEM only).
pub fn bind_named(name_str: &str) -> Result<Listener> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        use std::os::unix::fs::PermissionsExt;
        let path = Path::new(name_str);
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_err() {
            let _ = std::fs::remove_file(path);
        }
        let name = name_str.to_fs_name::<GenericFilePath>()?;
        let l = ListenerOptions::new()
            .name(name)
            .create_tokio()
            .with_context(|| format!("bind socket {name_str}"))?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        Ok(l)
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let name = name_str.to_ns_name::<GenericNamespaced>()?;
        // Owner (current user) and SYSTEM only.
        let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)").unwrap();
        let mut opts = ListenerOptions::new().name(name);
        match SecurityDescriptor::deserialize(&sddl) {
            Ok(sd) => opts = opts.security_descriptor(sd),
            Err(e) => eprintln!("security descriptor: {e}"),
        }
        opts.create_tokio().with_context(|| format!("bind pipe {name_str}"))
    }
}

/// Connect to the supervisor socket of session `id` in `dir`.
pub async fn connect(dir: &Path, id: &str) -> std::io::Result<Stream> {
    connect_named(&crate::socket_name(dir, id)).await
}

/// Connect to a socket bound with [`bind_named`].
pub async fn connect_named(name_str: &str) -> std::io::Result<Stream> {
    #[cfg(unix)]
    let name = {
        use interprocess::local_socket::GenericFilePath;
        name_str.to_fs_name::<GenericFilePath>()?
    };
    #[cfg(windows)]
    let name = {
        use interprocess::local_socket::GenericNamespaced;
        name_str.to_ns_name::<GenericNamespaced>()?
    };
    Stream::connect(name).await
}

/// Remove the Unix socket file after the supervisor stopped listening.
pub fn cleanup(dir: &Path) {
    #[cfg(unix)]
    let _ = std::fs::remove_file(dir.join(crate::SOCKET_FILE));
    #[cfg(windows)]
    let _ = dir;
}
