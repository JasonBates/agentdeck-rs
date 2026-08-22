use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use super::HttpBuildError;
use crate::assets::{INDEX_HTML, SETUP_HTML};

#[derive(Clone)]
pub(super) struct AssetSource {
    root: Option<Arc<PathBuf>>,
}

impl AssetSource {
    /// Development overrides are intentionally local-only and must live in a
    /// trusted directory. Component checks reject symlinks and group/world
    /// writable paths. This narrows, but does not claim to eliminate, the
    /// canonicalize/open race inherent in portable path-based `std` APIs.
    pub(super) fn new(root: Option<&Path>, loopback: bool) -> Result<Self, HttpBuildError> {
        let root = match root {
            Some(path) => {
                if !loopback {
                    return Err(HttpBuildError::InvalidPublicDirectory(
                        "development overrides are available only on a loopback listener"
                            .to_owned(),
                    ));
                }
                if !root_components_have_no_symlinks(path) {
                    return Err(HttpBuildError::InvalidPublicDirectory(format!(
                        "{} contains a symlink or parent traversal component",
                        path.display()
                    )));
                }
                let source_metadata = std::fs::symlink_metadata(path).map_err(|error| {
                    HttpBuildError::InvalidPublicDirectory(format!("{}: {error}", path.display()))
                })?;
                if source_metadata.file_type().is_symlink()
                    || !trusted_permissions(&source_metadata)
                {
                    return Err(HttpBuildError::InvalidPublicDirectory(format!(
                        "{} must be a trusted, non-symlink directory not writable by group or others",
                        path.display()
                    )));
                }
                let canonical = std::fs::canonicalize(path).map_err(|error| {
                    HttpBuildError::InvalidPublicDirectory(format!("{}: {error}", path.display()))
                })?;
                if !canonical.is_dir() {
                    return Err(HttpBuildError::InvalidPublicDirectory(format!(
                        "{} is not a directory",
                        canonical.display()
                    )));
                }
                Some(Arc::new(canonical))
            }
            None => None,
        };
        Ok(Self { root })
    }

    pub(super) async fn index(&self) -> Response {
        self.override_or_embedded("index.html", INDEX_HTML, "text/html; charset=utf-8")
            .await
    }

    pub(super) async fn setup(&self) -> Response {
        self.override_or_embedded("docs/setup.html", SETUP_HTML, "text/html; charset=utf-8")
            .await
    }

    pub(super) async fn file(&self, relative: &str) -> Response {
        let Some(root) = &self.root else {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        };
        let Some(safe) = safe_relative(relative) else {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        };
        let mut candidate = root.as_ref().clone();
        for component in safe.components() {
            let Component::Normal(name) = component else {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            };
            candidate.push(name);
            let metadata = match tokio::fs::symlink_metadata(&candidate).await {
                Ok(metadata)
                    if !metadata.file_type().is_symlink() && trusted_permissions(&metadata) =>
                {
                    metadata
                }
                _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
            };
            if candidate != root.as_ref().as_path() && metadata.is_dir() {
                continue;
            }
        }
        let canonical = match tokio::fs::canonicalize(&candidate).await {
            Ok(path) if path.starts_with(root.as_ref()) => path,
            _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
        };
        match tokio::fs::metadata(&canonical).await {
            Ok(metadata) if metadata.is_file() && trusted_permissions(&metadata) => {}
            _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
        }
        match tokio::fs::read(&canonical).await {
            Ok(bytes) => asset_response(bytes, content_type(&canonical)),
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    async fn override_or_embedded(
        &self,
        relative: &str,
        embedded: &'static str,
        mime: &'static str,
    ) -> Response {
        if self.root.is_some() {
            let response = self.file(relative).await;
            if response.status() != StatusCode::NOT_FOUND {
                return response;
            }
        }
        asset_response(embedded.as_bytes().to_vec(), mime)
    }
}

fn root_components_have_no_symlinks(path: &Path) -> bool {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                cursor.push(component.as_os_str());
            }
            Component::CurDir => continue,
            Component::ParentDir => return false,
        }
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {}
            _ => return false,
        }
    }
    true
}

#[cfg(unix)]
fn trusted_permissions(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn trusted_permissions(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn safe_relative(relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || relative.contains('\\')
        || relative.contains('%')
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_owned())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

fn asset_response(bytes: Vec<u8>, mime: &'static str) -> Response {
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(mime));
    response
}
