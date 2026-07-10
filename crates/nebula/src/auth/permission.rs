//! Permission definitions, ASP.NET Zero style: hierarchical dot-named
//! permissions (`Pages.Administration.Users.Edit`) defined in code by
//! modules, granted in the database to roles and overridden per user.
//!
//! Definitions are a tree for UIs to render, flat names for checks.
//! Granting a parent does not implicitly grant its children — every
//! grant is explicit, so an admin screen shows exactly what it does.

use crate::error::{Error, Result};
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PermissionDef {
    /// Dot-separated unique name, e.g. `Pages.Sales.Invoices.Post`.
    pub name: String,
    pub display_name: String,
    /// `no_recursion` stops utoipa's schema builder from recursing into
    /// the self-referential type forever.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(no_recursion)]
    pub children: Vec<PermissionDef>,
}

impl PermissionDef {
    pub fn new(name: &str, display_name: &str) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            children: Vec::new(),
        }
    }

    pub fn child(mut self, child: PermissionDef) -> Self {
        self.children.push(child);
        self
    }
}

/// All permissions defined by the application's modules, built by the
/// kernel at boot and shared through request extensions.
#[derive(Debug, Default)]
pub struct Registry {
    tree: Vec<PermissionDef>,
    names: HashSet<String>,
}

impl Registry {
    pub fn build(definitions: Vec<PermissionDef>) -> Result<Self> {
        let mut registry = Registry {
            tree: definitions,
            names: HashSet::new(),
        };
        let tree = registry.tree.clone();
        for def in &tree {
            registry.collect(def)?;
        }
        Ok(registry)
    }

    fn collect(&mut self, def: &PermissionDef) -> Result<()> {
        let valid = !def.name.is_empty()
            && def
                .name
                .split('.')
                .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric()));
        if !valid {
            return Err(Error::internal(format!(
                "invalid permission name {:?}: dot-separated alphanumeric segments required",
                def.name
            )));
        }
        if !self.names.insert(def.name.clone()) {
            return Err(Error::internal(format!(
                "permission {:?} is defined twice",
                def.name
            )));
        }
        for child in &def.children {
            self.collect(child)?;
        }
        Ok(())
    }

    pub fn tree(&self) -> &[PermissionDef] {
        &self.tree
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn all_names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Permission names the auth module defines for its own endpoints.
pub mod names {
    pub const ADMINISTRATION: &str = "Pages.Administration";
    pub const USERS: &str = "Pages.Administration.Users";
    pub const USERS_VIEW: &str = "Pages.Administration.Users.View";
    pub const USERS_CREATE: &str = "Pages.Administration.Users.Create";
    pub const USERS_EDIT: &str = "Pages.Administration.Users.Edit";
    pub const USERS_DELETE: &str = "Pages.Administration.Users.Delete";
    pub const USERS_PERMISSIONS: &str = "Pages.Administration.Users.Permissions";
    pub const ROLES: &str = "Pages.Administration.Roles";
    pub const ROLES_VIEW: &str = "Pages.Administration.Roles.View";
    pub const ROLES_CREATE: &str = "Pages.Administration.Roles.Create";
    pub const ROLES_EDIT: &str = "Pages.Administration.Roles.Edit";
    pub const ROLES_DELETE: &str = "Pages.Administration.Roles.Delete";
    pub const TENANT_SETTINGS: &str = "Pages.Administration.Tenant.Settings";
}

/// The administration permission tree contributed by [`crate::auth::AuthModule`].
pub fn administration_tree() -> PermissionDef {
    use names::*;
    PermissionDef::new(ADMINISTRATION, "Administration")
        .child(
            PermissionDef::new(USERS, "User management")
                .child(PermissionDef::new(USERS_VIEW, "View users"))
                .child(PermissionDef::new(USERS_CREATE, "Create users"))
                .child(PermissionDef::new(USERS_EDIT, "Edit users"))
                .child(PermissionDef::new(USERS_DELETE, "Delete users"))
                .child(PermissionDef::new(
                    USERS_PERMISSIONS,
                    "Manage user roles and permissions",
                )),
        )
        .child(
            PermissionDef::new(ROLES, "Role management")
                .child(PermissionDef::new(ROLES_VIEW, "View roles"))
                .child(PermissionDef::new(ROLES_CREATE, "Create roles"))
                .child(PermissionDef::new(ROLES_EDIT, "Edit role permissions"))
                .child(PermissionDef::new(ROLES_DELETE, "Delete roles")),
        )
        .child(
            PermissionDef::new("Pages.Administration.Tenant", "Tenant administration")
                .child(PermissionDef::new(TENANT_SETTINGS, "Tenant settings")),
        )
}
