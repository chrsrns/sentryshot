// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(clippy::unwrap_used)]

use crate::{Router, check_plugin_version, find_plugin_path, get_version2, pre_load_plugin};
use async_trait::async_trait;
use axum::{body::Body, routing::get};
use common::{
    AccountId, AccountSetRequest, AccountsMap, AuthAccountDeleteError, AuthAccountSetError,
    AuthenticatedUser, Authenticator, EnvPlugin, ValidateResponse,
};
use http::{HeaderMap, HeaderValue, Request, StatusCode};
use libloading::Library;
use pretty_assertions::assert_eq;
use std::{path::PathBuf, sync::Arc};
use tempfile::{TempDir, tempdir};
use tower::Service;

struct StubAuth(Option<AuthenticatedUser>);

#[async_trait]
impl Authenticator for StubAuth {
    async fn validate_request(&self, _req: &HeaderMap<HeaderValue>) -> ValidateResponse {
        self.0.clone()
    }
    async fn accounts(&self) -> AccountsMap {
        unimplemented!()
    }
    async fn account_set(&self, _req: AccountSetRequest) -> Result<bool, AuthAccountSetError> {
        unimplemented!()
    }
    async fn account_delete(&self, _id: &AccountId) -> Result<(), AuthAccountDeleteError> {
        unimplemented!()
    }
}

fn admin_user() -> Option<AuthenticatedUser> {
    Some(AuthenticatedUser {
        is_admin: true,
        stored_token: "x".to_owned(),
        token_valid: true,
    })
}

fn admin_bad_token() -> Option<AuthenticatedUser> {
    Some(AuthenticatedUser {
        is_admin: true,
        stored_token: "x".to_owned(),
        token_valid: false,
    })
}

fn regular_user() -> Option<AuthenticatedUser> {
    Some(AuthenticatedUser {
        is_admin: false,
        stored_token: "x".to_owned(),
        token_valid: true,
    })
}

async fn status(router: axum::Router) -> (StatusCode, axum::body::Bytes) {
    let mut router = router;
    std::future::poll_fn(|cx| {
        <axum::Router as Service<Request<Body>>>::poll_ready(&mut router, cx)
    })
    .await
    .unwrap();
    let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
    let resp = <axum::Router as Service<Request<Body>>>::call(&mut router, req)
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body)
}

async fn ok_handler() -> &'static str {
    "ok"
}

fn new_router(user: Option<AuthenticatedUser>) -> Router {
    Router::new(axum::Router::new(), Arc::new(StubAuth(user)))
}

// Chain order: unauthenticated -> non-admin -> bad CSRF token -> pass.
// All three blocks are 401; the CSRF body differs.
#[tokio::test]
async fn test_route_admin_chain() {
    let router = |user: Option<AuthenticatedUser>| {
        new_router(user).route_admin("/x", get(ok_handler)).inner()
    };

    // 1. Unauthenticated is rejected first.
    let (s, _b) = status(router(None)).await;
    assert_eq!(StatusCode::UNAUTHORIZED, s);

    // 2. Authenticated but not admin.
    let (s, _b) = status(router(regular_user())).await;
    assert_eq!(StatusCode::UNAUTHORIZED, s);

    // 3. Admin with invalid CSRF token.
    let (s, b) = status(router(admin_bad_token())).await;
    assert_eq!(StatusCode::UNAUTHORIZED, s);
    assert_eq!("Invalid CSRF-token", b);

    // 4. Admin with valid token passes.
    let (s, b) = status(router(admin_user())).await;
    assert_eq!(StatusCode::OK, s);
    assert_eq!("ok", b);
}

// The CSRF middleware applies to GET requests too.
#[tokio::test]
async fn test_route_admin_csrf_on_get() {
    let router = new_router(admin_bad_token())
        .route_admin("/x", get(ok_handler))
        .inner();
    let (s, _) = status(router).await;
    assert_eq!(StatusCode::UNAUTHORIZED, s);
}

#[tokio::test]
async fn test_route_admin_no_csrf() {
    let router = new_router(admin_bad_token())
        .route_admin_no_csrf("/x", get(ok_handler))
        .inner();
    let (s, _) = status(router).await;
    assert_eq!(StatusCode::OK, s);
}

#[tokio::test]
async fn test_route_user_no_csrf() {
    // Non-admin with a bad token still passes: only authn is checked.
    let bad_token_user = Some(AuthenticatedUser {
        is_admin: false,
        stored_token: "x".to_owned(),
        token_valid: false,
    });
    let router = new_router(bad_token_user)
        .route_user_no_csrf("/x", get(ok_handler))
        .inner();
    let (s, _) = status(router).await;
    assert_eq!(StatusCode::OK, s);
}

#[tokio::test]
async fn test_route_no_auth() {
    let router = new_router(None).route_no_auth("/x", get(ok_handler)).inner();
    let (s, _) = status(router).await;
    assert_eq!(StatusCode::OK, s);
}

// --- find_plugin_path ---

#[test]
fn test_find_plugin_path_long() {
    let dir = tempdir().unwrap();
    let long = dir.path().join("libsentryshot_test.so");
    std::fs::write(&long, b"").unwrap();
    assert_eq!(long, find_plugin_path(dir.path(), "test").unwrap());
}

#[test]
fn test_find_plugin_path_short_fallback() {
    let dir = tempdir().unwrap();
    let short = dir.path().join("libtest.so");
    std::fs::write(&short, b"").unwrap();
    assert_eq!(short, find_plugin_path(dir.path(), "test").unwrap());
}

#[test]
fn test_find_plugin_path_prefers_long() {
    let dir = tempdir().unwrap();
    let long = dir.path().join("libsentryshot_test.so");
    let short = dir.path().join("libtest.so");
    std::fs::write(&long, b"").unwrap();
    std::fs::write(&short, b"").unwrap();
    assert_eq!(long, find_plugin_path(dir.path(), "test").unwrap());
}

#[test]
fn test_find_plugin_path_not_found() {
    let dir = tempdir().unwrap();
    assert!(find_plugin_path(dir.path(), "test").is_err());
}

// --- fixture cdylib ---

// Compiles a tiny cdylib fixture with rustc and returns its path.
fn build_fixture(dir: &TempDir, name: &str, src: &str) -> PathBuf {
    let src_path = dir.path().join(format!("{name}.rs"));
    std::fs::write(&src_path, src).unwrap();
    let out_path = dir.path().join(format!("lib{name}.so"));
    let output = std::process::Command::new("rustc")
        .args(["--edition", "2021", "--crate-type", "cdylib", "-o"])
        .arg(&out_path)
        .arg(&src_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "rustc: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    out_path
}

const VERSION_SRC: &str = r#"
#[no_mangle]
pub extern "C" fn version() -> *const std::ffi::c_char {
    c"vx_test".as_ptr()
}
"#;

const VERSION_AND_LOAD_SRC: &str = r#"
#[no_mangle]
pub extern "C" fn version() -> *const std::ffi::c_char {
    c"vx_test".as_ptr()
}
#[no_mangle]
pub extern "C" fn load() {}
"#;

#[test]
fn test_check_plugin_version_ok() {
    let dir = tempdir().unwrap();
    let path = build_fixture(&dir, "plug_a", VERSION_SRC);
    let lib = unsafe { Library::new(path).unwrap() };
    check_plugin_version(&lib, "vx_test").unwrap();
}

#[test]
fn test_check_plugin_version_mismatch() {
    let dir = tempdir().unwrap();
    let path = build_fixture(&dir, "plug_b", VERSION_SRC);
    let lib = unsafe { Library::new(path).unwrap() };
    let e = check_plugin_version(&lib, "vx_other").unwrap_err();
    assert!(matches!(
        e,
        crate::CheckPluginVersionError::VersionMismatch(..)
    ));
}

// A library without a `version` symbol fails closed.
#[test]
fn test_check_plugin_version_missing_symbol() {
    let dir = tempdir().unwrap();
    let path = build_fixture(&dir, "plug_c", "pub extern \"C\" fn dummy() {}");
    let lib = unsafe { Library::new(path).unwrap() };
    let e = check_plugin_version(&lib, "vx_test").unwrap_err();
    assert!(matches!(
        e,
        crate::CheckPluginVersionError::LoadVersionSymbol(..)
    ));
}

fn env_plugin(name: &str) -> EnvPlugin {
    serde_json::from_str(&format!(r#"{{"name":"{name}","enable":true}}"#)).unwrap()
}

// Version checks out but the `load` symbol is missing -> error.
#[test]
fn test_pre_load_plugin_missing_load() {
    let dir = tempdir().unwrap();
    build_fixture(&dir, "test_plug", VERSION_SRC);
    let mut pre = crate::PreLoadedPlugins::default();
    let e = pre_load_plugin(dir.path(), &mut pre, &env_plugin("test_plug"), "vx_test").unwrap_err();
    assert!(matches!(e, crate::PreLoadPluginError::LoadSymbol(..)));
}

// Both symbols present -> load fn is registered.
#[test]
fn test_pre_load_plugin_ok() {
    let dir = tempdir().unwrap();
    build_fixture(&dir, "test_plug", VERSION_AND_LOAD_SRC);
    let mut pre = crate::PreLoadedPlugins::default();
    pre_load_plugin(dir.path(), &mut pre, &env_plugin("test_plug"), "vx_test").unwrap();
    assert_eq!(1, pre.load_fns_count());
}

#[cfg(debug_assertions)]
#[test]
fn test_get_version2_debug_suffix() {
    assert!(get_version2().ends_with("_debug"), "{}", get_version2());
}
