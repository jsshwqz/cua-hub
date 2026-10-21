//! Local web console: live screen preview, cursor/screen info and background
//! job control. Bound to loopback by default; if `CUA_HUB_MCP_TOKEN` is set the
//! same bearer token is required (query `?token=` or Authorization header).

use std::collections::HashMap;

use anyhow::{bail, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

#[derive(Clone)]
struct ConsoleState {
    token: Option<String>,
}

pub async fn serve(bind: &str) -> Result<()> {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let local = matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1");
    let token = std::env::var("CUA_HUB_MCP_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    if !local && token.is_none() {
        bail!("refusing to bind console on {host} without CUA_HUB_MCP_TOKEN");
    }

    let state = ConsoleState { token };
    let app = Router::new()
        .route("/", get(index))
        .route("/state", get(state_handler))
        .route("/frame.jpg", get(frame))
        .route("/run", post(run_task))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/mcp", post(mcp_rpc))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {bind}: {e}"))?;
    eprintln!("console: http://{bind}/?token=<your-token-if-set>");
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("console server: {e}"))?;
    Ok(())
}

fn authorized(state: &ConsoleState, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {expected}"))
        .unwrap_or(false);
    let from_query = query.get("token").map(|v| v == expected).unwrap_or(false);
    bearer || from_query
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn state_handler(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&state, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let screen = cua_io::capture_primary().ok().map(|c| {
        json!({
            "width": c.width,
            "height": c.height,
            "origin": { "x": c.origin.0, "y": c.origin.1 },
            "scale_factor": c.scale_factor,
        })
    });
    let cursor = cua_io::cursor_position()
        .ok()
        .map(|(x, y)| json!({ "x": x, "y": y }));
    Json(json!({
        "screen": screen,
        "dpi": cua_io::dpi_scale(),
        "cursor": cursor,
        "jobs": crate::jobs::snapshot(),
    }))
    .into_response()
}

async fn run_task(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if !authorized(&state, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    match crate::mcp::run_options_from_args(&body) {
        Ok(opts) => {
            let id = crate::jobs::start(opts);
            Json(json!({ "job_id": id, "status": "running" })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(e)).into_response(),
    }
}

/// MCP endpoint hosted by the console process, so agent-started jobs show up
/// in the same UI (jobs are in-process state).
async fn mcp_rpc(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(request): Json<serde_json::Value>,
) -> Response {
    if !authorized(&state, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    match crate::mcp::handle_request(request).await {
        Some(response) => Json(response).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn frame(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&state, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    match cua_io::observe(1280, 70) {
        Ok(frame) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            frame.observation.image,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn cancel(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path(id): Path<String>,
) -> Response {
    if !authorized(&state, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    match crate::jobs::cancel(&id) {
        Some(status) => Json(json!({ "job_id": id, "status": status })).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown job").into_response(),
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>cua-hub console</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.45 system-ui, sans-serif; background: #0d1117; color: #c9d1d9; }
  header { display: flex; gap: 16px; align-items: baseline; padding: 12px 18px; border-bottom: 1px solid #30363d; }
  header h1 { font-size: 16px; margin: 0; color: #58a6ff; }
  header span { color: #8b949e; }
  main { display: grid; grid-template-columns: minmax(0, 1fr) 360px; gap: 16px; padding: 16px; }
  .screen { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 8px; }
  .screen img { width: 100%; display: block; border-radius: 4px; background: #000; }
  aside { display: flex; flex-direction: column; gap: 12px; }
  section { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 12px; }
  h2 { font-size: 13px; margin: 0 0 8px; color: #8b949e; text-transform: uppercase; letter-spacing: .04em; }
  .job { border-top: 1px solid #30363d; padding: 8px 0; }
  .job:first-of-type { border-top: 0; }
  .job .task { color: #e6edf3; }
  .job .meta { color: #8b949e; font-size: 12px; }
  .job button { margin-top: 6px; background: #21262d; color: #f85149; border: 1px solid #30363d; border-radius: 4px; padding: 3px 8px; cursor: pointer; }
  .job button:disabled { color: #8b949e; cursor: default; }
  code { color: #7ee787; }
</style>
</head>
<body>
<header><h1>cua-hub console</h1><span id="screen">屏幕信息加载中…</span><span id="cursor"></span></header>
<main>
  <div class="screen"><img id="frame" alt="live screen"></div>
  <aside>
    <section><h2>启动任务</h2>
      <textarea id="taskText" rows="3" placeholder="例如：打开记事本输入 hello" style="width:100%;background:#0d1117;color:#c9d1d9;border:1px solid #30363d;border-radius:4px;padding:6px"></textarea>
      <div style="display:flex;gap:8px;margin-top:8px">
        <select id="engine" style="background:#0d1117;color:#c9d1d9;border:1px solid #30363d;border-radius:4px;padding:4px">
          <option value="">自动</option><option value="vision">vision</option>
          <option value="code">code</option><option value="browser">browser</option>
        </select>
        <button id="runBtn" style="background:#238636;color:#fff;border:0;border-radius:4px;padding:4px 12px;cursor:pointer">启动</button>
      </div>
      <div id="runMsg" class="meta"></div>
    </section>
    <section><h2>后台任务</h2><div id="jobs">暂无任务</div></section>
    <section><h2>说明</h2>
      <p>画面每 800ms 刷新一次，来源为主屏 JPEG 缩略图。</p>
      <p>取消任务只会中止后续步骤，已发出的动作可能已经生效。</p>
      <p>命令行：<code>cua-hub console --bind 127.0.0.1:8787</code></p>
    </section>
  </aside>
</main>
<script>
const token = new URLSearchParams(location.search).get('token') || '';
const qs = token ? `?token=${encodeURIComponent(token)}` : '';
const img = document.getElementById('frame');
const screenEl = document.getElementById('screen');
const cursorEl = document.getElementById('cursor');
const jobsEl = document.getElementById('jobs');

function refreshFrame() { img.src = `/frame.jpg${qs}${qs ? '&' : '?'}t=${Date.now()}`; }
setInterval(refreshFrame, 800);
refreshFrame();

async function refreshState() {
  try {
    const r = await fetch(`/state${qs}`);
    if (!r.ok) return;
    const s = await r.json();
    if (s.screen) {
      screenEl.textContent = `主屏 ${s.screen.width}x${s.screen.height} · DPI ${s.dpi}`;
    }
    if (s.cursor) cursorEl.textContent = `光标 (${s.cursor.x}, ${s.cursor.y})`;
    renderJobs(s.jobs || []);
  } catch (e) {}
}

function renderJobs(jobs) {
  if (!jobs.length) { jobsEl.textContent = '暂无任务'; return; }
  jobsEl.innerHTML = '';
  for (const j of jobs) {
    const div = document.createElement('div');
    div.className = 'job';
    const summary = j.summary ? ` — ${escapeHtml(String(j.summary)).slice(0, 120)}` : '';
    div.innerHTML = `<div class="task">${escapeHtml(j.task || '')}</div>
      <div class="meta">${j.status} · ${j.steps} 步${summary}</div>`;
    if (j.status === 'running') {
      const btn = document.createElement('button');
      btn.textContent = '取消';
      btn.onclick = async () => {
        btn.disabled = true;
        await fetch(`/jobs/${encodeURIComponent(j.job_id)}/cancel${qs}`, { method: 'POST' });
        refreshState();
      };
      div.appendChild(btn);
    }
    jobsEl.appendChild(div);
  }
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}

document.getElementById('runBtn').onclick = async () => {
  const task = document.getElementById('taskText').value.trim();
  if (!task) return;
  const engine = document.getElementById('engine').value;
  const msg = document.getElementById('runMsg');
  msg.textContent = '启动中…';
  try {
    const r = await fetch(`/run${qs}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(engine ? { task, engine } : { task }),
    });
    const j = await r.json();
    msg.textContent = j.job_id ? `已启动 ${j.job_id}` : `失败：${JSON.stringify(j).slice(0, 120)}`;
    refreshState();
  } catch (e) {
    msg.textContent = `失败：${e}`;
  }
};

setInterval(refreshState, 1000);
refreshState();
</script>
</body>
</html>"#;
