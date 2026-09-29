use std::path::PathBuf;

use crate::error::{Error, Result};

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "Pravera";
const APPLICATION: &str = "Pravera";

fn dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .ok_or_else(|| Error::Config("no home directory available for this user".into()))
}

/// Per-user configuration: the UI preferences and pinned host keys.
pub fn config_dir() -> Result<PathBuf> {
    Ok(dirs()?.config_dir().to_path_buf())
}

/// Per-user state: known-hosts store and session history.
pub fn data_dir() -> Result<PathBuf> {
    Ok(dirs()?.data_dir().to_path_buf())
}

/// Machine-wide state owned by the privileged service: the device identity key
/// and the user database.
///
/// Deliberately not under any user profile. The service runs as SYSTEM/root and
/// must reach this before anyone has logged in.
pub fn service_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        root.join("Pravera")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/var/lib/pravera")
    }
}

/// The device long-term ed25519 private key.
pub fn identity_file() -> PathBuf {
    service_data_dir().join("identity.key")
}

/// SQLite database of Pravera users, roles and permissions.
pub fn users_db() -> PathBuf {
    service_data_dir().join("users.db")
}

/// Host public keys this machine has accepted, for TOFU verification.
pub fn known_hosts_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("known_hosts"))
}

/// Where this machine's own identity and accounts actually live.
///
/// Machine-wide when it can be, per-user when it cannot. The difference
/// matters for exactly one reason: a service starting Pravera before anybody
/// has signed in has no user profile to read, so anything it needs has to sit
/// outside every profile. That is `C:\ProgramData\Pravera`, and the service is
/// why it exists.
///
/// The rule is written so the machine can never end up with two identities:
///
/// 1. if the machine-wide file is already there, it wins, always — even if
///    this process could not write to it, because reading is what identity
///    needs;
/// 2. otherwise, if the machine-wide directory can be made, anything already
///    in the per-user location moves up into it, so switching to the service
///    keeps the device ID other machines have pinned;
/// 3. otherwise the per-user location is used, which is what happens on a
///    machine where Pravera has never been elevated.
///
/// Falling back silently would be wrong if it could split the identity, and
/// rule 1 is what stops it: once the machine-wide file exists, no later run
/// can quietly go back to a per-user copy of it.
pub fn machine_dir() -> Result<PathBuf> {
    let shared = service_data_dir();

    // Rule 1. Asked about the directory as a whole rather than one file at a
    // time: resolving each file on its own lets the key end up in one place
    // and the accounts in another, and a service that can prove this machine's
    // identity but has nobody to let in is not a working machine.
    if MACHINE_FILES.iter().any(|name| shared.join(name).is_file()) {
        return Ok(shared);
    }

    let personal = data_dir()?;

    // Creating the directory is also the test for whether this process may
    // write there: an unelevated Pravera fails here and takes the per-user
    // path, rather than being asked to guess about elevation up front and
    // guessing wrong.
    if std::fs::create_dir_all(&shared).is_err() {
        return Ok(personal);
    }
    secure_directory(&shared);

    // Rule 2: carry the existing identity up rather than minting a new one.
    // All of it or none of it — a half-finished move is the split this whole
    // function exists to prevent, so anything that fails part-way leaves the
    // originals in place and keeps using them.
    for name in MACHINE_FILES {
        let from = personal.join(name);
        if from.is_file() && std::fs::copy(&from, shared.join(name)).is_err() {
            for undo in MACHINE_FILES {
                let _ = std::fs::remove_file(shared.join(undo));
            }
            return Ok(personal);
        }
    }
    Ok(shared)
}

/// Everything that belongs to the machine rather than to a person, and so
/// has to move as one set. See [`machine_dir`].
const MACHINE_FILES: &[&str] = &["device.key", "accounts.json"];

/// One of the machine's own files, wherever the machine keeps them.
pub fn machine_file(name: &str) -> Result<PathBuf> {
    debug_assert!(
        MACHINE_FILES.contains(&name),
        "{name} is not listed in MACHINE_FILES, so it would not be migrated with the rest"
    );
    Ok(machine_dir()?.join(name))
}

/// This machine's own long-term key: what its device ID is derived from, and
/// what the far end's TLS handshake proves.
pub fn device_key_file() -> Result<PathBuf> {
    machine_file("device.key")
}

/// The accounts other machines sign in with. Argon2id hashes, never passwords.
pub fn accounts_file() -> Result<PathBuf> {
    machine_file("accounts.json")
}

/// Keep everyone but SYSTEM and the administrators out of the shared
/// directory.
///
/// `ProgramData` hands its children an inherited entry granting every local
/// user read access. A device private key and a file of password hashes
/// sitting under that would be readable by any account on the machine, which
/// would make the machine-wide location strictly worse than the per-user one
/// it replaces. So the inherited entries are dropped and replaced with two.
///
/// A failure here is deliberately quiet and deliberately not fatal: on a
/// filesystem with no ACLs at all there is nothing to set, and the alternative
/// — refusing to start — helps nobody.
#[cfg(windows)]
fn secure_directory(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::BOOL;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    // `PAI` protects the list from inheriting anything; the two entries are
    // full access for `SY` (Local System) and `BA` (Builtin Administrators),
    // inherited by everything created underneath.
    const SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    let sddl: Vec<u16> = SDDL.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .is_err()
    {
        return;
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let read =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };

    if read.is_ok() && present.as_bool() {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // Returns a plain error code rather than a Result. Nothing useful can
        // be done with a failure here — see this function's doc.
        let _ = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            )
        };
    }

    // The descriptor was allocated by the conversion above and is the caller's
    // to free. The DACL points into it, so this happens after the last use.
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
}

#[cfg(not(windows))]
fn secure_directory(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_directory_is_outside_every_user_profile() {
        // The whole reason it exists: a service reads this before anybody has
        // signed in, so it cannot be under a profile that may not be loaded.
        let shared = service_data_dir();
        if let Ok(personal) = data_dir() {
            assert!(
                !shared.starts_with(&personal),
                "{shared:?} is inside {personal:?}"
            );
        }
        // Windows names the directory after the product; the Linux convention
        // is lowercase package names under /var/lib.
        #[cfg(windows)]
        assert!(shared.ends_with("Pravera"));
        #[cfg(not(windows))]
        assert!(shared.ends_with("pravera"));
    }

    #[test]
    fn the_key_and_the_accounts_sit_together() {
        // Both are machine state and both are needed before a sign-in. Having
        // them resolve differently would mean a service that could prove the
        // machine's identity but had nobody to let in.
        let (Ok(key), Ok(accounts)) = (device_key_file(), accounts_file()) else {
            return;
        };
        assert_eq!(key.parent(), accounts.parent());
    }
}
