use std::io::Write;
use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::perm::{Duty, Rule};
use crate::store::{ApiError, AuthSession, PullRequest, PushRequest, Store};

pub async fn serve(data: PathBuf, bind: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "ait 服务已启动");
    let _ = writeln!(err, "  管理页面  http://{addr}/");
    let _ = writeln!(err, "  数据目录  {}", data.display());
    drop(err);
    serve_listener(data, listener).await
}

pub async fn serve_listener(
    data: PathBuf,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let store = Store::open(data)?;
    let app = router(store);
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn router(store: Store) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/app.css", get(asset_css))
        .route("/app.js", get(asset_js))
        .route("/api/health", get(health))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/repos", get(list_repos).post(create_repo))
        .route("/api/repos/:name", get(repo_detail))
        .route("/api/repos/:name/tree", get(tree))
        .route("/api/repos/:name/blob", get(blob))
        .route("/api/repos/:name/commits", get(commits))
        .route("/api/repos/:name/diff", get(diff))
        .route("/api/repos/:name/branches", get(branches))
        .route("/api/repos/:name/policy", get(get_policy).put(put_policy))
        .route("/api/repos/:name/manifest", get(manifest))
        .route("/api/repos/:name/plan", get(get_plan).put(put_plan))
        .route("/api/repos/:name/reviews", get(reviews))
        .route("/api/repos/:name/reviews/:id/approve", post(approve))
        .route("/api/repos/:name/reviews/:id/reject", post(reject))
        .route("/api/repos/:name/pulls", get(pulls).post(create_pull))
        .route("/api/repos/:name/pulls/:id/approve", post(approve_pull))
        .route("/api/repos/:name/pulls/:id/merge", post(merge_pull))
        .route("/api/repos/:name/pulls/:id/close", post(close_pull))
        .route("/api/git/:name/refs", get(git_refs))
        .route("/api/git/:name/pull", post(git_pull))
        .route("/api/git/:name/push", post(git_push))
        .route("/r/:name", get(repo_redirect))
        .merge(crate::smart::routes())
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(store)
}

async fn index_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../assets/index.html"),
    )
}

async fn asset_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/app.css"),
    )
}

async fn asset_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/app.js"),
    )
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn repo_redirect(Path(name): Path<String>) -> Redirect {
    Redirect::temporary(&format!("/#/repo/{name}"))
}

#[derive(Deserialize)]
struct AuthBody {
    name: String,
    password: String,
}

async fn register(
    State(store): State<Store>,
    Json(body): Json<AuthBody>,
) -> Result<Json<AuthSession>, ApiError> {
    Ok(Json(store.register(&body.name, &body.password)?))
}

async fn login(
    State(store): State<Store>,
    Json(body): Json<AuthBody>,
) -> Result<Json<AuthSession>, ApiError> {
    Ok(Json(store.login(&body.name, &body.password)?))
}

async fn logout(State(store): State<Store>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    store.logout(token(&headers)?)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn me(State(store): State<Store>, headers: HeaderMap) -> Result<Json<AuthSession>, ApiError> {
    Ok(Json(auth(&store, &headers)?))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    description: String,
}

async fn list_repos(
    State(store): State<Store>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::store::RepoSummary>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.list_repos(&base_url(&headers))?))
}

async fn create_repo(
    State(store): State<Store>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<Json<crate::store::RepoMeta>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.create_repo(
        &session.user,
        session.admin,
        &body.name,
        &body.description,
    )?))
}

async fn repo_detail(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<crate::store::RepoSummary>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.repo_summary(&name, &base_url(&headers))?))
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    reference: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    limit: Option<usize>,
}

async fn tree(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<Vec<crate::store::TreeItem>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.tree(&name, &query.reference, &query.path)?))
}

async fn blob(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<crate::store::BlobView>, ApiError> {
    let _session = auth(&store, &headers)?;
    if query.path.is_empty() {
        return Err(ApiError::bad("缺少 path"));
    }
    Ok(Json(store.blob(&name, &query.reference, &query.path)?))
}

async fn commits(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<Vec<crate::store::CommitInfo>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.commits(
        &name,
        &query.reference,
        query.limit.unwrap_or(50),
    )?))
}

#[derive(Deserialize)]
struct DiffQuery {
    #[serde(default)]
    from: String,
    to: String,
}

async fn diff(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _session = auth(&store, &headers)?;
    let to = crate::git::Oid::parse(&query.to).map_err(|err| ApiError::bad(err.to_string()))?;
    let from = if query.from.is_empty() {
        None
    } else {
        Some(crate::git::Oid::parse(&query.from).map_err(|err| ApiError::bad(err.to_string()))?)
    };
    Ok(Json(
        serde_json::json!({ "diff": store.diff(&name, from, to)? }),
    ))
}

async fn branches(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Vec<crate::store::RefInfo>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.branches(&name)?))
}

async fn get_policy(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<crate::perm::Policy>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.policy(&name)?))
}

#[derive(Deserialize)]
struct PolicyBody {
    rules: Vec<Rule>,
    duties: Vec<Duty>,
}

async fn put_policy(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<PolicyBody>,
) -> Result<Json<crate::perm::Policy>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.put_policy(
        &session.user,
        session.admin,
        &name,
        body.rules,
        body.duties,
    )?))
}

async fn manifest(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<crate::perm::Manifest>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.manifest_for(
        &name,
        &session.user,
        session.admin,
    )?))
}

async fn get_plan(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(
        serde_json::json!({ "content": store.plan_text(&name)? }),
    ))
}

#[derive(Deserialize)]
struct PlanBody {
    content: String,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    use_llm: bool,
}

async fn put_plan(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<PlanBody>,
) -> Result<Json<crate::store::PlanView>, ApiError> {
    let session = auth(&store, &headers)?;
    let policy = store.policy(&name)?;
    if !policy.is_owner(&session.user, session.admin) {
        return Err(ApiError::forbidden("只有仓库所有者可以修改规划"));
    }
    if body.use_llm {
        let users = store.user_names()?;
        let content = body.content.clone();
        let items = tokio::task::spawn_blocking(move || {
            crate::plan::assignments_from_llm(&content, &users)
        })
        .await
        .map_err(|_| ApiError::bad("内部任务失败"))?
        .map_err(|err| ApiError::bad(err.to_string()))?;
        return Ok(Json(store.save_plan(
            &session.user,
            session.admin,
            &name,
            &body.content,
            body.apply,
            Some(items),
            "llm",
        )?));
    }
    Ok(Json(store.save_plan(
        &session.user,
        session.admin,
        &name,
        &body.content,
        body.apply,
        None,
        "heuristic",
    )?))
}

async fn reviews(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Vec<crate::store::ReviewView>>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.reviews(&name, &session.user, session.admin)?))
}

async fn approve(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::store::Review>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.approve(
        &name,
        &session.user,
        session.admin,
        &id,
    )?))
}

#[derive(Deserialize)]
struct RejectBody {
    #[serde(default)]
    reason: String,
}

async fn reject(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
    Json(body): Json<RejectBody>,
) -> Result<Json<crate::store::Review>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.reject(
        &name,
        &session.user,
        session.admin,
        &id,
        &body.reason,
    )?))
}

#[derive(Deserialize)]
struct CreatePullBody {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default = "default_base")]
    base: String,
    head: String,
}

fn default_base() -> String {
    "main".into()
}

async fn pulls(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Vec<crate::store::ProposalView>>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.proposals(
        &name,
        &session.user,
        session.admin,
    )?))
}

async fn create_pull(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<CreatePullBody>,
) -> Result<Json<crate::store::ProposalView>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.create_proposal(
        &name,
        &session.user,
        session.admin,
        &body.title,
        &body.body,
        &body.base,
        &body.head,
    )?))
}

async fn approve_pull(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::store::ProposalView>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.approve_proposal(
        &name,
        &session.user,
        session.admin,
        &id,
    )?))
}

async fn merge_pull(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::store::ProposalView>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.merge_proposal(
        &name,
        &session.user,
        session.admin,
        &id,
    )?))
}

async fn close_pull(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::store::ProposalView>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.close_proposal(
        &name,
        &session.user,
        session.admin,
        &id,
    )?))
}

async fn git_refs(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<crate::store::RefsResponse>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.refs(&name, &session.user, session.admin)?))
}

async fn git_pull(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<PullRequest>,
) -> Result<Json<crate::store::PullResponse>, ApiError> {
    let session = auth(&store, &headers)?;
    let store2 = store.clone();
    let user = session.user.clone();
    let admin = session.admin;
    let result = tokio::task::spawn_blocking(move || store2.pull(&name, &user, admin, body))
        .await
        .map_err(|_| ApiError::bad("内部任务失败"))?;
    Ok(Json(result?))
}

async fn git_push(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<PushRequest>,
) -> Result<Json<crate::store::PushResponse>, ApiError> {
    let session = auth(&store, &headers)?;
    let store2 = store.clone();
    let user = session.user.clone();
    let admin = session.admin;
    let result = tokio::task::spawn_blocking(move || store2.push(&name, &user, admin, body))
        .await
        .map_err(|_| ApiError::bad("内部任务失败"))?;
    Ok(Json(result?))
}

fn auth(store: &Store, headers: &HeaderMap) -> Result<AuthSession, ApiError> {
    store.authenticate(token(headers)?)
}

fn token(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ApiError::unauthorized("需要登录"))
}

fn base_url(headers: &HeaderMap) -> String {
    if let Ok(url) = std::env::var("AIT_PUBLIC_URL") {
        let url = url.trim().trim_end_matches('/').to_string();
        if (url.starts_with("http://") || url.starts_with("https://")) && safe_public_url(&url) {
            return url;
        }
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|host| safe_host(host))
        .unwrap_or("127.0.0.1:7878");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok());
    let proto = if proto.is_some_and(|value| value.eq_ignore_ascii_case("https")) {
        "https"
    } else {
        "http"
    };
    format!("{proto}://{host}")
}

fn safe_public_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or("");
    !rest.is_empty()
        && !rest.contains("://")
        && url
            .chars()
            .all(|ch| !ch.is_whitespace() && ch != '<' && ch != '>' && ch != '"' && ch != '\'')
}

fn safe_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        })
}
