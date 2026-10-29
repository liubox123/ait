//! 多人与多个 agent 协作时的校验、AI 审查和工作意图。
//!
//! 这里只放纯逻辑；文件存储、权限和合并门禁的接线在 `store.rs`。

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::git::{ChangeKind, FileDiff, Oid};
use crate::perm::{path_match, Access, Manifest, Policy};

pub const CHECKS_FILE: &str = ".ait/checks.toml";

/// `.ait/checks.toml` 里的一项校验。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckSpec {
    pub name: String,
    #[serde(default)]
    pub run: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub required: bool,
    /// 只在改动命中这些路径时适用；为空表示总是适用。
    #[serde(default)]
    pub paths: Vec<String>,
    /// `report` 只接受上报，`server` 只由服务端执行，`any` 两者皆可。
    #[serde(default = "default_runner")]
    pub runner: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_runner() -> String {
    "any".into()
}

fn default_timeout() -> u64 {
    600
}

#[derive(Deserialize)]
struct ChecksFile {
    #[serde(default, rename = "check")]
    checks: Vec<CheckSpec>,
}

pub fn parse_checks(text: &str) -> anyhow::Result<Vec<CheckSpec>> {
    let file: ChecksFile = toml::from_str(text).context(".ait/checks.toml 格式错误")?;
    let mut seen = std::collections::HashSet::new();
    for spec in &file.checks {
        if !valid_check_name(&spec.name) {
            bail!(
                "校验名只能包含字母、数字、点、下划线和短横线: {}",
                spec.name
            );
        }
        if !seen.insert(spec.name.clone()) {
            bail!("校验名重复: {}", spec.name);
        }
        if !matches!(spec.runner.as_str(), "report" | "server" | "any") {
            bail!("校验 {} 的 runner 只能是 report、server 或 any", spec.name);
        }
    }
    Ok(file.checks)
}

pub fn valid_check_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl CheckSpec {
    pub fn applies(&self, paths: &[String]) -> bool {
        self.paths.is_empty()
            || paths
                .iter()
                .any(|path| self.paths.iter().any(|pattern| path_match(pattern, path)))
    }
}

/// 一次校验结果。同一提交、同一校验名以最新一条为准。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: String,
    pub oid: Oid,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub log: String,
    pub reporter: String,
    /// 服务端执行、仓库所有者或 ci 账号上报的结果才能满足必过校验。
    pub trusted: bool,
    pub runner: String,
    pub created_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
}

pub const CHECK_STATUSES: &[&str] = &[
    "pending", "running", "success", "failure", "error", "skipped",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckGate {
    pub name: String,
    pub required: bool,
    pub status: String,
    pub trusted: bool,
    pub reporter: Option<String>,
    pub summary: String,
    pub run_id: Option<String>,
}

impl CheckGate {
    pub fn passed(&self) -> bool {
        self.status == "success" && self.trusted
    }
}

pub fn latest_runs(runs: &[CheckRun], oid: Oid) -> Vec<CheckRun> {
    let mut latest: Vec<CheckRun> = Vec::new();
    for run in runs.iter().filter(|run| run.oid == oid) {
        match latest.iter_mut().find(|item| item.name == run.name) {
            Some(item) if item.created_at <= run.created_at => *item = run.clone(),
            Some(_) => {}
            None => latest.push(run.clone()),
        }
    }
    latest
}

/// 汇总某个提交上各项校验的状态。`extra_required` 是职责限制里额外要求的校验。
pub fn check_gates(
    specs: &[CheckSpec],
    extra_required: &[String],
    runs: &[CheckRun],
    oid: Oid,
    paths: &[String],
) -> Vec<CheckGate> {
    let latest = latest_runs(runs, oid);
    let mut gates = Vec::new();
    let mut push_gate = |name: &str, required: bool| {
        if gates.iter().any(|gate: &CheckGate| gate.name == name) {
            return;
        }
        let run = latest.iter().find(|run| run.name == name);
        gates.push(CheckGate {
            name: name.to_string(),
            required,
            status: run
                .map(|run| run.status.clone())
                .unwrap_or_else(|| "missing".into()),
            trusted: run.map(|run| run.trusted).unwrap_or(false),
            reporter: run.map(|run| run.reporter.clone()),
            summary: run.map(|run| run.summary.clone()).unwrap_or_default(),
            run_id: run.map(|run| run.id.clone()),
        });
    };
    for spec in specs.iter().filter(|spec| spec.applies(paths)) {
        push_gate(
            &spec.name,
            spec.required || extra_required.contains(&spec.name),
        );
    }
    for name in extra_required {
        push_gate(name, true);
    }
    for run in &latest {
        push_gate(&run.name, false);
    }
    gates
}

pub fn failing_required(gates: &[CheckGate]) -> Vec<String> {
    gates
        .iter()
        .filter(|gate| gate.required && !gate.passed())
        .map(|gate| format!("{}（{}）", gate.name, gate_label(gate)))
        .collect()
}

fn gate_label(gate: &CheckGate) -> &'static str {
    match gate.status.as_str() {
        "success" if !gate.trusted => "结果来自不受信任的上报",
        "success" => "通过",
        "missing" => "还没有结果",
        "pending" | "running" => "进行中",
        "skipped" => "已跳过",
        _ => "未通过",
    }
}

/// 在临时目录里用 Docker 执行一项校验。容器没有网络，只挂载这次提交的文件。
pub fn run_in_docker(workdir: &Path, spec: &CheckSpec, container: &str) -> (String, String) {
    let image = if spec.image.trim().is_empty() {
        std::env::var("AIT_CHECK_IMAGE").unwrap_or_else(|_| "alpine:3".into())
    } else {
        spec.image.trim().to_string()
    };
    if let Ok(allowed) = std::env::var("AIT_CHECK_IMAGES") {
        let allowed: Vec<&str> = allowed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !allowed.is_empty() && !allowed.contains(&image.as_str()) {
            return (
                "error".into(),
                format!("镜像 {image} 不在 AIT_CHECK_IMAGES 允许列表里"),
            );
        }
    }
    if spec.run.trim().is_empty() {
        return ("error".into(), "校验没有填写 run 命令".into());
    }
    let log_path = workdir.with_extension("log");
    let log_file = match fs::File::create(&log_path) {
        Ok(file) => file,
        Err(err) => return ("error".into(), format!("无法创建日志文件: {err}")),
    };
    let log_err = match log_file.try_clone() {
        Ok(file) => file,
        Err(err) => return ("error".into(), format!("无法创建日志文件: {err}")),
    };
    let mut command = Command::new(std::env::var("AIT_DOCKER").unwrap_or_else(|_| "docker".into()));
    command.args([
        "run",
        "--rm",
        "--name",
        container,
        "--network",
        "none",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "512",
        "--memory",
        "2g",
        "--cpus",
        "2",
    ]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = fs::metadata(workdir) {
            command.args(["--user", &format!("{}:{}", meta.uid(), meta.gid())]);
        }
    }
    command
        .arg("-v")
        .arg(format!("{}:/work", workdir.display()))
        .args([
            "-w",
            "/work",
            "-e",
            "HOME=/tmp",
            &image,
            "sh",
            "-c",
            &spec.run,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return ("error".into(), format!("无法启动 docker: {err}")),
    };
    let deadline = Instant::now() + Duration::from_secs(spec.timeout.clamp(10, 3600));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = Command::new("docker").args(["kill", container]).output();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break None,
        }
    };
    let log = tail_text(&fs::read(&log_path).unwrap_or_default(), 64 * 1024);
    let _ = fs::remove_file(&log_path);
    match status {
        Some(status) if status.success() => ("success".into(), log),
        Some(status) => (
            "failure".into(),
            format!("{log}\n退出码: {}", status.code().unwrap_or(-1)),
        ),
        None => ("error".into(), format!("{log}\n超时，已终止")),
    }
}

fn tail_text(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max);
    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 {
        format!("…前面的输出已省略\n{text}")
    } else {
        text
    }
}

/// 按仓库配置的 AI 审查者。密钥只写不读。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 合并请求创建、更新和推送进入评审时自动审查。
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// 追加给模型的仓库审查要求。
    #[serde(default)]
    pub instructions: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfigView {
    pub enabled: bool,
    pub auto: bool,
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
    pub instructions: String,
    /// 是否能调用模型（仓库配置或服务端环境变量）。没有模型时只运行内置规则审查。
    pub llm_available: bool,
}

pub struct LlmEndpoint {
    pub url: String,
    pub model: String,
    pub key: String,
}

impl AiConfig {
    pub fn view(&self) -> AiConfigView {
        AiConfigView {
            enabled: self.enabled,
            auto: self.auto,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            has_api_key: !self.api_key.is_empty(),
            instructions: self.instructions.clone(),
            llm_available: self.endpoint().is_some(),
        }
    }

    pub fn endpoint(&self) -> Option<LlmEndpoint> {
        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let (base, key, model) = if !self.base_url.trim().is_empty() {
            (
                self.base_url.clone(),
                self.api_key.clone(),
                self.model.clone(),
            )
        } else {
            (
                env("AIT_LLM_BASE_URL")?,
                env("AIT_LLM_API_KEY").unwrap_or_default(),
                env("AIT_LLM_MODEL").unwrap_or_default(),
            )
        };
        Some(LlmEndpoint {
            url: format!("{}/chat/completions", base.trim().trim_end_matches('/')),
            model: if model.trim().is_empty() {
                "gpt-4o-mini".into()
            } else {
                model
            },
            key,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub severity: String,
    pub rule: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<usize>,
    pub message: String,
}

/// AI 审查意见。只作参考，是否通过由有权限的人决定。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiReview {
    pub id: String,
    /// `pr-…` 或 `r-…`。
    pub target: String,
    pub head: Oid,
    pub reviewer: String,
    pub verdict: String,
    pub summary: String,
    pub findings: Vec<Finding>,
    pub requested_by: String,
    pub created_at: i64,
    #[serde(default)]
    pub error: Option<String>,
}

pub struct ReviewInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub author: &'a str,
    pub manifest: &'a Manifest,
    pub policy: &'a Policy,
    pub files: &'a [FileDiff],
    pub claims: &'a [Claim],
    pub plan: &'a str,
    pub now: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffStat {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
}

pub fn diff_stat(files: &[FileDiff]) -> DiffStat {
    let mut stat = DiffStat {
        files: files.len(),
        ..DiffStat::default()
    };
    for file in files {
        for line in file.patch.lines() {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            if line.starts_with('+') {
                stat.added += 1;
            } else if line.starts_with('-') {
                stat.removed += 1;
            }
        }
    }
    stat
}

/// 超出职责限制的原因；为空表示没有超出。
pub fn limit_violations(manifest: &Manifest, stat: &DiffStat) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(max) = manifest.limits.max_files {
        if stat.files > max {
            out.push(format!("改动 {} 个文件，超过上限 {max}", stat.files));
        }
    }
    if let Some(max) = manifest.limits.max_lines {
        let lines = stat.added + stat.removed;
        if lines > max {
            out.push(format!("改动 {lines} 行，超过上限 {max}"));
        }
    }
    if manifest.limits.human_review {
        out.push("职责要求所有改动由另一位有权限的人通过".into());
    }
    out
}

fn added_lines(patch: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut line_no = 0usize;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            line_no = rest
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|part| part.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            out.push((line_no, text));
            line_no += 1;
        } else if !line.starts_with('-') && !line.starts_with('\\') {
            line_no += 1;
        }
    }
    out
}

fn has_token(text: &str, prefix: &str, min_len: usize) -> bool {
    let mut rest = text;
    while let Some(pos) = rest.find(prefix) {
        let tail = &rest[pos + prefix.len()..];
        let len = tail
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            .count();
        if len >= min_len {
            return true;
        }
        rest = tail;
    }
    false
}

fn looks_like_secret(line: &str) -> Option<&'static str> {
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
        return Some("私钥");
    }
    if has_token(line, "AKIA", 16) {
        return Some("AWS 访问密钥");
    }
    if has_token(line, "ghp_", 30) || has_token(line, "github_pat_", 30) {
        return Some("GitHub 令牌");
    }
    if has_token(line, "sk-", 24) {
        return Some("模型 API 密钥");
    }
    if has_token(line, "xoxb-", 20) || has_token(line, "xoxp-", 20) {
        return Some("Slack 令牌");
    }
    let lower = line.to_ascii_lowercase();
    let named = [
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "access_token",
    ]
    .iter()
    .any(|word| lower.contains(word));
    if named && (line.contains('=') || line.contains(':')) {
        for quote in ['"', '\''] {
            let parts: Vec<&str> = line.split(quote).collect();
            for literal in parts.iter().skip(1).step_by(2) {
                let literal_lower = literal.to_ascii_lowercase();
                if literal.len() >= 12
                    && !literal.contains(' ')
                    && !literal.contains("${")
                    && !["example", "xxx", "changeme", "your_", "<", "env"]
                        .iter()
                        .any(|word| literal_lower.contains(word))
                {
                    return Some("硬编码的口令或密钥");
                }
            }
        }
    }
    None
}

fn duty_holders(policy: &Policy, path: &str, except: &str) -> Vec<String> {
    let mut users: Vec<String> = policy
        .duties
        .iter()
        .filter(|duty| duty.user != except)
        .filter(|duty| {
            duty.paths
                .iter()
                .any(|pattern| pattern != "**" && path_match(pattern, path))
        })
        .map(|duty| duty.user.clone())
        .collect();
    users.sort();
    users.dedup();
    users
}

/// 内置规则审查：职责边界、改动规模、冲突标记、疑似密钥、删除测试、工作意图重叠。
pub fn rule_review(input: &ReviewInput<'_>) -> (String, String, Vec<Finding>) {
    let mut findings = Vec::new();
    let stat = diff_stat(input.files);
    let mut outside = Vec::new();
    let mut coordinate: Vec<String> = Vec::new();
    for file in input.files {
        match input.manifest.decide(&file.path) {
            Access::Deny => findings.push(Finding {
                severity: "error".into(),
                rule: "boundary".into(),
                path: Some(file.path.clone()),
                line: None,
                message: "这个路径对作者是禁止修改的".into(),
            }),
            Access::Review => outside.push(file.path.clone()),
            Access::Write => {}
        }
        for user in duty_holders(input.policy, &file.path, input.author) {
            if !coordinate.contains(&user) {
                coordinate.push(user);
            }
        }
        if file.kind == ChangeKind::Deleted && file.path.to_ascii_lowercase().contains("test") {
            findings.push(Finding {
                severity: "warning".into(),
                rule: "tests".into(),
                path: Some(file.path.clone()),
                line: None,
                message: "删除了测试文件，请确认不是为了让校验通过".into(),
            });
        }
        if file.binary {
            continue;
        }
        let mut todo = 0;
        let mut debug = 0;
        let added = added_lines(&file.patch);
        for (line, text) in &added {
            if text.starts_with("<<<<<<< ") || text.starts_with(">>>>>>> ") {
                findings.push(Finding {
                    severity: "error".into(),
                    rule: "conflict".into(),
                    path: Some(file.path.clone()),
                    line: Some(*line),
                    message: "残留的冲突标记".into(),
                });
            }
            if let Some(kind) = looks_like_secret(text) {
                findings.push(Finding {
                    severity: "error".into(),
                    rule: "secret".into(),
                    path: Some(file.path.clone()),
                    line: Some(*line),
                    message: format!("疑似{kind}，请移到环境变量或密钥管理里"),
                });
            }
            if text.contains("TODO") || text.contains("FIXME") {
                todo += 1;
            }
            if text.contains("dbg!(") || text.contains("console.log(") || text.contains("debugger;")
            {
                debug += 1;
            }
        }
        if debug > 0 {
            findings.push(Finding {
                severity: "info".into(),
                rule: "debug".into(),
                path: Some(file.path.clone()),
                line: None,
                message: format!("新增 {debug} 处调试输出"),
            });
        }
        if todo > 0 {
            findings.push(Finding {
                severity: "info".into(),
                rule: "todo".into(),
                path: Some(file.path.clone()),
                line: None,
                message: format!("新增 {todo} 处 TODO/FIXME"),
            });
        }
        if added.len() > 800 {
            findings.push(Finding {
                severity: "info".into(),
                rule: "size".into(),
                path: Some(file.path.clone()),
                line: None,
                message: format!("单个文件新增 {} 行，建议拆分评审", added.len()),
            });
        }
    }
    if !outside.is_empty() {
        findings.push(Finding {
            severity: "warning".into(),
            rule: "boundary".into(),
            path: None,
            line: None,
            message: format!(
                "{} 个文件超出作者的写权限，需要有权限的人通过: {}",
                outside.len(),
                outside
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    for reason in limit_violations(input.manifest, &stat) {
        findings.push(Finding {
            severity: "warning".into(),
            rule: "limits".into(),
            path: None,
            line: None,
            message: reason,
        });
    }
    if !coordinate.is_empty() {
        findings.push(Finding {
            severity: "info".into(),
            rule: "coordinate".into(),
            path: None,
            line: None,
            message: format!(
                "改动涉及 {} 负责的目录，建议知会对方",
                coordinate.join("、")
            ),
        });
    }
    for claim in input.claims {
        if claim.user == input.author || claim.expires_at <= input.now {
            continue;
        }
        let hit: Vec<String> = input
            .files
            .iter()
            .filter(|file| {
                claim
                    .paths
                    .iter()
                    .any(|pattern| path_match(pattern, &file.path))
            })
            .map(|file| file.path.clone())
            .collect();
        if !hit.is_empty() {
            findings.push(Finding {
                severity: "warning".into(),
                rule: "claim".into(),
                path: None,
                line: None,
                message: format!(
                    "{} 正在处理这些文件（{}）: {}",
                    claim.user,
                    claim.intent,
                    hit.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                ),
            });
        }
    }
    if input.manifest.kind == "agent"
        && input
            .policy
            .member(input.author)
            .and_then(|m| m.supervisor.as_ref())
            .is_none()
    {
        findings.push(Finding {
            severity: "info".into(),
            rule: "agent".into(),
            path: None,
            line: None,
            message: "作者是 agent，但没有指定负责人".into(),
        });
    }
    let verdict = verdict_of(&findings);
    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let warnings = findings.iter().filter(|f| f.severity == "warning").count();
    let summary = format!(
        "{} 个文件，+{} -{} 行；{} 个错误，{} 个提醒",
        stat.files, stat.added, stat.removed, errors, warnings
    );
    (verdict, summary, findings)
}

fn verdict_of(findings: &[Finding]) -> String {
    if findings.iter().any(|f| f.severity == "error") {
        "request_changes".into()
    } else if findings.iter().any(|f| f.severity == "warning") {
        "comment".into()
    } else {
        "approve".into()
    }
}

/// 调用 OpenAI 兼容接口审查，返回 (结论, 摘要, 意见)。
pub fn llm_review(
    endpoint: &LlmEndpoint,
    instructions: &str,
    input: &ReviewInput<'_>,
    rule_findings: &[Finding],
) -> anyhow::Result<(String, String, Vec<Finding>)> {
    let diff: String = crate::git::unified_diff(input.files)
        .chars()
        .take(60_000)
        .collect();
    let plan: String = input.plan.chars().take(4_000).collect();
    let rules = serde_json::to_string(rule_findings).unwrap_or_default();
    let mut system = String::from(
        "你是代码审查顾问，只提供建议，是否合并由有权限的人决定。关注正确性、安全、职责边界和可维护性。\
         只输出 JSON，不要解释。格式：{\"verdict\":\"approve|comment|request_changes\",\"summary\":\"一两句话\",\
         \"findings\":[{\"severity\":\"error|warning|info\",\"path\":\"文件\",\"line\":行号,\"message\":\"意见\"}]}。\
         没有把握的问题用 info。",
    );
    if !instructions.trim().is_empty() {
        system.push_str("\n仓库审查要求：\n");
        system.push_str(instructions.trim());
    }
    let user = format!(
        "标题：{}\n说明：{}\n作者：{}（{}）\n作者职责：{}\n可直接修改：{}\n\n项目规划：\n{}\n\n内置规则已发现：{}\n\n差异：\n{}",
        input.title,
        input.body,
        input.author,
        input.manifest.kind,
        input.manifest.duty,
        input.manifest.write.join(", "),
        plan,
        rules,
        diff
    );
    let body = serde_json::json!({
        "model": endpoint.model,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut request = client.post(&endpoint.url).json(&body);
    if !endpoint.key.is_empty() {
        request = request.bearer_auth(&endpoint.key);
    }
    let response = request.send().context("调用模型接口失败")?;
    let status = response.status();
    let text = response.text().context("读取模型响应失败")?;
    if !status.is_success() {
        let short: String = text.chars().take(300).collect();
        bail!("模型接口返回 {status}: {short}");
    }
    let value: serde_json::Value = serde_json::from_str(&text).context("模型响应不是 JSON")?;
    let content = value["choices"][0]["message"]["content"]
        .as_str()
        .context("模型没有返回文本")?;
    parse_llm_review(content).context("模型输出里没有合法的审查 JSON")
}

pub fn parse_llm_review(text: &str) -> Option<(String, String, Vec<Finding>)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    let verdict = match value["verdict"].as_str().unwrap_or("comment") {
        v @ ("approve" | "comment" | "request_changes") => v.to_string(),
        _ => "comment".into(),
    };
    let summary = value["summary"].as_str().unwrap_or("").to_string();
    let mut findings = Vec::new();
    for item in value["findings"].as_array().into_iter().flatten() {
        let message = item["message"].as_str().unwrap_or("").trim();
        if message.is_empty() {
            continue;
        }
        let severity = match item["severity"].as_str().unwrap_or("info") {
            s @ ("error" | "warning" | "info") => s.to_string(),
            _ => "info".into(),
        };
        findings.push(Finding {
            severity,
            rule: "llm".into(),
            path: item["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            line: item["line"].as_u64().map(|n| n as usize),
            message: message.to_string(),
        });
    }
    Some((verdict, summary, findings))
}

/// 开始工作前登记的意图，让其他人和 agent 知道谁在改哪里。只做提醒，不加锁。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub user: String,
    pub paths: Vec<String>,
    pub intent: String,
    #[serde(default)]
    pub branch: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

fn static_prefix(pattern: &str) -> Vec<&str> {
    pattern
        .trim_matches('/')
        .split('/')
        .take_while(|seg| !seg.contains('*') && !seg.contains('?') && !seg.is_empty())
        .collect()
}

/// 两个路径规则是否可能命中同一文件（偏宽松，用于提醒）。
pub fn patterns_overlap(a: &str, b: &str) -> bool {
    if path_match(a, b) || path_match(b, a) {
        return true;
    }
    let pa = static_prefix(a);
    let pb = static_prefix(b);
    let n = pa.len().min(pb.len());
    let a_exact = !a.contains('*') && !a.contains('?');
    let b_exact = !b.contains('*') && !b.contains('?');
    if a_exact && b_exact {
        return a.trim_matches('/') == b.trim_matches('/');
    }
    pa[..n] == pb[..n]
}

pub fn claim_overlaps(paths: &[String], claims: &[Claim], user: &str, now: i64) -> Vec<Claim> {
    claims
        .iter()
        .filter(|claim| claim.user != user && claim.expires_at > now)
        .filter(|claim| {
            claim
                .paths
                .iter()
                .any(|theirs| paths.iter().any(|mine| patterns_overlap(mine, theirs)))
        })
        .cloned()
        .collect()
}

/// 仓库动态，供协作看板展示。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub at: i64,
    pub user: String,
    pub kind: String,
    #[serde(default)]
    pub target: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff_bytes;
    use crate::perm::{Duty, Limits, Member, Rule};

    fn policy() -> Policy {
        Policy {
            owners: vec!["alice".into()],
            rules: vec![
                Rule {
                    user: "alice".into(),
                    pattern: "**".into(),
                    access: "write".into(),
                },
                Rule {
                    user: "bot".into(),
                    pattern: "src/shop/**".into(),
                    access: "write".into(),
                },
            ],
            duties: vec![
                Duty {
                    user: "bot".into(),
                    summary: "商品".into(),
                    paths: vec!["src/shop/**".into()],
                },
                Duty {
                    user: "carol".into(),
                    summary: "支付".into(),
                    paths: vec!["src/pay/**".into()],
                },
            ],
            members: vec![Member {
                user: "bot".into(),
                kind: "agent".into(),
                supervisor: Some("alice".into()),
                limits: Limits {
                    max_files: Some(1),
                    ..Limits::default()
                },
            }],
        }
    }

    #[test]
    fn checks_file_parses_and_gates_follow_trust() {
        let specs = parse_checks(
            "[[check]]\nname = \"test\"\nrun = \"cargo test\"\nrequired = true\npaths = [\"src/**\"]\n\n[[check]]\nname = \"lint\"\nrun = \"true\"\n",
        )
        .unwrap();
        assert_eq!(specs.len(), 2);
        assert!(parse_checks("[[check]]\nname = \"a b\"\n").is_err());
        let oid = Oid::parse("1111111111111111111111111111111111111111").unwrap();
        let run = |status: &str, trusted: bool, at: i64| CheckRun {
            id: format!("c{at}"),
            oid,
            name: "test".into(),
            status: status.into(),
            summary: String::new(),
            url: String::new(),
            log: String::new(),
            reporter: "x".into(),
            trusted,
            runner: "report".into(),
            created_at: at,
            finished_at: None,
        };
        let paths = vec!["src/lib.rs".to_string()];
        let gates = check_gates(&specs, &[], &[], oid, &paths);
        assert_eq!(failing_required(&gates).len(), 1);
        let gates = check_gates(&specs, &[], &[run("success", false, 1)], oid, &paths);
        assert_eq!(failing_required(&gates).len(), 1, "不受信任的上报不能放行");
        let gates = check_gates(
            &specs,
            &[],
            &[run("failure", true, 1), run("success", true, 2)],
            oid,
            &paths,
        );
        assert!(failing_required(&gates).is_empty());
        let docs = vec!["docs/a.md".to_string()];
        assert!(failing_required(&check_gates(&specs, &[], &[], oid, &docs)).is_empty());
        assert_eq!(
            failing_required(&check_gates(&specs, &["lint".into()], &[], oid, &docs)).len(),
            1
        );
    }

    #[test]
    fn rule_review_flags_boundary_secret_conflict_and_limits() {
        let policy = policy();
        let manifest = policy.manifest_for("bot", false);
        let files = vec![
            diff_bytes(
                "src/shop/item.rs",
                Some(b"a\n"),
                Some(b"a\nlet key = \"AKIAABCDEFGHIJKLMNOP\";\n<<<<<<< HEAD\n"),
            ),
            diff_bytes("src/pay/mod.rs", Some(b"x\n"), Some(b"y\n")),
        ];
        let claims = vec![Claim {
            id: "c1".into(),
            user: "carol".into(),
            paths: vec!["src/pay/**".into()],
            intent: "重构支付".into(),
            branch: None,
            created_at: 0,
            expires_at: 100,
        }];
        let input = ReviewInput {
            title: "t",
            body: "",
            author: "bot",
            manifest: &manifest,
            policy: &policy,
            files: &files,
            claims: &claims,
            plan: "",
            now: 10,
        };
        let (verdict, _summary, findings) = rule_review(&input);
        assert_eq!(verdict, "request_changes");
        let rules: Vec<&str> = findings.iter().map(|f| f.rule.as_str()).collect();
        for rule in [
            "secret",
            "conflict",
            "boundary",
            "limits",
            "coordinate",
            "claim",
        ] {
            assert!(rules.contains(&rule), "缺少 {rule}: {rules:?}");
        }
        let secret = findings.iter().find(|f| f.rule == "secret").unwrap();
        assert_eq!(secret.line, Some(2));
    }

    #[test]
    fn llm_output_and_overlap_parse() {
        let (verdict, summary, findings) = parse_llm_review(
            "```json\n{\"verdict\":\"comment\",\"summary\":\"ok\",\"findings\":[{\"severity\":\"warning\",\"path\":\"a.rs\",\"line\":3,\"message\":\"m\"}]}\n```",
        )
        .unwrap();
        assert_eq!(
            (verdict.as_str(), summary.as_str(), findings.len()),
            ("comment", "ok", 1)
        );
        assert!(patterns_overlap("src/shop/**", "src/shop/item.rs"));
        assert!(patterns_overlap("src/**", "src/pay/**"));
        assert!(!patterns_overlap("src/shop/**", "src/pay/**"));
        assert!(!patterns_overlap("a.rs", "b.rs"));
    }
}
