//! Named bundles of permissions.
//!
//! The capability flags themselves live in `pravera-core`, because the wire
//! protocol needs to name them without depending on the credential store. Roles
//! are an authentication concept and stay here.

use serde::{Deserialize, Serialize};

pub use pravera_core::Permission;

/// A named bundle of permissions.
///
/// The three built-ins cover almost every real need; custom roles exist because
/// "almost" is not "every".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub permissions: Permission,
    /// Built-in roles cannot be deleted, so a host can never be left with no
    /// way to grant access.
    pub builtin: bool,
}

impl Role {
    pub fn custom(name: impl Into<String>, permissions: Permission) -> Self {
        Role {
            name: name.into(),
            permissions,
            builtin: false,
        }
    }

    /// Look, do not touch.
    pub fn viewer() -> Self {
        Role {
            name: "viewer".into(),
            permissions: Permission::VIEW,
            builtin: true,
        }
    }

    /// Everyday remote use: drive the machine, share a clipboard, hear it, and
    /// move between monitors. Deliberately excludes file transfer, elevation
    /// and user management.
    pub fn operator() -> Self {
        Role {
            name: "operator".into(),
            permissions: Permission::VIEW
                | Permission::CONTROL
                | Permission::CLIPBOARD_READ
                | Permission::CLIPBOARD_WRITE
                | Permission::AUDIO
                | Permission::MULTI_MONITOR,
            builtin: true,
        }
    }

    /// Everything.
    pub fn admin() -> Self {
        Role {
            name: "admin".into(),
            permissions: Permission::all(),
            builtin: true,
        }
    }

    pub fn builtins() -> Vec<Role> {
        vec![Role::viewer(), Role::operator(), Role::admin()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_can_look_but_not_touch() {
        let p = Role::viewer().permissions;
        assert!(p.allows(Permission::VIEW));
        assert!(!p.allows(Permission::CONTROL));
        assert!(!p.allows(Permission::FILE_READ));
        assert!(!p.allows(Permission::ADMIN));
    }

    #[test]
    fn an_operator_drives_the_desktop_but_cannot_move_files_or_elevate() {
        let p = Role::operator().permissions;
        assert!(p.allows(Permission::VIEW | Permission::CONTROL));
        assert!(p.allows(Permission::AUDIO));
        assert!(
            !p.allows(Permission::FILE_READ),
            "file transfer is a separate grant"
        );
        assert!(!p.allows(Permission::FILE_WRITE));
        assert!(
            !p.allows(Permission::ELEVATE),
            "driving the desktop must not imply approving UAC"
        );
        assert!(!p.allows(Permission::ADMIN));
    }

    #[test]
    fn an_admin_holds_every_flag() {
        let p = Role::admin().permissions;
        for flag in Permission::ALL {
            assert!(p.allows(flag), "admin is missing {}", flag.label());
        }
    }

    #[test]
    fn elevation_is_independent_of_control() {
        // Someone may drive the desktop without being able to approve UAC, and
        // the reverse combination must also be expressible.
        let control_only = Permission::VIEW | Permission::CONTROL;
        assert!(!control_only.allows(Permission::ELEVATE));

        let with_elevation = control_only | Permission::ELEVATE;
        assert!(with_elevation.allows(Permission::ELEVATE));
    }

    #[test]
    fn clipboard_directions_are_separately_grantable() {
        let read_only = Permission::VIEW | Permission::CLIPBOARD_READ;
        assert!(read_only.allows(Permission::CLIPBOARD_READ));
        assert!(!read_only.allows(Permission::CLIPBOARD_WRITE));
    }

    #[test]
    fn a_role_without_view_is_not_a_usable_session() {
        assert!(!Permission::CONTROL.is_usable_session());
        assert!(Role::viewer().permissions.is_usable_session());
    }

    #[test]
    fn builtin_roles_are_marked_so_they_cannot_be_deleted() {
        for role in Role::builtins() {
            assert!(role.builtin, "{} must be undeletable", role.name);
        }
        assert!(!Role::custom("contractor", Permission::VIEW).builtin);
    }

    #[test]
    fn permissions_round_trip_through_serde() {
        let original = Role::operator().permissions;
        let json = serde_json::to_string(&original).unwrap();
        let back: Permission = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn every_flag_has_a_distinct_label_for_the_matrix() {
        let mut labels: Vec<&str> = Permission::ALL.iter().map(|p| p.label()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), before, "two permissions share a label");
    }
}
