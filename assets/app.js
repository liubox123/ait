const app = document.querySelector("#app");
let token = localStorage.getItem("ait_token") || "";
let me = null;

function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[ch]));
}

function fmtTime(seconds) {
  if (!seconds) return "";
  return new Date(seconds * 1000).toLocaleString();
}

async function api(path, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (token) headers.Authorization = "Bearer " + token;
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  const response = await fetch(path, { ...options, headers, body: options.body !== undefined ? JSON.stringify(options.body) : undefined });
  if (response.status === 204) return null;
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || response.statusText);
  return data;
}

function route() {
  const parts = (location.hash.replace(/^#/, "") || "/").split("/").filter(Boolean);
  if (!token) return renderAuth();
  if (parts[0] === "repo" && parts[1]) return renderRepo(parts[1], parts[2] || "code", parts.slice(3).join("/"));
  return renderHome();
}

function renderAuth() {
  app.innerHTML = `
    <div class="auth">
      <section class="auth-copy">
        <h1>ait</h1>
        <p>给人和 agent 共用的 Git。仓库仍然是 Git 格式；谁能直接改哪些目录，由服务端按项目规划决定。越权修改不会直接进入分支，而是变成一次评审。</p>
      </section>
      <section class="auth-card">
        <form class="card" id="auth-form">
          <h2 id="auth-title">登录</h2>
          <label>用户名</label>
          <input name="name" autocomplete="username" required>
          <label>密码</label>
          <input name="password" type="password" autocomplete="current-password" required>
          <div class="row" style="margin-top:16px">
            <button class="btn" type="submit">继续</button>
            <button class="linkish" type="button" id="toggle-mode">没有账号？注册</button>
          </div>
          <div class="error" id="auth-error"></div>
        </form>
      </section>
    </div>`;
  let mode = "login";
  document.querySelector("#toggle-mode").onclick = () => {
    mode = mode === "login" ? "register" : "login";
    document.querySelector("#auth-title").textContent = mode === "login" ? "登录" : "注册";
    document.querySelector("#toggle-mode").textContent = mode === "login" ? "没有账号？注册" : "已有账号？登录";
  };
  document.querySelector("#auth-form").onsubmit = async (event) => {
    event.preventDefault();
    const form = new FormData(event.target);
    try {
      const session = await api("/api/" + mode, { method: "POST", body: { name: form.get("name"), password: form.get("password") } });
      token = session.token;
      me = session;
      localStorage.setItem("ait_token", token);
      location.hash = "#/";
      route();
    } catch (error) {
      document.querySelector("#auth-error").textContent = error.message;
    }
  };
}

function shell(title, body, active) {
  const repos = window.__repos || [];
  app.innerHTML = `
    <div class="shell">
      <aside class="side">
        <div class="brand"><span>ait</span> 仓库</div>
        <a class="nav-item ${active === "home" ? "active" : ""}" href="#/">全部仓库</a>
        <div class="grow">
          ${repos.map((repo) => `<a class="nav-item ${active === repo.name ? "active" : ""}" href="#/repo/${esc(repo.name)}"><strong>${esc(repo.name)}</strong><small>${esc(repo.subject || "还没有提交")}</small></a>`).join("")}
        </div>
        <div class="me">${esc(me?.user || "")}${me?.admin ? " · 管理员" : ""}<br><button class="linkish" id="logout">退出</button></div>
      </aside>
      <section class="main">
        <div class="top"><h1>${esc(title)}</h1></div>
        ${body}
      </section>
    </div>`;
  document.querySelector("#logout").onclick = async () => {
    await api("/api/logout", { method: "POST" }).catch(() => {});
    token = "";
    localStorage.removeItem("ait_token");
    location.hash = "#/";
    renderAuth();
  };
}

async function renderHome() {
  const repos = await api("/api/repos");
  window.__repos = repos;
  shell("仓库", `<div class="content"><div class="home-grid">
    <div class="panel"><h3>仓库</h3>${repos.length ? repos.map((repo) => `<a class="commit-row" href="#/repo/${esc(repo.name)}"><span>${esc(repo.name)}</span><span class="meta">${esc(repo.owner)} · ${esc(repo.subject || "空仓库")}</span></a>`).join("") : `<div class="empty">还没有仓库</div>`}</div>
    <form class="card" id="create-repo">
      <h2>新建仓库</h2>
      <label>名称</label><input name="name" required placeholder="shop">
      <label>说明</label><input name="description" placeholder="可选">
      <div class="row" style="margin-top:16px"><button class="btn" type="submit">创建</button></div>
      <div class="error" id="create-error"></div>
    </form>
  </div></div>`, "home");
  document.querySelector("#create-repo").onsubmit = async (event) => {
    event.preventDefault();
    const form = new FormData(event.target);
    try {
      const repo = await api("/api/repos", { method: "POST", body: { name: form.get("name"), description: form.get("description") } });
      location.hash = "#/repo/" + repo.name;
    } catch (error) {
      document.querySelector("#create-error").textContent = error.message;
    }
  };
}

function tabs(name, tab) {
  const items = [["code", "代码"], ["commits", "提交"], ["pulls", "合并请求"], ["reviews", "评审"], ["access", "权限"], ["plan", "规划"]];
  return `<nav class="tabs">${items.map(([id, label]) => `<a class="${tab === id ? "active" : ""}" href="#/repo/${esc(name)}/${id}">${label}</a>`).join("")}</nav>`;
}

async function renderRepo(name, tab, path) {
  if (!window.__repos) window.__repos = await api("/api/repos").catch(() => []);
  const summary = await api("/api/repos/" + name);
  const manifest = await api("/api/repos/" + name + "/manifest");
  const banner = `<div class="banner"><strong>${esc(manifest.user)}</strong> · ${esc(manifest.duty)}<br>可直接修改：${esc(manifest.write.join("，") || "无")}。其他路径推送时需要评审。克隆地址 <span class="mono">${esc(summary.clone_url)}</span></div>`;
  let inner = "";
  if (tab === "commits") inner = await commitsView(name);
  else if (tab === "pulls") inner = await pullsView(name);
  else if (tab === "reviews") inner = await reviewsView(name);
  else if (tab === "access") inner = await accessView(name);
  else if (tab === "plan") inner = await planView(name);
  else inner = await codeView(name, path);
  shell(summary.name, `${tabs(name, tab)}<div class="content">${banner}${inner}</div>`, name);
  bindRepo(name, tab);
}

async function codeView(name, path) {
  let directory = path;
  let file = null;
  let entries = [];
  try {
    entries = await api(`/api/repos/${name}/tree?path=${encodeURIComponent(path)}`);
  } catch (error) {
    if (!path) return `<div class="empty">${esc(error.message)}</div>`;
    const slash = path.lastIndexOf("/");
    directory = slash >= 0 ? path.slice(0, slash) : "";
    file = path;
    entries = await api(`/api/repos/${name}/tree?path=${encodeURIComponent(directory)}`);
  }
  let blob = "";
  if (file) {
    const data = await api(`/api/repos/${name}/blob?path=${encodeURIComponent(file)}`);
    blob = data.binary ? `<div class="empty">二进制文件，${data.size} 字节</div>` : `<pre class="blob">${esc(data.text)}</pre>`;
  } else if (!entries.length) {
    blob = `<div class="empty">这个目录还是空的。用 ait clone 之后提交，推送后会出现在这里。</div>`;
  }
  const crumbs = ["<a href='#/repo/" + esc(name) + "/code'>根目录</a>"];
  let walked = "";
  for (const part of directory.split("/").filter(Boolean)) {
    walked = walked ? walked + "/" + part : part;
    crumbs.push(`<a href="#/repo/${esc(name)}/code/${esc(walked)}">${esc(part)}</a>`);
  }
  const rows = entries.map((entry) => {
    const href = `#/repo/${esc(name)}/code/${esc(entry.path)}`;
    return `<a class="file-row" href="${href}"><span class="kind">${entry.kind === "dir" ? "目录" : "文件"}</span><span>${esc(entry.name)}</span></a>`;
  }).join("");
  return `<div class="crumb">${crumbs.join(" / ")}</div><div class="split"><div class="panel"><h3>文件</h3>${rows || `<div class="empty">空目录</div>`}</div><div class="panel"><h3>${esc(file || "选择一个文件")}</h3>${blob}</div></div>`;
}

async function commitsView(name) {
  const commits = await api("/api/repos/" + name + "/commits?limit=50");
  if (!commits.length) return `<div class="empty">还没有提交</div>`;
  window.__commits = commits;
  return `<div class="panel">${commits.map((commit, index) => `<button class="commit-row" data-index="${index}"><span class="oid">${esc(commit.oid.slice(0, 7))}</span><span>${esc(commit.summary)}</span><span class="meta">${esc(commit.author_name)} · ${esc(fmtTime(commit.time))}</span></button>`).join("")}</div><pre class="diff" id="commit-diff"></pre>`;
}

async function pullsView(name) {
  const pulls = await api("/api/repos/" + name + "/pulls");
  if (!pulls.length) return `<div class="empty">还没有合并请求。推送分支后用 ait pr create 提交。</div>`;
  return pulls.map((item) => `<article class="review">
      <header>
        <div><strong>${esc(item.id)}</strong> <span class="pill ${esc(item.status)}">${esc(item.status)}</span>
          <div class="meta">${esc(item.author)} · ${esc(item.head_ref.replace(/^refs\/heads\//, ""))} → ${esc(item.base_ref.replace(/^refs\/heads\//, ""))} · ${esc(fmtTime(item.created_at))}</div>
          <div>${esc(item.title)}</div>
          <div>越权路径：${esc((item.blocked || []).join(", ") || "无")}</div>
        </div>
        <div class="row">
          ${item.can_approve ? `<button class="btn" data-pr-approve="${esc(item.id)}">通过</button>` : ""}
          ${item.can_merge ? `<button class="btn" data-pr-merge="${esc(item.id)}">合并</button>` : ""}
          ${item.can_close ? `<button class="btn danger" data-pr-close="${esc(item.id)}">关闭</button>` : ""}
        </div>
      </header>
      <div class="diff">${diffHtml(item.diff || "")}</div>
    </article>`).join("");
}

async function reviewsView(name) {
  const reviews = await api("/api/repos/" + name + "/reviews");
  if (!reviews.length) return `<div class="empty">没有评审。越权推送会出现在这里。</div>`;
  return reviews.map((item) => {
    const review = item;
    return `<article class="review">
      <header>
        <div><strong>${esc(review.id)}</strong> <span class="pill ${esc(review.status)}">${esc(review.status)}</span><div class="meta">${esc(review.author)} → ${esc(review.reference)} · ${esc(fmtTime(review.created_at))}</div>
        <div>越权路径：${esc((review.blocked || []).join(", ") || "无")}</div></div>
        ${review.can_approve ? `<div class="row"><button class="btn" data-approve="${esc(review.id)}">通过</button><button class="btn danger" data-reject="${esc(review.id)}">拒绝</button></div>` : ""}
      </header>
      <div class="diff">${diffHtml(review.diff || "")}</div>
    </article>`;
  }).join("");
}

function capturePolicyInputs() {
  const inputs = [...document.querySelectorAll('[data-kind="user"]')];
  if (!window.__policy || !inputs.length) return;
  window.__policy.rules = inputs.map((input) => {
    const index = input.dataset.index;
    return {
      user: input.value,
      pattern: document.querySelector(`[data-kind="pattern"][data-index="${index}"]`)?.value || "",
      access: document.querySelector(`[data-kind="access"][data-index="${index}"]`)?.value || "write",
    };
  });
}

function accessHtml(policy) {
  const rows = policy.rules.map((rule, index) => `<tr><td><input data-kind="user" data-index="${index}" value="${esc(rule.user)}"></td><td><input data-kind="pattern" data-index="${index}" value="${esc(rule.pattern)}"></td><td><select data-kind="access" data-index="${index}"><option ${rule.access === "write" ? "selected" : ""}>write</option><option ${rule.access === "deny" ? "selected" : ""}>deny</option></select></td><td><button class="linkish" data-remove="${index}">删除</button></td></tr>`).join("");
  const duties = policy.duties.map((duty) => `<tr><td>${esc(duty.user)}</td><td>${esc(duty.summary)}</td><td class="mono">${esc(duty.paths.join(", "))}</td></tr>`).join("");
  return `<div class="panel"><h3>职责</h3><table><tr><th>用户</th><th>职责</th><th>目录</th></tr>${duties}</table></div>
    <div class="panel" style="margin-top:16px"><h3>路径规则</h3><table><tr><th>用户</th><th>路径</th><th>权限</th><th></th></tr>${rows}</table>
    <div class="row" style="padding:12px"><button class="btn secondary" id="add-rule" type="button">添加规则</button><button class="btn" id="save-policy" type="button">保存权限</button><span class="error" id="policy-error"></span></div></div>
    <p class="meta">所有者 ${esc(policy.owners.join(", "))} 始终可以修改整个仓库。write 可以直接推送，deny 会被拒绝，其余路径进入评审。</p>`;
}

function redrawAccess(name) {
  const banner = document.querySelector(".banner");
  document.querySelector(".content").innerHTML = (banner ? banner.outerHTML : "") + accessHtml(window.__policy);
  bindRepo(name, "access");
}

async function accessView(name) {
  window.__policy = await api("/api/repos/" + name + "/policy");
  return accessHtml(window.__policy);
}

async function planView(name) {
  const plan = await api("/api/repos/" + name + "/plan");
  const sample = plan.content || `# 商城\n\n## 账户 (@alice)\n职责: 登录、注册与会话\n目录: src/account/**, src/session/**\n\n## 商品 (@bob)\n职责: 商品与库存\n目录: src/catalog/**\n`;
  return `<form id="plan-form">
    <label>项目规划。服务端据此划分目录和职责。</label>
    <textarea name="content">${esc(sample)}</textarea>
    <div class="row" style="margin-top:12px">
      <label class="row"><input type="checkbox" name="llm" style="width:auto"> 调用模型（需配置 AIT_LLM_BASE_URL）</label>
      <button class="btn secondary" name="mode" value="save" type="submit">只保存</button>
      <button class="btn" name="mode" value="apply" type="submit">解析并应用</button>
    </div>
    <div class="error" id="plan-error"></div>
    <pre class="diff" id="plan-result"></pre>
  </form>`;
}

function diffHtml(text) {
  return text.split("\n").map((line) => {
    const cls = line.startsWith("+") && !line.startsWith("+++") ? "add" : line.startsWith("-") && !line.startsWith("---") ? "del" : "";
    return `<div class="${cls}">${esc(line) || " "}</div>`;
  }).join("");
}

function bindRepo(name, tab) {
  document.querySelectorAll("[data-index]").forEach((button) => {
    if (!button.dataset.index || button.dataset.kind) return;
    button.onclick = async () => {
      const commit = window.__commits[Number(button.dataset.index)];
      const from = commit.parents[0] || "";
      const data = await api(`/api/repos/${name}/diff?to=${commit.oid}&from=${from}`);
      document.querySelector("#commit-diff").innerHTML = diffHtml(data.diff || "没有差异");
    };
  });
  document.querySelectorAll("[data-pr-approve]").forEach((button) => {
    button.onclick = async () => {
      await api(`/api/repos/${name}/pulls/${button.getAttribute("data-pr-approve")}/approve`, { method: "POST" });
      route();
    };
  });
  document.querySelectorAll("[data-pr-merge]").forEach((button) => {
    button.onclick = async () => {
      await api(`/api/repos/${name}/pulls/${button.getAttribute("data-pr-merge")}/merge`, { method: "POST" });
      route();
    };
  });
  document.querySelectorAll("[data-pr-close]").forEach((button) => {
    button.onclick = async () => {
      await api(`/api/repos/${name}/pulls/${button.getAttribute("data-pr-close")}/close`, { method: "POST" });
      route();
    };
  });
  document.querySelectorAll("[data-approve]").forEach((button) => {
    button.onclick = async () => {
      await api(`/api/repos/${name}/reviews/${button.dataset.approve}/approve`, { method: "POST" });
      route();
    };
  });
  document.querySelectorAll("[data-reject]").forEach((button) => {
    button.onclick = async () => {
      const reason = prompt("拒绝原因", "") || "";
      await api(`/api/repos/${name}/reviews/${button.dataset.reject}/reject`, { method: "POST", body: { reason } });
      route();
    };
  });
  const save = document.querySelector("#save-policy");
  if (save) {
    document.querySelector("#add-rule").onclick = () => {
      capturePolicyInputs();
      window.__policy.rules.push({ user: "", pattern: "", access: "write" });
      redrawAccess(name);
    };
    document.querySelectorAll("[data-remove]").forEach((button) => {
      button.onclick = () => {
        capturePolicyInputs();
        window.__policy.rules.splice(Number(button.dataset.remove), 1);
        redrawAccess(name);
      };
    });
    save.onclick = async () => {
      const rules = window.__policy.rules.map((rule, index) => ({
        user: document.querySelector(`[data-kind="user"][data-index="${index}"]`).value.trim(),
        pattern: document.querySelector(`[data-kind="pattern"][data-index="${index}"]`).value.trim(),
        access: document.querySelector(`[data-kind="access"][data-index="${index}"]`).value,
      })).filter((rule) => rule.user && rule.pattern);
      try {
        await api(`/api/repos/${name}/policy`, { method: "PUT", body: { rules, duties: window.__policy.duties } });
        route();
      } catch (error) {
        document.querySelector("#policy-error").textContent = error.message;
      }
    };
  }
  const plan = document.querySelector("#plan-form");
  if (plan) {
    plan.onsubmit = async (event) => {
      event.preventDefault();
      const submitter = event.submitter;
      const form = new FormData(plan);
      try {
        const result = await api(`/api/repos/${name}/plan`, {
          method: "PUT",
          body: { content: form.get("content"), apply: submitter?.value === "apply", use_llm: form.get("llm") === "on" },
        });
        const lines = (result.assignments || []).map((item) => `${item.user}：${item.duty}\n  ${item.paths.join("\n  ")}`);
        document.querySelector("#plan-result").textContent = (result.applied ? "已应用到权限\n" : "已保存，尚未应用\n") + lines.join("\n");
      } catch (error) {
        document.querySelector("#plan-error").textContent = error.message;
      }
    };
  }
}

window.addEventListener("hashchange", () => { if (token) route(); });
(async function start() {
  if (!token) return renderAuth();
  try {
    me = await api("/api/me");
    route();
  } catch {
    token = "";
    localStorage.removeItem("ait_token");
    renderAuth();
  }
})();
