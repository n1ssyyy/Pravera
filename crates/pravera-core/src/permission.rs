//! What a connected user is allowed to do.
//!
//! Lives in `pravera-core` rather than in `pravera-auth` because both ends of
//! the wire need it: the host decides a permission set during authentication,
//! and the client is told what it was granted so it can grey out controls it
//! does not have. Keeping it here means `pravera-proto` can name the type
//! without dragging a credential store, SQLite and Argon2 into every client.
//!
//! Being *told* a permission is not the same as *having* one. The client copy
//! is a display hint. Enforcement happens host-side in `pravera-host` at
//! message dispatch, so a client patched to believe it holds ADMIN still gets
//! nothing.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Capabilities that can be granted to a Pravera user.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Permission: u32 {
        /// See the remote screen. Every session needs at least this.
        const VIEW            = 1 << 0;
        /// Inject mouse and keyboard input.
        const CONTROL         = 1 << 1;
        /// Read the remote clipboard into the local one.
        const CLIPBOARD_READ  = 1 << 2;
        /// Push the local clipboard to the remote machine.
        const CLIPBOARD_WRITE = 1 << 3;
        /// Download files from the remote machine.
        const FILE_READ       = 1 << 4;
        /// Upload files to the remote machine.
        const FILE_WRITE      = 1 << 5;
        /// Hear remote system audio.
        const AUDIO           = 1 << 6;
        /// Enumerate and switch between remote displays.
        const MULTI_MONITOR   = 1 << 7;
        /// Interact with UAC prompts and the secure desktop. Separate from
        /// CONTROL because letting someone drive the desktop is a much smaller
        /// grant than letting them approve elevation.
        const ELEVATE         = 1 << 8;
        /// Manage users, roles and host settings remotely.
        const ADMIN           = 1 << 9;
    }
}

impl Permission {
    /// Human-readable name for a single flag, for the permission matrix UI.
    pub fn label(self) -> &'static str {
        match self {
            Permission::VIEW => "View screen",
            Permission::CONTROL => "Mouse and keyboard",
            Permission::CLIPBOARD_READ => "Read clipboard",
            Permission::CLIPBOARD_WRITE => "Write clipboard",
            Permission::FILE_READ => "Download files",
            Permission::FILE_WRITE => "Upload files",
            Permission::AUDIO => "Hear audio",
            Permission::MULTI_MONITOR => "Switch displays",
            Permission::ELEVATE => "Approve UAC prompts",
            Permission::ADMIN => "Manage users",
            _ => "Multiple permissions",
        }
    }

    /// Every flag, in the order the matrix should display them.
    pub const ALL: [Permission; 10] = [
        Permission::VIEW,
        Permission::CONTROL,
        Permission::CLIPBOARD_READ,
        Permission::CLIPBOARD_WRITE,
        Permission::FILE_READ,
        Permission::FILE_WRITE,
        Permission::AUDIO,
        Permission::MULTI_MONITOR,
        Permission::ELEVATE,
        Permission::ADMIN,
    ];

    /// True when every capability in `required` is held.
    pub fn allows(self, required: Permission) -> bool {
        self.contains(required)
    }

    /// A session with no VIEW can see nothing, so it is not a session at all.
    /// Used to reject a role that would connect and then show a black screen.
    pub fn is_usable_session(self) -> bool {
        self.contains(Permission::VIEW)
    }
}
