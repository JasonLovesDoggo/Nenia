//! A deliberately small single-tenant backup service. Immutable uploads are the API.
use crate::{
    pack,
    vault::{Vault, MAX_MANIFEST_SIZE},
};
use anyhow::{ensure, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::{net::TcpListener, sync::Semaphore};

#[derive(Clone)]
struct App {
    vault: Arc<Mutex<Vault>>,
    token: Arc<String>,
    uploads: Arc<Semaphore>,
}

pub fn router(vault: Vault, token: String) -> Result<Router> {
    ensure!(
        token.len() >= 24,
        "server token must contain at least 24 characters"
    );
    let app = App {
        vault: Arc::new(Mutex::new(vault)),
        token: Arc::new(token),
        uploads: Arc::new(Semaphore::new(4)),
    };
    let protected = Router::new()
        .route("/v1/objects", get(inventory))
        .route("/v1/{kind}/{id}", get(download).put(upload))
        .route("/v1/snapshots", get(snapshots))
        .route("/v1/search", get(search))
        .route("/v1/stats", get(stats))
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(app.clone(), authenticate));
    Ok(Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(protected)
        .with_state(app))
}

pub async fn serve(mut vault: Vault, listen: &str, token: String) -> Result<()> {
    let recovered = vault.reconcile()?;
    let router = router(vault, token)?;
    let listener = TcpListener::bind(listen).await?;
    eprintln!(
        "Nenia listening on {} ({recovered} snapshots recovered)",
        listener.local_addr()?
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn authenticate(State(app): State<App>, req: Request, next: Next) -> Response {
    let supplied = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = app.token.as_bytes();
    let mut diff = supplied.len() ^ expected.len();
    for (a, b) in supplied.as_bytes().iter().zip(expected) {
        diff |= (*a ^ *b) as usize;
    }
    if diff != 0 {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

type ApiResult<T> = std::result::Result<T, (StatusCode, String)>;

async fn blocking<T: Send + 'static>(
    app: App,
    f: impl FnOnce(&mut Vault) -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(move || {
        let mut vault = app
            .vault
            .lock()
            .map_err(|_| anyhow::anyhow!("vault mutex poisoned"))?;
        f(&mut vault)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}\n")))
}

async fn inventory(State(app): State<App>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(app, |v| v.inventory()).await?))
}

async fn download(
    State(app): State<App>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<Response> {
    if !matches!(kind.as_str(), "packs" | "manifests") || pack::validate_hash_id(&id).is_err() {
        return Err((StatusCode::NOT_FOUND, "object not found\n".into()));
    }
    let bytes = blocking(app, move |v| v.object(&kind, &id))
        .await
        .map_err(|(_, e)| (StatusCode::NOT_FOUND, e))?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Body::from(bytes),
    )
        .into_response())
}

async fn upload(
    State(app): State<App>,
    Path((kind, id)): Path<(String, String)>,
    req: Request,
) -> ApiResult<StatusCode> {
    let max = match kind.as_str() {
        "packs" => pack::MAX_PACK_RAW_SIZE + 65536,
        "manifests" => MAX_MANIFEST_SIZE,
        _ => return Err((StatusCode::NOT_FOUND, "unknown object kind\n".into())),
    };
    pack::validate_hash_id(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // Bound simultaneous body allocation before buffering any request.
    let permit = app.uploads.clone().try_acquire_owned().map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "upload capacity reached; retry\n".into(),
        )
    })?;
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        to_bytes(req.into_body(), max),
    )
    .await
    .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "upload timed out\n".into()))?
    .map_err(|_| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "object exceeds size limit\n".into(),
        )
    })?;
    let created = blocking(app, move |v| v.receive_object(&kind, &id, &bytes)).await?;
    drop(permit);
    Ok(if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    })
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    all_versions: Option<bool>,
    q: Option<String>,
}

async fn snapshots(
    State(app): State<App>,
    Query(q): Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(app, move |v| {
            v.list(
                None,
                q.limit.unwrap_or(100).min(10000),
                q.all_versions.unwrap_or(false),
            )
        })
        .await?,
    ))
}

async fn search(
    State(app): State<App>,
    Query(q): Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(app, move |v| {
            v.search(
                q.q.as_deref().unwrap_or(""),
                q.limit.unwrap_or(100).min(10000),
                q.all_versions.unwrap_or(false),
            )
        })
        .await?,
    ))
}

async fn stats(State(app): State<App>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(app, |v| v.stats()).await?))
}
