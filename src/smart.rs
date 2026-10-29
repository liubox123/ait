//! Git smart HTTP（`git clone/fetch/push http://主机/r/仓库`），以及按 pack 取对象、
//! 受保护分支、改密码、注销全部会话等接口。推送与 ait 推送走同样的权限检查。

use std::io::Read;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Deserialize;

use crate::git::Oid;
use crate::store::{ApiError, AuthSession, RefCommand, Store};

const AGENT: &str = concat!("agent=ait/", env!("CARGO_PKG_VERSION"));
const MAX_DATA: usize = 65515;

pub fn routes() -> Router<Store> {
    Router::new()
        .route("/api/git/:name/fetch-pack", post(fetch_pack))
        .route(
            "/api/repos/:name/protected",
            get(get_protected).put(put_protected),
        )
        .route("/api/password", post(change_password))
        .route("/api/logout-all", post(logout_all))
        .route("/r/:name/info/refs", get(info_refs))
        .route("/r/:name/git-upload-pack", post(upload_pack))
        .route("/r/:name/git-receive-pack", post(receive_pack))
}

// ---------- 认证 ----------

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
}

fn api_session(store: &Store, headers: &HeaderMap) -> Result<AuthSession, ApiError> {
    store.authenticate(bearer(headers).ok_or_else(|| ApiError::unauthorized("需要登录"))?)
}

/// Git 客户端用 Basic（用户名 + 口令或令牌），也接受 Bearer。
fn git_session(store: &Store, headers: &HeaderMap) -> Result<AuthSession, Response> {
    let challenge = |message: &str| {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"ait\"")],
            message.to_string(),
        )
            .into_response()
    };
    if let Some(token) = bearer(headers) {
        return store.authenticate(token).map_err(|err| challenge(&err.message));
    }
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
    else {
        return Err(challenge("需要登录"));
    };
    let decoded = STANDARD
        .decode(value.trim())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| challenge("认证信息格式错误"))?;
    let (user, secret) = decoded
        .split_once(':')
        .ok_or_else(|| challenge("认证信息格式错误"))?;
    store
        .verify_basic(user, secret)
        .map_err(|err| challenge(&err.message))
}

fn repo_name(name: &str) -> String {
    name.strip_suffix(".git").unwrap_or(name).to_string()
}

// ---------- ait 客户端接口 ----------

#[derive(Deserialize)]
struct FetchBody {
    #[serde(default)]
    wants: Vec<Oid>,
    #[serde(default)]
    haves: Vec<Oid>,
    #[serde(default)]
    include_tags: bool,
}

async fn fetch_pack(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<FetchBody>,
) -> Result<Response, ApiError> {
    api_session(&store, &headers)?;
    let pack = tokio::task::spawn_blocking(move || {
        store.fetch_pack(&name, &body.wants, &body.haves, body.include_tags)
    })
    .await
    .map_err(|_| ApiError::bad("内部任务失败"))??;
    Ok(([(header::CONTENT_TYPE, "application/x-git-packfile")], pack).into_response())
}

async fn get_protected(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    api_session(&store, &headers)?;
    Ok(Json(
        serde_json::json!({ "patterns": store.protected_patterns(&name)? }),
    ))
}

#[derive(Deserialize)]
struct ProtectedBody {
    patterns: Vec<String>,
}

async fn put_protected(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ProtectedBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = api_session(&store, &headers)?;
    let patterns = store.set_protected(&name, &session.user, session.admin, body.patterns)?;
    Ok(Json(serde_json::json!({ "patterns": patterns })))
}

#[derive(Deserialize)]
struct PasswordBody {
    old_password: String,
    new_password: String,
}

async fn change_password(
    State(store): State<Store>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> Result<Json<AuthSession>, ApiError> {
    let session = api_session(&store, &headers)?;
    let store2 = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        store2.change_password(&session.user, &body.old_password, &body.new_password)
    })
    .await
    .map_err(|_| ApiError::bad("内部任务失败"))?;
    Ok(Json(result?))
}

async fn logout_all(
    State(store): State<Store>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = api_session(&store, &headers)?;
    let count = store.logout_all(&session.user)?;
    Ok(Json(serde_json::json!({ "revoked": count })))
}

// ---------- pkt-line ----------

fn pkt(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
}

fn pkt_str(out: &mut Vec<u8>, text: &str) {
    pkt(out, text.as_bytes());
}

fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

fn delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

fn sideband(out: &mut Vec<u8>, band: u8, data: &[u8]) {
    for chunk in data.chunks(MAX_DATA) {
        let mut buf = Vec::with_capacity(chunk.len() + 1);
        buf.push(band);
        buf.extend_from_slice(chunk);
        pkt(out, &buf);
    }
}

enum Pkt {
    Data(Vec<u8>),
    Flush,
    Delim,
}

/// 解析 pkt-line，返回各行和剩余字节（推送时紧跟着 pack）。
fn parse_pkts(mut data: &[u8], stop_at_flush: bool) -> Result<(Vec<Pkt>, &[u8]), String> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let len = std::str::from_utf8(&data[..4])
            .ok()
            .and_then(|text| usize::from_str_radix(text, 16).ok())
            .ok_or("pkt-line 长度错误")?;
        match len {
            0 => {
                data = &data[4..];
                out.push(Pkt::Flush);
                if stop_at_flush {
                    return Ok((out, data));
                }
            }
            1 | 2 => {
                data = &data[4..];
                out.push(Pkt::Delim);
            }
            3 => return Err("pkt-line 长度错误".into()),
            _ => {
                if data.len() < len {
                    return Err("pkt-line 被截断".into());
                }
                out.push(Pkt::Data(data[4..len].to_vec()));
                data = &data[len..];
            }
        }
    }
    Ok((out, data))
}

fn line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end_matches('\n').to_string()
}

fn body_bytes(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, String> {
    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("gzip"));
    if !gzip {
        return Ok(body.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&body[..])
        .read_to_end(&mut out)
        .map_err(|err| format!("请求体解压失败: {err}"))?;
    Ok(out)
}

fn protocol_v2(headers: &HeaderMap) -> bool {
    headers
        .get("git-protocol")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(':').any(|item| item.trim() == "version=2"))
}

fn git_response(kind: &str, body: Vec<u8>) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&format!("application/x-{kind}")) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn git_error(status: StatusCode, err: ApiError) -> Response {
    (status, err.message).into_response()
}

fn error_pkt(kind: &str, message: &str) -> Response {
    let mut out = Vec::new();
    pkt_str(&mut out, &format!("ERR {}\n", message.replace('\n', " ")));
    git_response(kind, out)
}

// ---------- 引用通告 ----------

#[derive(Deserialize)]
struct ServiceQuery {
    service: Option<String>,
}

struct Advert {
    head: Option<(String, Option<Oid>)>,
    refs: Vec<(String, Oid, Option<Oid>)>,
}

fn load_advert(store: &Store, name: &str) -> Result<Advert, ApiError> {
    let refs = store.advertised_refs(name)?;
    let head = store.head_branch(name)?.map(|target| {
        let oid = refs
            .iter()
            .find(|(name, _, _)| *name == target)
            .map(|(_, oid, _)| *oid);
        (target, oid)
    });
    Ok(Advert { head, refs })
}

fn v0_advert(advert: &Advert, service: &str) -> Vec<u8> {
    let mut caps = match service {
        "git-upload-pack" => {
            "side-band-64k ofs-delta include-tag no-progress allow-tip-sha1-in-want".to_string()
        }
        _ => "report-status delete-refs side-band-64k quiet ofs-delta".to_string(),
    };
    if service == "git-upload-pack" {
        if let Some((target, _)) = &advert.head {
            caps.push_str(&format!(" symref=HEAD:{target}"));
        }
    }
    caps.push_str(&format!(" {AGENT} object-format=sha1"));
    let mut lines: Vec<(Oid, String)> = Vec::new();
    if service == "git-upload-pack" {
        if let Some((_, Some(oid))) = &advert.head {
            lines.push((*oid, "HEAD".into()));
        }
    }
    for (name, oid, peeled) in &advert.refs {
        lines.push((*oid, name.clone()));
        if service == "git-upload-pack" {
            if let Some(peeled) = peeled {
                lines.push((*peeled, format!("{name}^{{}}")));
            }
        }
    }
    let mut out = Vec::new();
    pkt_str(&mut out, &format!("# service={service}\n"));
    flush(&mut out);
    if lines.is_empty() {
        pkt(
            &mut out,
            format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO).as_bytes(),
        );
    }
    for (index, (oid, name)) in lines.iter().enumerate() {
        if index == 0 {
            pkt(&mut out, format!("{oid} {name}\0{caps}\n").as_bytes());
        } else {
            pkt_str(&mut out, &format!("{oid} {name}\n"));
        }
    }
    flush(&mut out);
    out
}

fn v2_capabilities() -> Vec<u8> {
    let mut out = Vec::new();
    pkt_str(&mut out, "version 2\n");
    pkt_str(&mut out, &format!("{AGENT}\n"));
    pkt_str(&mut out, "ls-refs=unborn\n");
    pkt_str(&mut out, "fetch\n");
    pkt_str(&mut out, "server-option\n");
    pkt_str(&mut out, "object-format=sha1\n");
    flush(&mut out);
    out
}

async fn info_refs(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<ServiceQuery>,
) -> Response {
    let service = query.service.unwrap_or_default();
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return (StatusCode::FORBIDDEN, "只支持 Git smart HTTP").into_response();
    }
    if let Err(response) = git_session(&store, &headers) {
        return response;
    }
    let name = repo_name(&name);
    let kind = format!("{service}-advertisement");
    if service == "git-upload-pack" && protocol_v2(&headers) {
        if let Err(err) = store.head_branch(&name) {
            return git_error(StatusCode::NOT_FOUND, err);
        }
        return git_response(&kind, v2_capabilities());
    }
    match load_advert(&store, &name) {
        Ok(advert) => git_response(&kind, v0_advert(&advert, &service)),
        Err(err) => git_error(err.status, err),
    }
}

// ---------- upload-pack ----------

async fn upload_pack(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    const KIND: &str = "git-upload-pack-result";
    if let Err(response) = git_session(&store, &headers) {
        return response;
    }
    let name = repo_name(&name);
    let data = match body_bytes(&headers, body) {
        Ok(data) => data,
        Err(err) => return error_pkt(KIND, &err),
    };
    let v2 = protocol_v2(&headers) || data.get(4..12) == Some(b"command=");
    let result = tokio::task::spawn_blocking(move || {
        if v2 {
            upload_v2(&store, &name, &data)
        } else {
            upload_v0(&store, &name, &data)
        }
    })
    .await;
    match result {
        Ok(Ok(out)) => git_response(KIND, out),
        Ok(Err(message)) => error_pkt(KIND, &message),
        Err(_) => error_pkt(KIND, "内部任务失败"),
    }
}

fn upload_v2(store: &Store, name: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let (pkts, _) = parse_pkts(data, false)?;
    let mut command = String::new();
    let mut args = Vec::new();
    let mut in_args = false;
    for item in pkts {
        match item {
            Pkt::Delim => in_args = true,
            Pkt::Flush => break,
            Pkt::Data(bytes) => {
                let text = line(&bytes);
                if in_args {
                    args.push(text);
                } else if let Some(cmd) = text.strip_prefix("command=") {
                    command = cmd.to_string();
                }
            }
        }
    }
    match command.as_str() {
        "ls-refs" => ls_refs(store, name, &args),
        "fetch" => fetch_v2(store, name, &args),
        "" => Err("缺少 command".into()),
        other => Err(format!("不支持的命令 {other}")),
    }
}

fn ls_refs(store: &Store, name: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let advert = load_advert(store, name).map_err(|err| err.message)?;
    let peel = args.iter().any(|arg| arg == "peel");
    let symrefs = args.iter().any(|arg| arg == "symrefs");
    let unborn = args.iter().any(|arg| arg == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|arg| arg.strip_prefix("ref-prefix "))
        .collect();
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let mut out = Vec::new();
    if wanted("HEAD") {
        if let Some((target, oid)) = &advert.head {
            let sym = if symrefs {
                format!(" symref-target:{target}")
            } else {
                String::new()
            };
            match oid {
                Some(oid) => pkt_str(&mut out, &format!("{oid} HEAD{sym}\n")),
                None if unborn => pkt_str(&mut out, &format!("unborn HEAD{sym}\n")),
                None => {}
            }
        }
    }
    for (refname, oid, peeled) in &advert.refs {
        if !wanted(refname) {
            continue;
        }
        let mut text = format!("{oid} {refname}");
        if peel {
            if let Some(peeled) = peeled {
                text.push_str(&format!(" peeled:{peeled}"));
            }
        }
        text.push('\n');
        pkt_str(&mut out, &text);
    }
    flush(&mut out);
    Ok(out)
}

fn parse_oid(text: &str) -> Result<Oid, String> {
    text.trim()
        .parse::<Oid>()
        .map_err(|_| format!("非法对象名 {text}"))
}

fn fetch_v2(store: &Store, name: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut include_tag = false;
    let mut progress = true;
    for arg in args {
        if let Some(oid) = arg.strip_prefix("want ") {
            wants.push(parse_oid(oid)?);
        } else if let Some(oid) = arg.strip_prefix("have ") {
            haves.push(parse_oid(oid)?);
        } else if arg == "done" {
            done = true;
        } else if arg == "include-tag" {
            include_tag = true;
        } else if arg == "no-progress" {
            progress = false;
        } else if arg.starts_with("deepen") || arg.starts_with("shallow ") {
            return Err("服务端不支持浅克隆".into());
        } else if arg.starts_with("filter ") {
            return Err("服务端不支持部分克隆".into());
        } else if arg.starts_with("want-ref ") {
            return Err("服务端不支持 want-ref".into());
        }
    }
    if wants.is_empty() {
        let mut out = Vec::new();
        flush(&mut out);
        return Ok(out);
    }
    let mut out = Vec::new();
    if !done {
        let common = store
            .common_objects(name, &haves)
            .map_err(|err| err.message)?;
        pkt_str(&mut out, "acknowledgments\n");
        if common.is_empty() {
            pkt_str(&mut out, "NAK\n");
            flush(&mut out);
            return Ok(out);
        }
        for oid in &common {
            pkt_str(&mut out, &format!("ACK {oid}\n"));
        }
        pkt_str(&mut out, "ready\n");
        delim(&mut out);
    }
    let pack = store
        .fetch_pack(name, &wants, &haves, include_tag)
        .map_err(|err| err.message)?;
    pkt_str(&mut out, "packfile\n");
    if progress {
        sideband(&mut out, 2, b"ait: sending pack\n");
    }
    sideband(&mut out, 1, &pack);
    flush(&mut out);
    Ok(out)
}

fn upload_v0(store: &Store, name: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let (pkts, _) = parse_pkts(data, false)?;
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut caps = Vec::new();
    let mut done = false;
    for item in pkts {
        let Pkt::Data(bytes) = item else { continue };
        let text = line(&bytes);
        if let Some(rest) = text.strip_prefix("want ") {
            let mut parts = rest.split(' ');
            wants.push(parse_oid(parts.next().unwrap_or(""))?);
            if caps.is_empty() {
                caps = parts.map(str::to_string).collect();
            }
        } else if let Some(oid) = text.strip_prefix("have ") {
            haves.push(parse_oid(oid)?);
        } else if text == "done" {
            done = true;
        } else if text.starts_with("shallow ") || text.starts_with("deepen") {
            return Err("服务端不支持浅克隆".into());
        }
    }
    let common = store
        .common_objects(name, &haves)
        .map_err(|err| err.message)?;
    let mut out = Vec::new();
    if !done {
        pkt_str(&mut out, "NAK\n");
        return Ok(out);
    }
    match common.first() {
        Some(oid) => pkt_str(&mut out, &format!("ACK {oid}\n")),
        None => pkt_str(&mut out, "NAK\n"),
    }
    let include_tag = caps.iter().any(|cap| cap == "include-tag");
    let pack = store
        .fetch_pack(name, &wants, &haves, include_tag)
        .map_err(|err| err.message)?;
    if caps.iter().any(|cap| cap == "side-band-64k" || cap == "side-band") {
        sideband(&mut out, 1, &pack);
        flush(&mut out);
    } else {
        out.extend_from_slice(&pack);
    }
    Ok(out)
}

// ---------- receive-pack ----------

async fn receive_pack(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    const KIND: &str = "git-receive-pack-result";
    let session = match git_session(&store, &headers) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let name = repo_name(&name);
    let data = match body_bytes(&headers, body) {
        Ok(data) => data,
        Err(err) => return error_pkt(KIND, &err),
    };
    let (pkts, pack) = match parse_pkts(&data, true) {
        Ok(parsed) => parsed,
        Err(err) => return error_pkt(KIND, &err),
    };
    let mut commands = Vec::new();
    let mut caps: Vec<String> = Vec::new();
    for item in pkts {
        let Pkt::Data(bytes) = item else { continue };
        let (text, cap_text) = match bytes.iter().position(|b| *b == 0) {
            Some(pos) => (line(&bytes[..pos]), Some(line(&bytes[pos + 1..]))),
            None => (line(&bytes), None),
        };
        if let Some(cap_text) = cap_text {
            caps = cap_text.split(' ').map(str::to_string).collect();
        }
        if text.starts_with("shallow ") {
            continue;
        }
        let parts: Vec<&str> = text.splitn(3, ' ').collect();
        if parts.len() != 3 {
            return error_pkt(KIND, &format!("无法解析推送命令 {text}"));
        }
        let (old, new) = match (parse_oid(parts[0]), parse_oid(parts[1])) {
            (Ok(old), Ok(new)) => (old, new),
            _ => return error_pkt(KIND, &format!("无法解析推送命令 {text}")),
        };
        commands.push(RefCommand {
            old,
            new,
            reference: parts[2].to_string(),
        });
    }
    let pack = pack.to_vec();
    let store2 = store.clone();
    let user = session.user.clone();
    let repo = name.clone();
    let cmds = commands.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        store2.receive_commands(&repo, &user, session.admin, &cmds, &pack)
    })
    .await;
    let mut report = Vec::new();
    match outcome {
        Ok(Ok(results)) => {
            pkt_str(&mut report, "unpack ok\n");
            for (reference, result) in &results {
                match result {
                    Ok(response) if response.status == "review" => pkt_str(
                        &mut report,
                        &format!("ng {reference} {}\n", response.message.replace('\n', " ")),
                    ),
                    Ok(_) => pkt_str(&mut report, &format!("ok {reference}\n")),
                    Err(message) => {
                        pkt_str(&mut report, &format!("ng {reference} {message}\n"))
                    }
                }
            }
        }
        Ok(Err(err)) => {
            pkt_str(&mut report, &format!("unpack {}\n", err.message.replace('\n', " ")));
            for cmd in &commands {
                pkt_str(&mut report, &format!("ng {} unpacker error\n", cmd.reference));
            }
        }
        Err(_) => return error_pkt(KIND, "内部任务失败"),
    }
    flush(&mut report);
    if caps.iter().any(|cap| cap == "side-band-64k" || cap == "side-band") {
        let mut out = Vec::new();
        sideband(&mut out, 1, &report);
        flush(&mut out);
        git_response(KIND, out)
    } else {
        git_response(KIND, report)
    }
}
