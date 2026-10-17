use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mv_proto::agent::MAX_EVENT_LEN;
use mv_proto::api::{
    DeployConfig, ErrorBody, HEADER_DURATION, HEADER_FUNCTION_DURATION, HEADER_FUNCTION_ERROR,
    HEADER_REQUEST_ID, HEADER_RESTORE, HEADER_START, HEADER_VM,
};
use serde::Deserialize;

use crate::builder::BuildErrorKind;
use crate::platform::{InvokeReport, InvokeResult, Platform, PlatformError, Start};

const DEFAULT_LOG_LIMIT: usize = 100;
const MAX_LOG_LIMIT: usize = crate::logs::PER_FUNCTION;

#[derive(Clone)]
struct AppState {
    platform: Arc<Platform>,
    token: Option<Arc<str>>,
    max_code_bytes: usize,
}

pub fn router(platform: Arc<Platform>, token: Option<String>, max_code_bytes: usize) -> Router {
    let state = AppState {
        platform,
        token: token.map(Into::into),
        max_code_bytes,
    };
    let deploy_limit = max_code_bytes + (1 << 20);
    Router::new()
        .route("/metrics", get(metrics))
        .route("/v1/status", get(status))
        .route("/v1/vms", get(vms))
        .route("/v1/functions", get(list))
        .route(
            "/v1/functions/{name}",
            get(describe)
                .put(deploy)
                .delete(delete)
                .layer(DefaultBodyLimit::max(deploy_limit)),
        )
        .route("/v1/functions/{name}/logs", get(logs))
        .route(
            "/v1/functions/{name}/invoke",
            post(invoke).layer(DefaultBodyLimit::max(MAX_EVENT_LEN)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(state)
}

async fn authorize(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(token) = &state.token {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if presented != Some(token) {
            return error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid bearer token".into(),
                Vec::new(),
            );
        }
    }
    next.run(request).await
}

fn error(status: StatusCode, kind: &str, message: String, logs: Vec<String>) -> Response {
    let body = ErrorBody {
        error: message,
        kind: kind.to_string(),
        logs,
    };
    (status, Json(body)).into_response()
}

fn platform_error(err: PlatformError) -> Response {
    match err {
        PlatformError::NotFound(_) => error(
            StatusCode::NOT_FOUND,
            "not_found",
            err.to_string(),
            Vec::new(),
        ),
        PlatformError::BadRequest(message) => {
            error(StatusCode::BAD_REQUEST, "bad_request", message, Vec::new())
        }
        PlatformError::Build(build) => {
            let (status, kind) = match build.kind {
                BuildErrorKind::Init => (StatusCode::UNPROCESSABLE_ENTITY, "init_failed"),
                BuildErrorKind::Load => (StatusCode::UNPROCESSABLE_ENTITY, "load_failed"),
                BuildErrorKind::Platform => (StatusCode::INTERNAL_SERVER_ERROR, "build_failed"),
            };
            error(status, kind, build.message, build.logs)
        }
        PlatformError::Throttled { reason, message } => {
            let mut response = error(StatusCode::TOO_MANY_REQUESTS, reason, message, Vec::new());
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        PlatformError::Timeout(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            err.to_string(),
            Vec::new(),
        ),
        PlatformError::VmFailed { kind, message } => {
            error(StatusCode::BAD_GATEWAY, kind, message, Vec::new())
        }
        PlatformError::Internal(message) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            message,
            Vec::new(),
        ),
    }
}

fn result<T: serde::Serialize>(value: Result<T, PlatformError>) -> Response {
    match value {
        Ok(value) => Json(value).into_response(),
        Err(err) => platform_error(err),
    }
}

async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.platform.render_metrics(),
    )
        .into_response()
}

async fn status(State(state): State<AppState>) -> Response {
    Json(state.platform.status()).into_response()
}

async fn vms(State(state): State<AppState>) -> Response {
    Json(state.platform.vms()).into_response()
}

async fn list(State(state): State<AppState>) -> Response {
    Json(state.platform.list()).into_response()
}

async fn describe(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    result(state.platform.describe(&name))
}

async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.platform.delete(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => platform_error(err),
    }
}

async fn deploy(
    State(state): State<AppState>,
    Path(name): Path<String>,
    mut multipart: Multipart,
) -> Response {
    let mut config = None;
    let mut code = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("invalid upload: {err}"),
                    Vec::new(),
                );
            }
        };
        let field_name = field.name().unwrap_or_default().to_string();
        let bytes = match field.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("invalid upload: {err}"),
                    Vec::new(),
                );
            }
        };
        match field_name.as_str() {
            "config" => match serde_json::from_slice::<DeployConfig>(&bytes) {
                Ok(parsed) => config = Some(parsed),
                Err(err) => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "bad_request",
                        format!("invalid config: {err}"),
                        Vec::new(),
                    );
                }
            },
            "code" => code = Some(bytes),
            other => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("unexpected field {other:?}"),
                    Vec::new(),
                );
            }
        }
    }
    let (Some(config), Some(code)) = (config, code) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "a deploy needs a \"config\" field and a \"code\" field".into(),
            Vec::new(),
        );
    };
    if code.len() > state.max_code_bytes {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "the code archive is too large".into(),
            Vec::new(),
        );
    }
    match state.platform.deploy(&name, config, code.to_vec()).await {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(err) => platform_error(err),
    }
}

#[derive(Deserialize)]
struct LogQuery {
    limit: Option<usize>,
    request_id: Option<String>,
}

async fn logs(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<LogQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(MAX_LOG_LIMIT);
    result(
        state
            .platform
            .logs(&name, limit, query.request_id.as_deref()),
    )
}

fn invoke_headers(report: &InvokeReport) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut set = |name: &'static str, value: String| {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    };
    set(HEADER_REQUEST_ID, report.request_id.clone());
    set(
        HEADER_DURATION,
        format!("{:.3}", report.duration.as_secs_f64() * 1000.0),
    );
    if let Some(start) = report.start {
        set(HEADER_START, start.label().to_string());
        if let Start::Cold(restore) = start {
            set(
                HEADER_RESTORE,
                format!("{:.3}", restore.as_secs_f64() * 1000.0),
            );
        }
    }
    if let Some(vm) = &report.vm_id {
        set(HEADER_VM, vm.clone());
    }
    if let Some(function_ms) = report.function_ms {
        set(HEADER_FUNCTION_DURATION, format!("{function_ms:.3}"));
    }
    headers
}

async fn invoke(State(state): State<AppState>, Path(name): Path<String>, body: Bytes) -> Response {
    let mut report = state.platform.invoke(&name, body.to_vec()).await;
    let headers = invoke_headers(&report);
    let result = std::mem::replace(
        &mut report.result,
        Err(PlatformError::Internal(String::new())),
    );
    let mut response = match result {
        Ok(InvokeResult::Response(body)) => {
            ([(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Ok(InvokeResult::FunctionError(error)) => {
            let mut response = (StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response();
            response.headers_mut().insert(
                HeaderName::from_static(HEADER_FUNCTION_ERROR),
                HeaderValue::from_static("unhandled"),
            );
            response
        }
        Err(err) => platform_error(err),
    };
    response.headers_mut().extend(headers);
    response
}
