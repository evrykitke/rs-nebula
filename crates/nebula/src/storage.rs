//! Public file storage — where uploads live and how they are addressed.
//!
//! Files sit on the local filesystem under `files.root` and are served
//! read-only at `/public`. Every upload follows one convention:
//! `{files.root}/{namespace}/{id}/{resource}` — for tenant files the
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

/// The application's public file store, created by the kernel from
/// `files.root` and shared with every module (and, as a request
/// extension, with application handlers).
#[derive(Clone, Debug)]
pub struct Storage {
    root: Arc<PathBuf>,
}

impl Storage {
    pub fn new(config: &FilesConfig) -> Self {
        Self {
            root: Arc::new(PathBuf::from(&config.root)),
        }
    }

    /// The container for a tenant's public files: `{root}/{slug}/…`,
    /// served at `/public/{slug}/…`. Tenant names are validated at
    /// registration, so this cannot fail.
    pub fn tenant(&self, tenant: &TenantRef) -> Container {
        Container {
            root: self.root.clone(),
            namespace: tenant.name.clone(),
        }
    }

    /// A container for an arbitrary namespace (host-level assets,
    /// single-tenant deployments). Namespaces follow the tenant-name
    /// shape: 1-64 lowercase letters, digits or dashes.
    pub fn container(&self, namespace: &str) -> Result<Container> {
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
        Ok(Container {
            root: self.root.clone(),
            namespace: namespace.to_string(),
        })
    }

    /// Delete a stored file by the root-relative path a previous store
    /// answered (also accepts paths from before the `{slug}/{id}/…`
    /// convention). Answers whether a file was actually removed; the
    /// upload's id directory is cleaned up when it ends up empty.
    pub(crate) async fn remove(&self, path: &str) -> Result<bool> {
        validate_relative(path)?;
        let target = self.root.join(path);
        match tokio::fs::remove_file(&target).await {
            Ok(()) => {
                if let Some(dir) = target.parent()
                    && dir != self.root.as_path()
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

    /// Read a stored file's bytes by its root-relative path — for embedding
    /// stored assets (e.g. a tenant logo) into generated documents. The
    /// same traversal guard as [`Storage::remove`] applies.
    pub(crate) async fn read(&self, path: &str) -> Result<Vec<u8>> {
        validate_relative(path)?;
        let target = self.root.join(path);
        tokio::fs::read(&target)
            .await
            .map_err(|e| Error::internal(format!("could not read the stored file {path:?}: {e}")))
    }
}

/// A namespaced slice of the store; hand one to whatever writes files.
#[derive(Clone, Debug)]
pub struct Container {
    root: Arc<PathBuf>,
    namespace: String,
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
            url: format!("/public/{path}"),
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
        Storage {
            root: self.root.clone(),
        }
        .remove(path)
        .await
    }
}

/// A file the store accepted.
#[derive(Clone, Debug)]
pub struct StoredFile {
    /// Root-relative: `{namespace}/{id}/{resource}` — what to persist.
    pub path: String,
    /// Where it is served: `/public/{path}`.
    pub url: String,
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
