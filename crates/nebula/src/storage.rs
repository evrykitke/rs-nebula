//! File storage — where uploads live and how they are addressed.
//!
//! Two roots, one layout. **Public** files sit under `files.root` and are
//! served read-only at `/public` — anything stored there is reachable
//! without authentication, so it is only for genuinely public assets
//! (a company logo). **Private** files sit under `files.private_root`,
//! which is never mounted: they leave the server only through a handler
//! that has checked authentication and permissions (report artifacts).
//! Both follow one convention:
//! `{root}/{namespace}/{id}/{resource}` — for tenant files the
//! namespace is the tenant's slug, the id is a fresh random key per
//! upload (no collisions, and a new URL on every re-upload so cached
//! copies never go stale), and the resource keeps its sanitized
//! original name so downloads stay meaningful.
//!
//! Handlers talk to [`Storage`] and [`Container`], never to the
//! filesystem: the API is the contract, so an object-store backend can
//! replace the local disk later without touching call sites.
//!
//! Uploads that come from clients must pass through [`guard_image`]
//! before they are stored. See the [`guard`] module for the threat model
//! — briefly: `/public` serves files same-origin, so a file that claims
//! to be an image but carries HTML or an SVG script would be stored XSS.
//! Validation is by content (magic bytes), never by the client-supplied
//! name, and `/public` is served with `nosniff` + a locked-down CSP.

pub mod guard;

pub use guard::{ImageFormat, guard_image};

use crate::config::FilesConfig;
use crate::error::{Error, Result};
use crate::tenancy::TenantRef;
use std::path::PathBuf;
use std::sync::Arc;

/// The application's file store, created by the kernel from `files.root`
/// / `files.private_root` and shared with every module (and, as a request
/// extension, with application handlers).
#[derive(Clone, Debug)]
pub struct Storage {
    public_root: Arc<PathBuf>,
    private_root: Arc<PathBuf>,
}

impl Storage {
    pub fn new(config: &FilesConfig) -> Self {
        Self {
            public_root: Arc::new(PathBuf::from(&config.root)),
            private_root: Arc::new(PathBuf::from(&config.private_root)),
        }
    }

    /// The container for a tenant's public files: `{root}/{slug}/…`,
    /// served at `/public/{slug}/…`. Tenant names are validated at
    /// registration, so this cannot fail.
    pub fn tenant(&self, tenant: &TenantRef) -> Container {
        Container {
            root: self.public_root.clone(),
            namespace: tenant.name.clone(),
            public: true,
        }
    }

    /// The container for a tenant's **private** files — never served at
    /// `/public`; the bytes leave only through a permission-checked
    /// handler (report artifacts and the like).
    pub fn private_tenant(&self, tenant: &TenantRef) -> Container {
        Container {
            root: self.private_root.clone(),
            namespace: tenant.name.clone(),
            public: false,
        }
    }

    /// A container for an arbitrary namespace (host-level assets,
    /// single-tenant deployments). Namespaces follow the tenant-name
    /// shape: 1-64 lowercase letters, digits or dashes.
    pub fn container(&self, namespace: &str) -> Result<Container> {
        validate_namespace(namespace)?;
        Ok(Container {
            root: self.public_root.clone(),
            namespace: namespace.to_string(),
            public: true,
        })
    }

    /// Like [`Storage::container`], but under the private root.
    pub fn private_container(&self, namespace: &str) -> Result<Container> {
        validate_namespace(namespace)?;
        Ok(Container {
            root: self.private_root.clone(),
            namespace: namespace.to_string(),
            public: false,
        })
    }

    /// Delete a stored **public** file by the root-relative path a
    /// previous store answered (also accepts paths from before the
    /// `{slug}/{id}/…` convention). Answers whether a file was actually
    /// removed; the upload's id directory is cleaned up when it ends up
    /// empty.
    pub(crate) async fn remove(&self, path: &str) -> Result<bool> {
        validate_relative(path)?;
        remove_under(&self.public_root, path).await
    }

    /// Delete a stored **private** file. Falls back to the public root
    /// for artifacts stored before the private root existed.
    pub(crate) async fn remove_private(&self, path: &str) -> Result<bool> {
        validate_relative(path)?;
        if remove_under(&self.private_root, path).await? {
            return Ok(true);
        }
        remove_under(&self.public_root, path).await
    }

    /// Read a stored **public** file's bytes by its root-relative path —
    /// for embedding stored assets (e.g. a tenant logo) into generated
    /// documents. The same traversal guard as [`Storage::remove`] applies.
    pub(crate) async fn read(&self, path: &str) -> Result<Vec<u8>> {
        validate_relative(path)?;
        let target = self.public_root.join(path);
        tokio::fs::read(&target)
            .await
            .map_err(|e| Error::internal(format!("could not read the stored file {path:?}: {e}")))
    }

    /// Read a stored **private** file's bytes. Falls back to the public
    /// root so artifacts stored before the private root existed still
    /// download.
    pub(crate) async fn read_private(&self, path: &str) -> Result<Vec<u8>> {
        validate_relative(path)?;
        match tokio::fs::read(self.private_root.join(path)).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::read(self.public_root.join(path)).await.map_err(|e| {
                    Error::internal(format!("could not read the stored file {path:?}: {e}"))
                })
            }
            Err(e) => Err(Error::internal(format!(
                "could not read the stored file {path:?}: {e}"
            ))),
        }
    }
}

/// Delete `{root}/{path}` and, when it ends up empty, the upload's id
/// directory. Answers whether a file was actually removed.
async fn remove_under(root: &std::path::Path, path: &str) -> Result<bool> {
    let target = root.join(path);
    match tokio::fs::remove_file(&target).await {
        Ok(()) => {
            if let Some(dir) = target.parent()
                && dir != root
            {
                let _ = tokio::fs::remove_dir(dir).await;
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::internal(format!(
            "could not remove the stored file {path:?}: {e}"
        ))),
    }
}

fn validate_namespace(namespace: &str) -> Result<()> {
    let ok = !namespace.is_empty()
        && namespace.len() <= 64
        && namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(Error::Validation(format!(
            "a storage namespace must be 1-64 lowercase letters, digits or dashes, got {namespace:?}"
        )));
    }
    Ok(())
}

/// A namespaced slice of the store; hand one to whatever writes files.
#[derive(Clone, Debug)]
pub struct Container {
    root: Arc<PathBuf>,
    namespace: String,
    public: bool,
}

impl Container {
    /// Store an upload under a fresh id: `{namespace}/{id}/{resource}`.
    /// The resource name is sanitized (base name only, unsafe characters
    /// replaced), so it is safe to pass a client-supplied file name.
    pub async fn store(&self, resource: &str, data: &[u8]) -> Result<StoredFile> {
        let resource = sanitize_resource(resource)?;
        let id = new_id();
        let dir = self.root.join(&self.namespace).join(&id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| Error::internal(format!("could not create the upload directory: {e}")))?;
        tokio::fs::write(dir.join(&resource), data)
            .await
            .map_err(|e| Error::internal(format!("could not store {resource:?}: {e}")))?;
        let path = format!("{}/{id}/{resource}", self.namespace);
        Ok(StoredFile {
            url: self.public.then(|| format!("/public/{path}")),
            path,
        })
    }

    /// Delete a file this container stored, by its root-relative path.
    /// Paths outside the namespace are refused.
    pub async fn remove(&self, path: &str) -> Result<bool> {
        let Some(rest) = path.strip_prefix(&self.namespace) else {
            return Err(Error::Validation(format!(
                "path {path:?} is outside the {:?} container",
                self.namespace
            )));
        };
        if !rest.starts_with('/') {
            return Err(Error::Validation(format!(
                "path {path:?} is outside the {:?} container",
                self.namespace
            )));
        }
        validate_relative(path)?;
        remove_under(&self.root, path).await
    }
}

/// A file the store accepted.
#[derive(Clone, Debug)]
pub struct StoredFile {
    /// Root-relative: `{namespace}/{id}/{resource}` — what to persist.
    pub path: String,
    /// Where it is served, for public files: `/public/{path}`. `None`
    /// for private files, which have no direct URL.
    pub url: Option<String>,
}

/// Random 10-character lowercase key — the per-upload directory.
fn new_id() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..10)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Reduce a client-supplied file name to something safe to write and to
/// serve: the base name only, ASCII letters/digits/`.`/`-`/`_`, no
/// leading dots (which also kills `..`), at most 128 bytes.
fn sanitize_resource(name: &str) -> Result<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut clean = String::with_capacity(base.len());
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
            clean.push(c);
        } else if !clean.ends_with('-') {
            clean.push('-');
        }
    }
    while clean.contains("-.") {
        clean = clean.replace("-.", ".");
    }
    let mut clean = clean.trim_matches(['.', '-']).to_string();
    clean.truncate(128);
    if clean.is_empty() {
        return Err(Error::Validation(format!(
            "the file name {name:?} has no usable characters"
        )));
    }
    Ok(clean)
}

/// A stored path must stay under the root: forward-slash segments only,
/// each of them a plain name.
fn validate_relative(path: &str) -> Result<()> {
    let ok = !path.is_empty()
        && !path.contains('\\')
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        });
    if !ok {
        return Err(Error::Validation(format!(
            "{path:?} is not a valid stored-file path"
        )));
    }
    Ok(())
}
