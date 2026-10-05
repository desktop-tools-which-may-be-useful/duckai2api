/* duckai2api-rust 控制台（无构建链，axios + highlight.js + tailwindcss CDN） */
/* global axios, hljs */
"use strict";

const $ = (s) => document.querySelector(s);
const api = axios.create({ baseURL: "/admin/api", timeout: 10000 });

let STATE = { models: [], settings: null, proto: "openai" };

// ------------------------------------------------------------------ 提示
function toast(msg, ok = true) {
  const el = $("#toast");
  el.textContent = msg;
  el.className =
    "fixed bottom-4 right-4 px-4 py-2 rounded-lg text-sm shadow-lg " +
    (ok ? "bg-emerald-700 text-white" : "bg-red-700 text-white");
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.add("hidden"), 3200);
}

function showLogin() {
  $("#login-overlay").classList.remove("hidden");
  $("#login-overlay").classList.add("flex");
}
function hideLogin() {
  $("#login-overlay").classList.add("hidden");
  $("#login-overlay").classList.remove("flex");
}

api.interceptors.response.use(
  (r) => r,
  (err) => {
    const st = err.response && err.response.status;
    const msg =
      (err.response && err.response.data && err.response.data.error) ||
      err.message;
    if (st === 401) showLogin();
    else toast(msg, false);
    return Promise.reject(err);
  }
);

// ------------------------------------------------------------------ 导航
function goto(page) {
  document.querySelectorAll(".page").forEach((p) => p.classList.add("hidden"));
  const el = $("#page-" + page);
  if (el) el.classList.remove("hidden");
  document.querySelectorAll(".nav-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset.page === page);
  });
  location.hash = page;
  if (page === "logs") loadLogs();
  if (page === "dashboard") loadStatus();
  if (page === "keys") loadKeys();
}

$("#nav").addEventListener("click", (e) => {
  const b = e.target.closest("[data-page]");
  if (b) goto(b.dataset.page);
});

// ------------------------------------------------------------------ 概览
function renderHealth(h, settings) {
  $("#h-status").textContent = h.status || "—";
  $("#h-mode").textContent = (h.upstream && h.upstream.mode) || "—";
  $("#h-vqd").textContent = h.upstream && h.upstream.vqd_valid ? "是" : "否";
  $("#h-inflight").textContent = h.inflight;
  $("#h-egress").textContent =
    h.egress.healthy + " / " + h.egress.total;
  $("#h-banned").textContent = h.egress.banned;
  $("#h-fe").textContent =
    h.upstream.fe_version_age === undefined || h.upstream.fe_version_age === null
      ? "从未抓取"
      : h.upstream.fe_version_age;
  $("#h-model").textContent = (settings && settings.default_model) || "—";
}

// ------------------------------------------------------------------ 模型
function renderModels(models) {
  STATE.models = models;
  const body = $("#models-body");
  body.innerHTML = "";
  models.forEach((m) => {
    const tr = document.createElement("tr");
    const aliases = (m.aliases || []).join(", ") || "—";
    tr.innerHTML =
      "<td class='font-mono'>" + esc(m.id) + "</td>" +
      "<td>" + esc(m.owned_by) + "</td>" +
      "<td class='text-slate-400'>" + esc(aliases) + "</td>" +
      "<td><button class='btn' data-set-model='" + esc(m.id) + "'>设为默认</button></td>";
    body.appendChild(tr);
  });
  fillModelSelects();
}

$("#models-body").addEventListener("click", async (e) => {
  const id = e.target.dataset && e.target.dataset.setModel;
  if (!id) return;
  try {
    await api.post("/settings", { default_model: id });
    toast("默认模型已切换为 " + id);
    loadStatus();
  } catch (_) {}
});

function fillModelSelects() {
  ["#api-model", "#set-model"].forEach((sel) => {
    const el = $(sel);
    const keep = el.value;
    el.innerHTML = "";
    STATE.models.forEach((m) => {
      const o = document.createElement("option");
      o.value = m.id;
      o.textContent = m.id;
      el.appendChild(o);
    });
    const want =
      (STATE.settings && STATE.settings.default_model) || keep || "";
    if (want) el.value = want;
    renderExamples();
  });
}

// ------------------------------------------------------------------ 出口
function renderEgress(rows) {
  const body = $("#egress-body");
  body.innerHTML = "";
  if (!rows.length) {
    body.innerHTML = "<tr><td colspan='10' class='text-slate-500'>（无出口记录）</td></tr>";
    return;
  }
  rows.forEach((r) => {
    const tr = document.createElement("tr");
    const color =
      r.state === "Healthy" ? "text-emerald-400"
      : r.state === "Banned" ? "text-red-400"
      : r.state === "Cooldown" ? "text-amber-400"
      : "text-sky-400";
    const sub =
      "<div class='text-xs text-slate-500'>" + esc(r.reason || "—") +
      " · " + new Date(r.since_ms).toLocaleTimeString() + "</div>";
    tr.innerHTML =
      "<td>" + r.index + "</td>" +
      "<td class='font-mono'>" + esc(r.label) + sub + "</td>" +
      "<td><input type='checkbox' data-en='" + r.index + "' class='accent-blue-600' title='启用出口（参与分发/探测）'" +
        (r.enabled ? " checked" : "") + "></td>" +
      "<td class='" + color + "'>" + esc(r.state) + "</td>" +
      "<td>" + r.score + "/" + r.inflight + "</td>" +
      "<td><input type='checkbox' data-cd='" + r.index + "' class='accent-blue-600' title='失败自动冷却开关'" +
        (r.cooldown_enabled ? " checked" : "") + "></td>" +
      "<td><input type='number' min='1' data-f='ban_secs' data-i='" + r.index + "' value='" + r.ban_secs + "' class='inp w-16 px-1 py-0.5' title='首次封禁秒数'></td>" +
      "<td><input type='number' min='1' data-f='ban_cap_secs' data-i='" + r.index + "' value='" + r.ban_cap_secs + "' class='inp w-16 px-1 py-0.5' title='封禁秒数上限'></td>" +
      "<td><input type='number' min='1' data-f='rate_limit_secs' data-i='" + r.index + "' value='" + r.rate_limit_secs + "' class='inp w-16 px-1 py-0.5' title='429 无 Retry-After 的起始退避'></td>" +
      "<td class='space-x-1 whitespace-nowrap'>" +
        "<button class='btn btn-primary' data-savep='" + r.index + "'>保存</button> " +
        "<button class='btn btn-danger' data-ban='" + r.index + "'>封禁</button> " +
        "<button class='btn' data-unban='" + r.index + "'>解封</button>" +
      "</td>";
    body.appendChild(tr);
  });
  renderDirectToggle(rows);
}

function renderDirectToggle(rows) {
  const d = rows.find((r) => r.direct);
  const btn = $("#direct-toggle");
  btn._enabled = !!(d && d.enabled);
  btn.textContent = !d
    ? "直连：未配置（点击添加）"
    : d.enabled
      ? "直连：已启用（点击停用）"
      : "直连：已停用（点击启用）";
}

$("#direct-toggle").addEventListener("click", async () => {
  const enabled = !$("#direct-toggle")._enabled;
  try {
    const { data } = await api.post("/egress/direct", { enabled });
    toast(enabled ? "直连出口已启用" : "直连出口已停用");
    renderEgress(data.egresses);
  } catch (_) {}
});

$("#egress-body").addEventListener("click", async (e) => {
  const d = e.target.dataset || {};
  try {
    if (d.savep !== undefined) {
      const tr = e.target.closest("tr");
      const num = (f) => Number(tr.querySelector("[data-f='" + f + "']").value);
      const policy = {
        enabled: tr.querySelector("[data-en]").checked,
        cooldown_enabled: tr.querySelector("[data-cd]").checked,
        ban_secs: num("ban_secs"),
        ban_cap_secs: num("ban_cap_secs"),
        rate_limit_secs: num("rate_limit_secs"),
      };
      const { data } = await api.post("/egress/policy", {
        index: Number(d.savep),
        policy,
      });
      toast("出口 #" + d.savep + " 策略已保存");
      renderEgress(data.egresses);
      return;
    }
    if (d.ban !== undefined) {
      await api.post("/egress/ban", { index: Number(d.ban) });
      toast("已手动封禁出口 #" + d.ban);
    } else if (d.unban !== undefined) {
      await api.post("/egress/unban", { index: Number(d.unban) });
      toast("已解封出口 #" + d.unban);
    } else return;
    const { data } = await api.get("/egress");
    renderEgress(data.egresses);
  } catch (_) {}
});

$("#probe-btn").addEventListener("click", async () => {
  try {
    await api.post("/probe", {});
    toast("已触发全量探测（202）");
  } catch (_) {}
});
$("#egress-refresh").addEventListener("click", async () => {
  try {
    const { data } = await api.get("/egress");
    renderEgress(data.egresses);
  } catch (_) {}
});

// ------------------------------------------------------------------ 密钥
async function loadKeys() {
  try {
    const { data } = await api.get("/keys");
    renderKeys(data.keys);
  } catch (_) {}
}

function renderKeys(keys) {
  const body = $("#keys-body");
  body.innerHTML = "";
  if (!keys.length) {
    body.innerHTML = "<tr><td colspan='6' class='text-slate-500'>（无密钥，接口鉴权未启用）</td></tr>";
    return;
  }
  keys.forEach((k) => {
    const revoked = !!k.revoked_at;
    const tr = document.createElement("tr");
    tr.innerHTML =
      "<td>" + k.id + "</td>" +
      "<td>" + esc(k.label || "—") + "</td>" +
      "<td class='font-mono'>" + esc(k.prefix) + "…</td>" +
      "<td class='text-slate-500'>" + new Date(k.created_at * 1000).toLocaleString() + "</td>" +
      "<td class='" + (revoked ? "text-red-400" : "text-emerald-400") + "'>" +
        (revoked ? "已吊销" : "有效") + "</td>" +
      "<td>" + (revoked ? "—" :
        "<button class='btn btn-danger' data-revoke-key='" + k.id + "'>吊销</button>") + "</td>";
    body.appendChild(tr);
  });
}

$("#keys-body").addEventListener("click", async (e) => {
  const id = e.target.dataset && e.target.dataset.revokeKey;
  if (id === undefined) return;
  try {
    const { data } = await api.delete("/keys/" + encodeURIComponent(id));
    renderKeys(data.keys);
    toast("密钥 #" + id + " 已吊销");
  } catch (_) {}
});

$("#key-create").addEventListener("click", async () => {
  const label = $("#key-label").value.trim();
  try {
    const { data } = await api.post("/keys", { label: label || null });
    $("#key-label").value = "";
    $("#key-once-val").textContent = data.key;
    $("#key-once").classList.remove("hidden");
    renderKeys(data.keys);
    toast("密钥已创建，明文仅此一次可见");
  } catch (_) {}
});

$("#key-copy").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText($("#key-once-val").textContent);
    toast("已复制到剪贴板");
  } catch (_) {
    toast("复制失败，请手动选择", false);
  }
});

$("#keys-refresh").addEventListener("click", loadKeys);

// ------------------------------------------------------------------ 日志
async function loadLogs() {
  const n = $("#logs-lines").value;
  try {
    const { data } = await api.get("/logs?lines=" + encodeURIComponent(n));
    const body = $("#logs-body");
    body.innerHTML = "";
    if (!data.logs.length) {
      body.innerHTML = "<tr><td colspan='7' class='text-slate-500'>暂无日志</td></tr>";
      return;
    }
    data.logs.forEach((l) => {
      const tr = document.createElement("tr");
      const cls =
        l.status >= 500 ? "text-red-400" : l.status >= 400 ? "text-amber-400" : "text-emerald-400";
      tr.innerHTML =
        "<td class='text-slate-500'>" + new Date(l.ts_ms).toLocaleTimeString() + "</td>" +
        "<td>" + esc(l.protocol) + "</td>" +
        "<td class='font-mono'>" + esc(l.model) + "</td>" +
        "<td>" + l.duration_ms + "ms</td>" +
        "<td class='" + cls + "'>" + l.status + "</td>" +
        "<td>" + l.retries + "</td>" +
        "<td class='text-slate-400'>" + esc(l.error || (l.switch_reason ? l.switch_reason + " → 换端" : "—")) + "</td>";
      body.appendChild(tr);
    });
  } catch (_) {}
}
$("#logs-refresh").addEventListener("click", loadLogs);
$("#logs-lines").addEventListener("change", loadLogs);

// ------------------------------------------------------------------ 设置
function renderSettings(s) {
  STATE.settings = s;
  if (s.default_model) $("#set-model").value = s.default_model;
  $("#set-concurrency").value = s.max_concurrency;
  $("#set-base").value = s.base || "";
  $("#set-vqd").value = s.vqd_override || "";
  $("#set-newchat").checked = !!s.new_chat;
  $("#set-meta").textContent =
    s.bind + "  ·  API 鉴权" + (s.auth_enabled ? "已启用" : "未启用") +
    "  ·  管理口令" + (s.admin_password_set ? "已设置" : "未设置（只读）") +
    "  ·  上游 " + s.upstream_mode +
    "  ·  新会话 " + (s.new_chat ? "开" : "关");
  renderProxies(s.proxies || []);
}

function renderProxies(list) {
  const ul = $("#proxy-list");
  ul.innerHTML = "";
  if (!list.length) {
    ul.innerHTML = "<li class='text-slate-500'>（直连，无代理）</li>";
    return;
  }
  list.forEach((p) => {
    const li = document.createElement("li");
    li.className = "flex items-center justify-between bg-slate-900 rounded px-3 py-1";
    li.innerHTML = "<span class='font-mono'>" + esc(p) + "</span>" +
      "<button class='btn btn-danger' data-del-proxy='" + esc(p) + "'>删除</button>";
    ul.appendChild(li);
  });
}

$("#set-save").addEventListener("click", async () => {
  try {
    const body = {
      default_model: $("#set-model").value,
      max_concurrency: Number($("#set-concurrency").value),
    };
    const { data } = await api.post("/settings", body);
    renderSettings(data);
    toast("设置已保存");
  } catch (_) {}
});

$("#proxy-add").addEventListener("click", async () => {
  const url = $("#proxy-input").value.trim();
  if (!url) return toast("请输入代理 URL", false);
  try {
    const { data } = await api.post("/proxy", { url });
    $("#proxy-input").value = "";
    renderProxies(data.proxies);
    toast("代理已添加");
  } catch (_) {}
});

$("#proxy-list").addEventListener("click", async (e) => {
  const u = e.target.dataset && e.target.dataset.delProxy;
  if (!u) return;
  try {
    const { data } = await api.delete("/proxy", { params: { url: u } });
    renderProxies(data.proxies);
    toast("代理已删除");
  } catch (_) {}
});

$("#set-save2").addEventListener("click", async () => {
  try {
    const { data } = await api.post("/settings/save", {
      base: $("#set-base").value.trim(),
      vqd_override: $("#set-vqd").value.trim(),
      new_chat: $("#set-newchat").checked,
    });
    renderSettings(data.settings);
    $("#set-save-note").textContent = data.note;
    toast("存储设置已保存");
  } catch (_) {}
});

$("#pw-save").addEventListener("click", async () => {
  const nw = $("#pw-new").value;
  if (nw.length < 8) return toast("新口令至少 8 位", false);
  try {
    const { data } = await api.post("/password", {
      old: $("#pw-old").value,
      new: nw,
    });
    $("#pw-old").value = "";
    $("#pw-new").value = "";
    renderSettings(data.settings);
    toast("管理口令已修改，旧口令立即失效");
  } catch (_) {}
});

// ------------------------------------------------------------------ API 示例
const tabs = document.querySelectorAll("#proto-tabs [data-proto]");
tabs.forEach((b) =>
  b.addEventListener("click", () => {
    STATE.proto = b.dataset.proto;
    tabs.forEach((x) => x.classList.toggle("active-tab", x === b));
    renderExamples();
  })
);

function base() {
  return location.origin;
}

function snippets() {
  const m = $("#api-model").value || "gpt-5.6-luna";
  const key = $("#api-key").value.trim();
  const auth = key ? ` \\\n  -H "Authorization: Bearer ${key}"` : "";
  const origin = base();
  if (STATE.proto === "anthropic") {
    return {
      plain:
`curl ${origin}/v1/messages${auth} \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m}",
    "max_tokens": 512,
    "messages": [
      { "role": "user", "content": "用一句话介绍 Rust" }
    ]
  }'`,
      stream:
`curl -N ${origin}/v1/messages${auth} \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m}",
    "max_tokens": 512,
    "stream": true,
    "messages": [
      { "role": "user", "content": "用一句话介绍 Rust" }
    ]
  }'`,
    };
  }
  if (STATE.proto === "responses") {
    return {
      plain:
`curl ${origin}/v1/responses${auth} \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m}",
    "input": "用一句话介绍 Rust"
  }'`,
      stream:
`curl -N ${origin}/v1/responses${auth} \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m}",
    "stream": true,
    "input": "用一句话介绍 Rust"
  }'`,
    };
  }
  return {
    plain:
`curl ${origin}/v1/chat/completions${auth} \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m}",
    "messages": [
      { "role": "user", "content": "用一句话介绍 Rust" }
    ]
  }'`,
    stream:
`curl -N ${origin}/v1/chat/completions${auth} \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${m}",
    "stream": true,
    "messages": [
      { "role": "user", "content": "用一句话介绍 Rust" }
    ]
  }'`,
  };
}

function putCode(sel, text, lang) {
  const el = $(sel);
  el.removeAttribute("data-highlighted");
  el.className = "language-" + lang;
  el.textContent = text;
  if (window.hljs) hljs.highlightElement(el);
}

function renderExamples() {
  if (!$("#ex-plain")) return;
  const s = snippets();
  putCode("#ex-plain", s.plain, "bash");
  putCode("#ex-stream", s.stream, "bash");
}

$("#api-model").addEventListener("change", renderExamples);
$("#api-key").addEventListener("input", renderExamples);

function requestBody() {
  const m = $("#api-model").value || "gpt-5.6-luna";
  if (STATE.proto === "anthropic")
    return {
      model: m,
      max_tokens: 512,
      messages: [{ role: "user", content: "用一句话介绍 Rust" }],
    };
  if (STATE.proto === "responses")
    return { model: m, input: "用一句话介绍 Rust" };
  return {
    model: m,
    messages: [{ role: "user", content: "用一句话介绍 Rust" }],
  };
}

function endpoint() {
  if (STATE.proto === "anthropic") return "/v1/messages";
  if (STATE.proto === "responses") return "/v1/responses";
  return "/v1/chat/completions";
}

$("#send-test").addEventListener("click", async () => {
  const key = $("#api-key").value.trim();
  const headers = { "Content-Type": "application/json" };
  if (key) headers["Authorization"] = "Bearer " + key;
  $("#test-status").textContent = "请求中…";
  try {
    const resp = await axios.post(base() + endpoint(), requestBody(), {
      headers,
      timeout: 30000,
    });
    $("#test-status").textContent = "HTTP " + resp.status;
    putCode("#ex-result", JSON.stringify(resp.data, null, 2), "json");
    toast("测试请求成功");
  } catch (err) {
    const st = err.response ? err.response.status : "网络错误";
    const data = err.response ? err.response.data : String(err);
    $("#test-status").textContent = "HTTP " + st;
    putCode("#ex-result", JSON.stringify(data, null, 2), "json");
    toast("测试请求失败：" + st, false);
  }
});

// ------------------------------------------------------------------ 登录
$("#login-btn").addEventListener("click", async () => {
  try {
    await api.post("/login", { password: $("#login-pass").value });
    $("#login-pass").value = "";
    $("#login-err").classList.add("hidden");
    hideLogin();
    $("#logout-btn").classList.remove("hidden");
    toast("登录成功");
    boot();
  } catch (err) {
    const el = $("#login-err");
    el.textContent =
      (err.response && err.response.data && err.response.data.error) || "登录失败";
    el.classList.remove("hidden");
  }
});
$("#login-pass").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("#login-btn").click();
});
$("#logout-btn").addEventListener("click", async () => {
  try {
    await api.post("/logout", {});
  } catch (_) {}
  location.reload();
});

// ------------------------------------------------------------------ 数据装载
async function loadStatus() {
  try {
    const { data } = await api.get("/status");
    renderHealth(data.health, data.settings);
    renderSettings(data.settings);
    renderModels(data.models);
    renderEgress(data.egresses);
  } catch (_) {}
}

function esc(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

async function boot() {
  const page = (location.hash || "#dashboard").slice(1);
  const pages = ["dashboard", "models", "egress", "keys", "logs", "settings", "api"];
  goto(pages.includes(page) ? page : "dashboard");
  await loadStatus();
  renderExamples();
  setInterval(() => {
    if (location.hash === "" || location.hash === "#dashboard") loadStatus();
  }, 6000);
}

$("#models-refresh").addEventListener("click", loadStatus);
boot();
