//! Proof of concept: the public file store — the
//! `{namespace}/{id}/{resource}` layout, client file-name sanitization
//! and traversal safety. Runs entirely on a temp directory.

use nebula::Storage;
use nebula::config::FilesConfig;
use std::path::PathBuf;

fn temp_storage() -> (Storage, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "nebula-storage-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Storage::new(&FilesConfig {
        root: root.to_string_lossy().to_string(),
    });
    (storage, root)
}

#[tokio::test]
async fn stores_under_namespace_id_resource() {
    let (storage, root) = temp_storage();
    let container = storage.container("acme").unwrap();

    let first = container
        .store("Quarterly Report (final).PDF", b"pdf bytes")
        .await
        .unwrap();
    let segments: Vec<&str> = first.path.split('/').collect();
    assert_eq!(segments.len(), 3, "layout is namespace/id/resource: {}", first.path);
    assert_eq!(segments[0], "acme");
    assert_eq!(segments[1].len(), 10, "a 10-character upload id");
    assert_eq!(segments[2], "Quarterly-Report-final.PDF", "sanitized, meaning kept");
    assert_eq!(first.url, format!("/public/{}", first.path));
    assert_eq!(std::fs::read(root.join(&first.path)).unwrap(), b"pdf bytes");

    // Same resource again: a fresh id, never an overwrite.
    let second = container.store("Quarterly Report (final).PDF", b"v2").await.unwrap();
    assert_ne!(second.path, first.path);

    // Removal deletes the file and its now-empty id directory.
    assert!(container.remove(&first.path).await.unwrap());
    assert!(!root.join(&first.path).exists());
    assert!(!root.join(segments[0]).join(segments[1]).exists(), "empty id dir is cleaned up");
    assert!(!container.remove(&first.path).await.unwrap(), "second removal finds nothing");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn hostile_names_cannot_escape_the_root() {
    let (storage, root) = temp_storage();
    let container = storage.container("acme").unwrap();

    // Traversal in the file name is reduced to its base name.
    let stored = container.store("../../../evil.sh", b"#!").await.unwrap();
    assert!(stored.path.starts_with("acme/"), "stayed in the container: {}", stored.path);
    assert!(stored.path.ends_with("/evil.sh"));
    assert!(root.join(&stored.path).exists());

    // Names with nothing usable left are refused outright.
    container.store("..", b"x").await.unwrap_err();
    container.store("???", b"x").await.unwrap_err();

    // Namespaces follow the tenant-name shape.
    storage.container("../etc").unwrap_err();
    storage.container("").unwrap_err();
    storage.container("Acme").unwrap_err();

    // Removal refuses paths outside the container or with traversal.
    container.remove("other/abcdefghij/file.txt").await.unwrap_err();
    container.remove("acme/../secret").await.unwrap_err();
    container.remove("acme/id/..\\secret").await.unwrap_err();

    let _ = std::fs::remove_dir_all(&root);
}
