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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::admission::{Admission, Limits};
    use crate::builder::{Builder, Images};
    use crate::jail::JailConfig;
    use crate::metrics::Metrics;
    use crate::platform::Settings;
    use crate::store::Store;
    use crate::vm::Launcher;

    const BOUNDARY: &str = "mvd-test-boundary";

    fn platform(dir: &Path) -> Arc<Platform> {
        let launcher = Arc::new(Launcher::new(
            JailConfig {
                enabled: false,
                jailer: dir.join("mv-jailer"),
                vmm: dir.join("minivmm"),
                base_dir: dir.join("jails"),
            },
            None,
            32,
        ));
        let images = Images {
            kernel: dir.join("missing-bzImage"),
            runtime_dir: dir.join("runtimes"),
        };
        let builder = Builder::new(
            launcher.clone(),
            images,
            dir,
            900_000,
            Duration::from_secs(5),
        );
        let admission = Admission::new(
            Limits {
                memory_total_mib: 4096,
                memory_limit_mib: 2048,
                memory_reserve_mib: 256,
                cpus: 2,
                vcpu_limit: 4,
            },
            Box::new(|| Some(3000)),
        );
        let settings = Settings {
            uid_base: 900_000,
            idle_timeout: Duration::from_secs(60),
            queue_timeout: Duration::from_millis(10),
            vm_overhead_mib: 32,
        };
        Platform::new(
            settings,
            Store::open(dir).unwrap(),
            launcher,
            builder,
            admission,
            Arc::new(Metrics::new()),
        )
    }

    fn app(dir: &Path, token: Option<&str>) -> Router {
        router(platform(dir), token.map(str::to_string), 1 << 20)
    }

    async fn send(
        app: &Router,
        request: axum::http::Request<Body>,
    ) -> (StatusCode, HeaderMap, Value) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, headers, body)
    }

    fn get(path: &str) -> axum::http::Request<Body> {
        axum::http::Request::get(path).body(Body::empty()).unwrap()
    }

    fn multipart(fields: &[(&str, &[u8])]) -> Body {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        Body::from(body)
    }

    fn deploy(name: &str, fields: &[(&str, &[u8])]) -> axum::http::Request<Body> {
        axum::http::Request::put(format!("/v1/functions/{name}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(multipart(fields))
            .unwrap()
    }

    #[tokio::test]
    async fn read_only_endpoints_describe_an_empty_platform() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path(), None);
        let (status, _, body) = send(&app, get("/healthz")).await;
        assert_eq!(
            (status, body),
            (StatusCode::OK, Value::String("ok\n".into()))
        );
        let (status, _, body) = send(&app, get("/v1/functions")).await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!([])));
        let (status, _, body) = send(&app, get("/v1/status")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["capacity"]["memory_limit_mib"], 2048);
        assert_eq!(body["functions"], 0);
        let (_, headers, body) = send(&app, get("/metrics")).await;
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        assert!(body.as_str().unwrap().contains("mvd_host_vcpu_limit 4"));
        let (status, _, body) = send(&app, get("/v1/vms")).await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!([])));
    }

    #[tokio::test]
    async fn unknown_functions_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path(), None);
        let (status, _, body) = send(&app, get("/v1/functions/ghost")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["kind"], "not_found");
        let (status, _, _) = send(&app, get("/v1/functions/ghost/logs")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let delete = axum::http::Request::delete("/v1/functions/ghost")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, delete).await.0, StatusCode::NOT_FOUND);
        let invoke = axum::http::Request::post("/v1/functions/ghost/invoke")
            .body(Body::from("{}"))
            .unwrap();
        let (status, headers, _) = send(&app, invoke).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(headers.contains_key(HEADER_REQUEST_ID));
        assert!(headers.contains_key(HEADER_DURATION));
        assert!(!headers.contains_key(HEADER_START));
    }

    #[tokio::test]
    async fn deploys_are_validated_before_building() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path(), None);
        let config = br#"{"runtime":"native"}"#.as_slice();
        type Fields<'a> = Vec<(&'a str, &'a [u8])>;
        let cases: [(&str, Fields, &str); 5] = [
            (
                "Bad_Name",
                vec![("config", config), ("code", b"x")],
                "function names",
            ),
            ("ok", vec![("config", config)], "\"code\" field"),
            (
                "ok",
                vec![("config", b"{"), ("code", b"x")],
                "invalid config",
            ),
            (
                "ok",
                vec![("config", br#"{"runtime":"java"}"#), ("code", b"x")],
                "invalid config",
            ),
            (
                "ok",
                vec![
                    ("config", br#"{"runtime":"native","vcpus":99}"#),
                    ("code", b"x"),
                ],
                "vcpus",
            ),
        ];
        for (name, fields, expected) in cases {
            let (status, _, body) = send(&app, deploy(name, &fields)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body["error"].as_str().unwrap().contains(expected), "{body}");
        }
        let oversized = vec![0u8; (1 << 20) + 1];
        let (status, _, _) = send(
            &app,
            deploy("big", &[("config", config), ("code", &oversized)]),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (status, _, body) = send(
            &app,
            deploy("real", &[("config", config), ("code", b"archive")]),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["kind"], "build_failed");
        assert_eq!(
            send(&app, get("/v1/functions/real")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn bearer_tokens_guard_everything_but_health() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path(), Some("s3cret"));
        assert_eq!(send(&app, get("/healthz")).await.0, StatusCode::OK);
        let (status, _, body) = send(&app, get("/v1/functions")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["kind"], "unauthorized");
        let wrong = axum::http::Request::get("/v1/functions")
            .header(header::AUTHORIZATION, "Bearer nope")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED);
        let right = axum::http::Request::get("/v1/functions")
            .header(header::AUTHORIZATION, "Bearer s3cret")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, right).await.0, StatusCode::OK);
    }

    #[test]
    fn throttling_sets_retry_after_and_errors_map_to_statuses() {
        let response = platform_error(PlatformError::Throttled {
            reason: "memory",
            message: "full".into(),
        });
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        let cases = [
            (
                PlatformError::Timeout(Duration::from_secs(1)),
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                PlatformError::VmFailed {
                    kind: "vm_crashed",
                    message: String::new(),
                },
                StatusCode::BAD_GATEWAY,
            ),
            (
                PlatformError::BadRequest(String::new()),
                StatusCode::BAD_REQUEST,
            ),
            (
                PlatformError::Internal(String::new()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(platform_error(error).status(), status);
        }
    }
}
