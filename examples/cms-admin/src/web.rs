use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::{Environment, context};
use serde::Deserialize;
use tower_http::services::ServeDir;

use crate::atlas::{Atlas, ListQuery};
use crate::auth::Auth;

#[derive(Clone)]
pub struct AppState {
    pub atlas: Atlas,
    pub auth: Auth,
    pub media_url: String,
    templates: Arc<Environment<'static>>,
}

impl AppState {
    pub fn new(
        atlas: Atlas,
        auth: Auth,
        media_url: String,
        templates_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let mut templates = Environment::new();
        let root = templates_dir.clone();
        templates.set_loader(move |name| Ok(std::fs::read_to_string(root.join(name)).ok()));
        templates.set_auto_escape_callback(|name| {
            if name.ends_with(".html") {
                minijinja::AutoEscape::Html
            } else {
                minijinja::AutoEscape::None
            }
        });
        templates.add_filter("human_bytes", human_bytes);
        templates.add_filter("time", format_time);
        templates.add_filter("mime_suffix", mime_suffix);
        Ok(Self {
            atlas,
            auth,
            media_url,
            templates: Arc::new(templates),
        })
    }
}

pub fn router(state: AppState, static_dir: PathBuf) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/library", get(library))
        .route("/assets/{kind}/{id}", get(asset_detail))
        .route("/trash", get(trash))
        .route("/upload", get(upload_page).post(upload))
        .route("/bulk", post(bulk))
        .route("/assets/{kind}/{id}/pin", post(pin))
        .route("/assets/{kind}/{id}/delete", post(delete_asset))
        .route("/trash/{kind}/{id}/restore", post(restore))
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(DefaultBodyLimit::max(60 * 1024 * 1024))
        .with_state(state)
}

async fn root() -> Redirect {
    Redirect::temporary("/library")
}

#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

async fn login_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    if state.auth.is_authenticated(&headers) {
        return Redirect::to("/library").into_response();
    }
    render(
        &state,
        "login.html",
        context! { next => safe_next(query.next.as_deref()), error => None::<String> },
    )
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    next: Option<String>,
}

async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    match state.auth.login(&form.password) {
        Some(session) => {
            let mut response = Redirect::to(safe_next(form.next.as_deref())).into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, session.cookie);
            response
        }
        None => render(
            &state,
            "login.html",
            context! { next => safe_next(form.next.as_deref()), error => Some("That password did not open the dashboard.".to_owned()) },
        ),
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state.auth.logout(&headers);
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, state.auth.clear_cookie());
    response
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
struct LibraryParams {
    kind: Option<String>,
    state: Option<String>,
    q: Option<String>,
    mime: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    page: Option<i64>,
    flash: Option<String>,
    flash_error: Option<bool>,
}

impl Default for LibraryParams {
    fn default() -> Self {
        Self {
            kind: Some("images".to_owned()),
            state: Some("active".to_owned()),
            q: None,
            mime: None,
            sort: Some("created_at".to_owned()),
            order: Some("desc".to_owned()),
            page: Some(1),
            flash: None,
            flash_error: None,
        }
    }
}

async fn library(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<LibraryParams>,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    let kind = query
        .kind
        .as_deref()
        .filter(|k| is_kind(k))
        .unwrap_or("images");
    let page = query.page.unwrap_or(1).max(1);
    let limit = 50_i64;
    let offset = (page - 1) * limit;
    let list_query = ListQuery {
        state: query.state.as_deref().filter(|s| !s.is_empty()),
        q: query.q.as_deref().filter(|s| !s.trim().is_empty()),
        mime: query.mime.as_deref().filter(|s| !s.is_empty()),
        sort: query.sort.as_deref().filter(|s| !s.is_empty()),
        order: query.order.as_deref().filter(|s| !s.is_empty()),
        limit: Some(limit),
        offset: Some(offset),
    };
    let list = match state.atlas.list(kind, &list_query).await {
        Ok(list) => list,
        Err(error) => {
            return render(
                &state,
                "library.html",
                context! { error => error.to_string(), assets => Vec::<()>::new(), kind, page, limit, total => 0_i64, query => query.clone(), summary => None::<()>, formats => None::<()>, media_url => state.media_url, active => "library", flash => query.flash, flash_error => query.flash_error.unwrap_or(false) },
            );
        }
    };
    let summary = state.atlas.summary().await.ok();
    let formats = state.atlas.formats().await.ok();
    let template = if headers
        .get("HX-Request")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| value == "true")
    {
        "partials/library-results.html"
    } else {
        "library.html"
    };
    render(
        &state,
        template,
        context! {
            assets => list.assets,
            kind,
            page,
            limit => list.limit,
            total => list.total,
            query => query.clone(),
            summary,
            formats,
            media_url => state.media_url,
            error => None::<String>,
            active => "library",
            flash => query.flash,
            flash_error => query.flash_error.unwrap_or(false),
        },
    )
}

async fn asset_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    if !is_kind(&kind) {
        return not_found();
    }
    let flash = uri.query().and_then(|query| query_value(query, "flash"));
    let flash_error = uri
        .query()
        .and_then(|query| query_value(query, "flash_error"))
        .is_some_and(|value| value == "true");
    match state.atlas.asset(&kind, &id).await {
        Ok(asset) => {
            let srcset = if kind == "images" && asset.state == "active" {
                state.atlas.srcset(&id).await.ok().map(|mut srcset| {
                    srcset.src = public_media_url(&state.media_url, &srcset.src);
                    srcset.srcset = srcset
                        .srcset
                        .replace("/media/", &format!("{}/media/", state.media_url));
                    srcset
                })
            } else {
                None
            };
            render(
                &state,
                "asset.html",
                context! { asset, kind, srcset, media_url => state.media_url, error => None::<String>, active => "library", flash, flash_error },
            )
        }
        Err(error) => render(
            &state,
            "asset.html",
            context! { asset => None::<()>, kind, srcset => None::<()>, media_url => state.media_url, error => Some(error.to_string()), active => "library", flash, flash_error },
        ),
    }
}

async fn trash(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    let summary = state.atlas.summary().await.ok();
    let flash = uri.query().and_then(|query| query_value(query, "flash"));
    let flash_error = uri
        .query()
        .and_then(|query| query_value(query, "flash_error"))
        .is_some_and(|v| v == "true");
    match state.atlas.trash().await {
        Ok(trash) => render(
            &state,
            "trash.html",
            context! { trash, summary, error => None::<String>, active => "trash", flash, flash_error },
        ),
        Err(error) => render(
            &state,
            "trash.html",
            context! { trash => None::<()>, summary, error => Some(error.to_string()), active => "trash", flash, flash_error },
        ),
    }
}

async fn upload_page(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    let formats = state.atlas.formats().await.ok();
    render(
        &state,
        "upload.html",
        context! { formats, summary => state.atlas.summary().await.ok(), results => Vec::<String>::new(), error => None::<String>, active => "upload" },
    )
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    mut multipart: Multipart,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    let mut results: Vec<(String, bool, String)> = Vec::new();
    let mut selected_kind: Option<String> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                results.push(("upload".to_owned(), false, error.to_string()));
                break;
            }
        };
        if field.name() == Some("kind") {
            selected_kind = field.text().await.ok();
            continue;
        }
        if field.name() != Some("files") {
            continue;
        }
        let filename = field.file_name().unwrap_or("upload").to_owned();
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let default_kind = if content_type.starts_with("image/") {
            "images"
        } else {
            "documents"
        };
        let kind = selected_kind.as_deref().unwrap_or(default_kind);
        if !is_kind(kind) {
            results.push((
                filename,
                false,
                "Choose images or documents for this upload.".to_owned(),
            ));
            continue;
        }
        match field.bytes().await {
            Ok(bytes) => match state
                .atlas
                .upload(kind, &filename, &content_type, bytes.to_vec())
                .await
            {
                Ok((status, asset)) => {
                    results.push((filename, true, format!("{} · {}", asset.id, status)))
                }
                Err(error) => results.push((filename, false, error.to_string())),
            },
            Err(error) => results.push((filename, false, error.to_string())),
        }
    }
    let hx = headers.get("HX-Request").and_then(|h| h.to_str().ok()) == Some("true");
    if hx {
        return render(&state, "partials/upload-results.html", context! { results });
    }
    render(
        &state,
        "upload.html",
        context! { formats => state.atlas.formats().await.ok(), summary => state.atlas.summary().await.ok(), results, error => None::<String>, active => "upload" },
    )
}

#[derive(Deserialize)]
struct BulkForm {
    kind: String,
    action: String,
    ids: String,
}

async fn bulk(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<BulkForm>,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    if !is_kind(&form.kind)
        || !matches!(form.action.as_str(), "pin" | "unpin" | "delete" | "restore")
    {
        return bad_request("Unknown collection or action.");
    }
    let ids = form
        .ids
        .split(',')
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    match state.atlas.act(&form.kind, &form.action, &ids).await {
        Ok(result) => {
            let failed = result
                .results
                .iter()
                .filter(|row| row.status == "error")
                .count();
            let message = if failed == 0 {
                format!("{} assets: {}.", result.results.len(), form.action)
            } else {
                format!(
                    "Atlas rejected the batch. {failed} of {} actions failed.",
                    result.results.len()
                )
            };
            redirect_with_flash("/library", &message, failed > 0)
        }
        Err(error) => redirect_with_flash("/library", &error.to_string(), true),
    }
}

async fn pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    let pinned = match state.atlas.asset(&kind, &id).await {
        Ok(asset) => !asset.pinned,
        Err(error) => {
            return redirect_with_flash(&format!("/assets/{kind}/{id}"), &error.to_string(), true);
        }
    };
    match state.atlas.set_pinned(&kind, &id, pinned).await {
        Ok(_) => redirect_with_flash(
            &format!("/assets/{kind}/{id}"),
            if pinned {
                "Pinned; Atlas will not purge it."
            } else {
                "Unpinned."
            },
            false,
        ),
        Err(error) => {
            redirect_with_flash(&format!("/assets/{kind}/{id}"), &error.to_string(), true)
        }
    }
}

async fn delete_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    match state.atlas.delete(&kind, &id).await {
        Ok(()) => redirect_with_flash("/library", "Moved to Atlas trash.", false),
        Err(error) => {
            redirect_with_flash(&format!("/assets/{kind}/{id}"), &error.to_string(), true)
        }
    }
}

async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
) -> Response {
    if let Some(response) = signed_in(&state, &headers, &uri) {
        return response;
    }
    match state.atlas.restore(&kind, &id).await {
        Ok(()) => redirect_with_flash("/trash", "Restored at the same public URL.", false),
        Err(error) => redirect_with_flash("/trash", &error.to_string(), true),
    }
}

fn signed_in(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if state.auth.is_authenticated(headers) {
        return None;
    }
    let target = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/library");
    let next = percent_encode(target);
    let mut response = Redirect::to(&format!("/login?next={next}")).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, state.auth.clear_cookie());
    Some(response)
}

fn render(state: &AppState, name: &str, value: minijinja::Value) -> Response {
    match state
        .templates
        .get_template(name)
        .and_then(|template| template.render(value))
    {
        Ok(body) => Html(body).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Template error: {error}"),
        )
            .into_response(),
    }
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

fn redirect_with_flash(location: &str, message: &str, error: bool) -> Response {
    let separator = if location.contains('?') { '&' } else { '?' };
    let target = format!(
        "{location}{separator}flash={}&flash_error={error}",
        percent_encode(message)
    );
    Redirect::to(&target).into_response()
}

fn safe_next(value: Option<&str>) -> &str {
    value
        .filter(|value| {
            value.starts_with('/')
                && !value.starts_with("//")
                && !value.contains('\\')
                && !value.chars().any(char::is_control)
        })
        .unwrap_or("/library")
}

fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~/".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (candidate, value) = pair.split_once('=')?;
        (candidate == key).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    output.push(((high << 4) | low) as u8);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn is_kind(value: &str) -> bool {
    matches!(value, "images" | "documents")
}

fn public_media_url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    for unit in UNITS {
        if value < 1024.0 || unit == "GiB" {
            return if unit == "B" {
                format!("{value:.0} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    unreachable!()
}

fn format_time(value: minijinja::Value) -> String {
    let Some(seconds) = value.as_i64() else {
        return "—".to_owned();
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "unknown date".to_owned())
}

fn mime_suffix(value: String) -> String {
    value.rsplit('/').next().unwrap_or(&value).to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::atlas::Atlas;
    use crate::auth::Auth;

    use super::{AppState, human_bytes, router, safe_next};

    async fn test_app() -> axum::Router {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let atlas = Atlas::new("http://127.0.0.1:1", "test-token").unwrap();
        let auth = Auth::new("test-password".to_owned(), false);
        let state = AppState::new(
            atlas,
            auth,
            "http://127.0.0.1:1".to_owned(),
            root.join("templates"),
        )
        .unwrap();
        router(state, root.join("static"))
    }

    #[test]
    fn bytes_have_human_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
    }

    #[test]
    fn login_next_never_becomes_an_external_redirect() {
        assert_eq!(safe_next(Some("/library?q=hi")), "/library?q=hi");
        assert_eq!(safe_next(Some("//evil.example/")), "/library");
        assert_eq!(safe_next(Some("https://evil.example/")), "/library");
        assert_eq!(safe_next(Some("/\\\\evil.example/")), "/library");
        assert_eq!(
            safe_next(Some("/library\r\nSet-Cookie: evil=1")),
            "/library"
        );
    }

    #[tokio::test]
    async fn private_pages_redirect_to_login_and_login_opens_them() {
        let app = test_app().await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/library?q=holiday")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/login?next=/library%3Fq%3Dholiday"
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=test-password&next=%2Flibrary"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/library");
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        let pair = cookie.split(';').next().unwrap().to_owned();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/library")
                    .header(header::COOKIE, pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Library"), "the protected page rendered");
        assert!(html.contains("Atlas"), "Atlas errors stay visible: {html}");
    }

    #[tokio::test]
    async fn wrong_password_stays_on_login_and_explains_itself() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("did not open")
        );
    }

    #[tokio::test]
    async fn static_vendored_assets_are_served_without_login() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/static/vendor/htmx.min.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
