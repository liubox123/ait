use std::io::Write;
use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::perm::{Duty, Member, Rule};
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
        .route("/api/repos/:name/pulls/:id/ai-review", post(ai_review_pull))
        .route("/api/repos/:name/reviews/:id/ai-review", post(ai_review_push))
        .route("/api/repos/:name/policy/preview", post(policy_preview))
        .route("/api/repos/:name/ai", get(get_ai).put(put_ai))
        .route("/api/repos/:name/ai-reviews", get(ai_reviews))
        .route("/api/repos/:name/checks", get(checks).post(report_check))
        .route("/api/repos/:name/checks/run", post(run_checks))
        .route("/api/repos/:name/checks/:id", get(check_detail))
        .route("/api/repos/:name/claims", get(claims).post(add_claim))
        .route("/api/repos/:name/claims/:id", axum::routing::delete(release_claim))
        .route("/api/repos/:name/events", get(events))
        .route("/api/users", get(users))
        .route("/ui", get(web_index))
        .route("/ui/", get(web_index))
        .route("/ui/*path", get(web_asset))
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
    #[serde(default)]
    members: Option<Vec<Member>>,
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
        body.members,
    )?))
}

#[derive(Deserialize)]
struct PreviewBody {
    #[serde(default)]
    reference: String,
    rules: Vec<Rule>,
    duties: Vec<Duty>,
    #[serde(default)]
    members: Vec<Member>,
}

async fn policy_preview(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<crate::store::PolicyPreview>, ApiError> {
    let _session = auth(&store, &headers)?;
    blocking(move || {
        store.policy_preview(&name, &body.reference, body.rules, body.duties, body.members)
    })
    .await
}

async fn users(
    State(store): State<Store>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.user_names()?))
}

async fn get_ai(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<crate::collab::AiConfigView>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.ai_config(&name)?))
}

async fn put_ai(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<crate::store::AiConfigUpdate>,
) -> Result<Json<crate::collab::AiConfigView>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.put_ai_config(
        &session.user,
        session.admin,
        &name,
        body,
    )?))
}

#[derive(Deserialize)]
struct TargetQuery {
    #[serde(default)]
    target: String,
}

async fn ai_reviews(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<TargetQuery>,
) -> Result<Json<Vec<crate::collab::AiReview>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.ai_reviews(&name, &query.target)?))
}

async fn ai_review_pull(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::collab::AiReview>, ApiError> {
    let session = auth(&store, &headers)?;
    if !id.starts_with("pr-") {
        return Err(ApiError::not_found("合并请求不存在"));
    }
    blocking(move || store.run_ai_review(&name, &session.user, &id)).await
}

async fn ai_review_push(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::collab::AiReview>, ApiError> {
    let session = auth(&store, &headers)?;
    if !id.starts_with("r-") {
        return Err(ApiError::not_found("评审不存在"));
    }
    blocking(move || store.run_ai_review(&name, &session.user, &id)).await
}

async fn checks(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<crate::store::ChecksView>, ApiError> {
    let _session = auth(&store, &headers)?;
    blocking(move || store.checks_view(&name, &query.reference)).await
}

async fn check_detail(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<crate::collab::CheckRun>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.check_run(&name, &id)?))
}

async fn report_check(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<crate::store::CheckReport>,
) -> Result<Json<crate::collab::CheckRun>, ApiError> {
    let session = auth(&store, &headers)?;
    blocking(move || store.report_check(&name, &session.user, session.admin, body)).await
}

#[derive(Deserialize)]
struct RunChecksBody {
    #[serde(default)]
    reference: String,
    #[serde(default)]
    name: Option<String>,
}

async fn run_checks(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<RunChecksBody>,
) -> Result<Json<Vec<crate::collab::CheckRun>>, ApiError> {
    let session = auth(&store, &headers)?;
    let queue = store.clone();
    let repo = name.clone();
    let jobs = blocking(move || {
        queue.queue_server_checks(&repo, &session.user, &body.reference, body.name.as_deref())
    })
    .await?
    .0;
    let runs = jobs.iter().map(|(run, _)| run.clone()).collect();
    std::thread::spawn(move || {
        for (run, spec) in jobs {
            store.execute_check(&name, &run, &spec);
        }
    });
    Ok(Json(runs))
}

async fn claims(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Vec<crate::collab::Claim>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.claims(&name)?))
}

#[derive(Deserialize)]
struct ClaimBody {
    paths: Vec<String>,
    intent: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    ttl_minutes: Option<i64>,
}

async fn add_claim(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ClaimBody>,
) -> Result<Json<crate::store::ClaimResult>, ApiError> {
    let session = auth(&store, &headers)?;
    Ok(Json(store.add_claim(
        &name,
        &session.user,
        body.paths,
        &body.intent,
        body.branch,
        body.ttl_minutes,
    )?))
}

async fn release_claim(
    State(store): State<Store>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let session = auth(&store, &headers)?;
    store.release_claim(&name, &session.user, session.admin, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn events(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<Vec<crate::collab::Event>>, ApiError> {
    let _session = auth(&store, &headers)?;
    Ok(Json(store.events(&name, query.limit.unwrap_or(100))?))
}

async fn blocking<T, F>(task: F) -> Result<Json<T>, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| ApiError::bad("内部任务失败"))?
        .map(Json)
}

/// 推送或创建合并请求后，按仓库配置自动运行 AI 审查和服务端校验。
fn after_change(store: &Store, name: &str, user: &str, ai_target: Option<String>, oid: Option<String>) {
    let store = store.clone();
    let name = name.to_string();
    let user = user.to_string();
    std::thread::spawn(move || {
        if let Some(target) = ai_target {
            if store.ai_auto(&name) {
                let _ = store.run_ai_review(&name, &user, &target);
            }
        }
        if let (Some(oid), Some(_)) = (oid, crate::store::check_runner()) {
            if let Ok(jobs) = store.queue_server_checks(&name, &user, &oid, None) {
                for (run, spec) in jobs {
                    store.execute_check(&name, &run, &spec);
                }
            }
        }
    });
}

async fn web_index() -> impl IntoResponse {
    web_file("index.html").await
}

async fn web_asset(Path(path): Path<String>) -> impl IntoResponse {
    web_file(&path).await
}

/// 提供 `AIT_WEB_DIR` 里构建好的 ait-web 前端；找不到的路径交给前端路由。
async fn web_file(path: &str) -> axum::response::Response {
    let Ok(root) = std::env::var("AIT_WEB_DIR") else {
        return (StatusCode::NOT_FOUND, "没有配置 AIT_WEB_DIR").into_response();
    };
    let root = PathBuf::from(root);
    let clean: Vec<&str> = path
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .collect();
    let mut file = root.join(clean.join("/"));
    let hashed = clean.first() == Some(&"assets");
    if clean.is_empty() || !file.is_file() {
        if hashed {
            return (StatusCode::NOT_FOUND, "找不到文件").into_response();
        }
        file = root.join("index.html");
    }
    // 构建产物带内容哈希，可以长期缓存；入口页每次都要重新取。
    let cache = if hashed {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let content_type = match file.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, cache),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "找不到页面").into_response(),
    }
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
    let view = store.create_proposal(
        &name,
        &session.user,
        session.admin,
        &body.title,
        &body.body,
        &body.base,
        &body.head,
    )?;
    after_change(
        &store,
        &name,
        &session.user,
        Some(view.proposal.id.clone()),
        None,
    );
    Ok(Json(view))
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
    let repo = name.clone();
    let result = tokio::task::spawn_blocking(move || store2.push(&repo, &user, admin, body))
        .await
        .map_err(|_| ApiError::bad("内部任务失败"))??;
    let ai_target = match &result.review_id {
        Some(id) => Some(id.clone()),
        None => store.open_pull_for(&name, &result.reference),
    };
    after_change(
        &store,
        &name,
        &session.user,
        ai_target,
        Some(result.oid.to_string()),
    );
    Ok(Json(result))
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
