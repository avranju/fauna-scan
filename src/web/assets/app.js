const state = {
  cameras: [],
  config: null,
  health: null,
  filters: { from: null, to: null, cameras: [] },
  pageCursor: null,
  refreshFailures: 0,
};

const $ = (selector) => document.querySelector(selector);
const content = $("#content");

function h(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;").replaceAll("'", "&#39;");
}

function localTime(value, options = {}) {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return "—";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: options.short ? undefined : "medium",
    timeStyle: "medium",
    ...(options.short ? { hour: "2-digit", minute: "2-digit", second: "2-digit" } : {}),
  }).format(date);
}

function relative(value) {
  if (!value) return "Never";
  const seconds = Math.round((new Date(value).valueOf() - Date.now()) / 1000);
  const abs = Math.abs(seconds);
  const [amount, unit] = abs < 60 ? [seconds, "second"] : abs < 3600 ? [Math.round(seconds / 60), "minute"] : abs < 86400 ? [Math.round(seconds / 3600), "hour"] : [Math.round(seconds / 86400), "day"];
  return new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }).format(amount, unit);
}

function cameraName(camera) {
  return camera?.name || `Camera ${camera?.channel ?? camera?.channel_number ?? "?"}`;
}

function species(result) {
  const values = Array.isArray(result?.species) ? result.species : [];
  const first = values[0];
  if (typeof first === "string") return first;
  return first?.name || (result?.contains_wildlife ? "Wildlife" : "No wildlife");
}

function confidence(result) {
  return Number.isFinite(result?.confidence) ? `${Math.round(result.confidence * 100)}%` : "";
}

function speciesDetails(result) {
  const values = Array.isArray(result?.species) ? result.species : [];
  if (!values.length) return "";
  return `<div class="badges">${values.map((item) => {
    const name = typeof item === "string" ? item : item?.name || "Unknown";
    const score = Number.isFinite(item?.confidence) ? ` · ${Math.round(item.confidence * 100)}%` : "";
    return `<span class="badge done">${h(name)}${h(score)}</span>`;
  }).join("")}</div>`;
}

async function api(path) {
  const response = await fetch(path, { headers: { Accept: "application/json" } });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body?.error?.message || `Request failed (${response.status})`);
  return body;
}

function showError(error) {
  content.innerHTML = `<div class="error-panel" role="alert"><strong>Unable to load this view</strong><p>${h(error.message)}</p><button class="button ghost" id="retry">Try again</button></div>`;
  $("#retry")?.addEventListener("click", route);
}

function toast(message) {
  const element = $("#toast");
  element.textContent = message;
  element.hidden = false;
  window.setTimeout(() => { element.hidden = true; }, 2200);
}

async function copy(value, label = "Copied") {
  try { await navigator.clipboard.writeText(value); toast(label); }
  catch { toast("Copy failed"); }
}

function setDefaultRange(hours = 24) {
  const to = new Date();
  const from = new Date(to.valueOf() - hours * 3600_000);
  state.filters.from = from.toISOString();
  state.filters.to = to.toISOString();
  syncTimeInputs();
}

function localInputValue(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  const offset = d.getTimezoneOffset() * 60_000;
  return new Date(d.valueOf() - offset).toISOString().slice(0, 19);
}

function syncTimeInputs() {
  $("#time-from").value = localInputValue(state.filters.from);
  $("#time-to").value = localInputValue(state.filters.to);
}

function queryString(extra = {}) {
  const query = new URLSearchParams();
  if (state.filters.from) query.set("from", state.filters.from);
  if (state.filters.to) query.set("to", state.filters.to);
  if (state.filters.cameras.length) query.set("camera", state.filters.cameras.join(","));
  Object.entries(extra).forEach(([key, value]) => {
    if (value !== null && value !== undefined && value !== "") query.set(key, value);
  });
  return query.toString();
}

function readUrlFilters() {
  const query = new URLSearchParams(location.search);
  if (query.has("from") || query.has("to")) {
    state.filters.from = query.get("from");
    state.filters.to = query.get("to");
  } else if (!state.filters.from) {
    setDefaultRange(24);
  }
  state.filters.cameras = (query.get("camera") || "").split(",").filter(Boolean);
  syncTimeInputs();
}

function writeUrlFilters() {
  const url = new URL(location.href);
  ["from", "to", "camera"].forEach((key) => url.searchParams.delete(key));
  if (state.filters.from) url.searchParams.set("from", state.filters.from);
  if (state.filters.to) url.searchParams.set("to", state.filters.to);
  if (state.filters.cameras.length) url.searchParams.set("camera", state.filters.cameras.join(","));
  history.pushState({}, "", url);
}

function setupFilters() {
  const cameraSelect = $("#camera-select");
  cameraSelect.innerHTML = state.cameras.map((camera) => `<option value="${camera.id}">${h(cameraName(camera))} · Ch ${camera.channel_number}${camera.enabled ? "" : " · inactive"}</option>`).join("");
  [...cameraSelect.options].forEach((option) => { option.selected = state.filters.cameras.includes(option.value); });
  syncTimeInputs();
}

function applyPreset() {
  const value = $("#range-preset").value;
  if (value === "all") {
    state.filters.from = null; state.filters.to = null; syncTimeInputs(); return;
  }
  if (value !== "custom") setDefaultRange(Number(value));
}

function applyFilters() {
  const from = $("#time-from").value;
  const to = $("#time-to").value;
  state.filters.from = from ? new Date(from).toISOString() : null;
  state.filters.to = to ? new Date(to).toISOString() : null;
  state.filters.cameras = [...$("#camera-select").selectedOptions].map((option) => option.value);
  writeUrlFilters();
  route();
}

function heading(eyebrow, title, generatedAt) {
  return `<header class="page-heading"><div><p class="eyebrow">${h(eyebrow)}</p><h1>${h(title)}</h1></div>${generatedAt ? `<span class="as-of">Updated ${h(relative(generatedAt))}</span>` : ""}</header>`;
}

function markNavigation() {
  document.querySelectorAll("[data-link]").forEach((link) => {
    const path = new URL(link.href).pathname;
    const active = path === "/" ? location.pathname === "/" : location.pathname.startsWith(path);
    link.classList.toggle("active", active);
    if (active) link.setAttribute("aria-current", "page"); else link.removeAttribute("aria-current");
  });
}

function bindLinks(root = document) {
  root.querySelectorAll("a[data-link]").forEach((link) => link.addEventListener("click", (event) => {
    if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    const target = new URL(link.href);
    if (link.closest(".primary-nav")) {
      const filters = new URLSearchParams(queryString());
      filters.forEach((value, key) => target.searchParams.set(key, value));
    }
    history.pushState({}, "", target); route();
  }));
}

async function renderOverview() {
  const query = queryString();
  const [overview, cameras, health] = await Promise.all([
    api(`/api/v1/overview?${query}`), api("/api/v1/cameras"), api("/api/v1/health"),
  ]);
  state.health = health;
  updateHealth(health);
  const c = overview.counts;
  const metrics = [
    ["Images discovered", c.discovered, ""], ["Downloaded", c.downloaded, ""],
    ["Classified", c.classified, ""], ["Wildlife", c.wildlife, "wildlife"],
    ["Interesting", c.interesting, "wildlife"], ["Retrying", c.retryable_failures, "warn"],
    ["Permanent failures", c.permanent_failures, "danger"],
  ];
  const visibleCameras = state.filters.cameras.length ? cameras.data.filter((camera) => state.filters.cameras.includes(String(camera.id))) : cameras.data;
  content.innerHTML = `${heading("Operations", "Overview", overview.generated_at)}
    <section class="metric-grid" aria-label="Summary counts">${metrics.map(([label, value, cls]) => `<article class="metric ${cls}"><span class="metric-value">${Number(value).toLocaleString()}</span><span class="metric-label">${h(label)}</span></article>`).join("")}</section>
    <section class="section-card"><div class="section-header"><h2>Pipeline status</h2></div><div class="health-grid">
      ${pipelineCard("Downloader", health.active_downloads, health.last_downloader_poll, "active download")}
      ${pipelineCard("Classifier", health.active_classifications, health.last_scanner_pass, "active classification")}
    </div></section>
    <section class="section-card"><div class="section-header"><h2>Camera search progress</h2><span class="subtle">${visibleCameras.length} cameras</span></div>${cameraTable(visibleCameras)}</section>`;
}

function pipelineCard(name, active, lastSuccess, unit) {
  const current = active > 0 ? `${active} ${unit}${active === 1 ? "" : "s"}` : "Idle";
  return `<article class="pipeline"><strong><span>${h(name)}</span><span class="badge ${active > 0 ? "processing" : "done"}">${h(current)}</span></strong><dl><dt>Last success</dt><dd title="${h(localTime(lastSuccess))}">${h(relative(lastSuccess))}</dd><dt>Web status</dt><dd>Connected</dd></dl></article>`;
}

function cameraTable(cameras) {
  if (!cameras.length) return `<div class="empty">No cameras have been discovered yet.</div>`;
  return `<div class="table-wrap"><table><thead><tr><th>Camera</th><th>Status</th><th>Search through</th><th>Last poll</th><th>Error</th></tr></thead><tbody>${cameras.map((camera) => `<tr><td><strong>${h(cameraName(camera))}</strong><br><span class="subtle">Channel ${camera.channel_number} · ${h(camera.picture_track_id)}</span></td><td><span class="badge ${camera.enabled ? "done" : ""}">${camera.enabled ? "Active" : "Inactive"}</span></td><td title="${h(localTime(camera.last_completed_window_end))}">${h(relative(camera.last_completed_window_end))}</td><td>${h(relative(camera.last_poll_at))}</td><td class="error-text">${h(camera.last_error || "—")}</td></tr>`).join("")}</tbody></table></div>`;
}

function imageExtraFilters() {
  const query = new URLSearchParams(location.search);
  return {
    download_status: query.get("download_status") || "",
    processing_status: query.get("processing_status") || "",
    contains_wildlife: query.get("contains_wildlife") || "",
    interesting: query.get("interesting") || "",
    sort: query.get("sort") || "captured_desc",
  };
}

async function renderImages(append = false) {
  const extra = imageExtraFilters();
  const query = queryString({ ...extra, limit: 60, cursor: append ? state.pageCursor : null });
  const response = await api(`/api/v1/images?${query}`);
  if (!append) {
    content.innerHTML = `${heading("Explore", "Images", response.generated_at)}
      <section class="image-toolbar" aria-label="Image filters">
        <label>Download <select id="download-filter"><option value="">Any state</option>${["downloaded","pending","downloading","retry_wait","unavailable","failed"].map((value) => `<option value="${value}" ${extra.download_status === value ? "selected" : ""}>${h(value.replaceAll("_", " "))}</option>`).join("")}</select></label>
        <label>Classification <select id="processing-filter"><option value="">Any state</option>${["done","new","processing","retry_wait","failed","missing"].map((value) => `<option value="${value}" ${extra.processing_status === value ? "selected" : ""}>${h(value.replaceAll("_", " "))}</option>`).join("")}</select></label>
        <label>Wildlife <select id="wildlife-filter"><option value="">Any</option><option value="true" ${extra.contains_wildlife === "true" ? "selected" : ""}>Wildlife</option><option value="false" ${extra.contains_wildlife === "false" ? "selected" : ""}>No wildlife</option></select></label>
        <label>Interesting <select id="interesting-filter"><option value="">Any</option><option value="true" ${extra.interesting === "true" ? "selected" : ""}>Interesting</option><option value="false" ${extra.interesting === "false" ? "selected" : ""}>Not interesting</option></select></label>
        <label>Order <select id="sort-filter"><option value="captured_desc" ${extra.sort === "captured_desc" ? "selected" : ""}>Newest first</option><option value="captured_asc" ${extra.sort === "captured_asc" ? "selected" : ""}>Oldest first</option></select></label>
        <button class="button primary" id="apply-image-filters">Filter</button>
        <span class="result-count">${response.data.length}${response.page.has_more ? "+" : ""} shown</span>
      </section><section id="gallery" class="gallery"></section><button id="load-more" class="button ghost load-more" type="button">Load more</button>`;
    $("#apply-image-filters").addEventListener("click", applyImageFilters);
  }
  const gallery = $("#gallery");
  gallery.insertAdjacentHTML("beforeend", response.data.map(imageCard).join(""));
  bindLinks(gallery);
  state.pageCursor = response.page.next_cursor;
  const loadMore = $("#load-more");
  loadMore.hidden = !response.page.has_more;
  loadMore.onclick = async () => { loadMore.disabled = true; await renderImages(true).catch(showError); loadMore.disabled = false; };
  if (!append && !response.data.length) gallery.innerHTML = `<div class="empty">No images match the selected time, cameras, and states.</div>`;
}

function applyImageFilters() {
  const url = new URL(location.href);
  const values = {
    download_status: $("#download-filter").value,
    processing_status: $("#processing-filter").value,
    contains_wildlife: $("#wildlife-filter").value,
    interesting: $("#interesting-filter").value,
    sort: $("#sort-filter").value,
  };
  Object.entries(values).forEach(([key, value]) => value ? url.searchParams.set(key, value) : url.searchParams.delete(key));
  history.pushState({}, "", url); route();
}

function imageCard(image) {
  const result = image.classification;
  const picture = image.thumbnail_url ? `<img src="${h(image.thumbnail_url)}" loading="lazy" alt="${h(result?.summary || `Captured image from ${cameraName(image.camera)}`)}">` : `<div class="thumb-placeholder"><span class="icon" aria-hidden="true">◫</span><span>${h(image.download_status.replaceAll("_", " "))}</span></div>`;
  return `<a class="image-card" href="/images/${image.id}${location.search}" data-link><div class="thumb">${picture}</div><div class="card-body"><div class="card-title"><strong>${h(cameraName(image.camera))}</strong><time datetime="${h(image.captured_at)}">${h(localTime(image.captured_at, { short: true }))}</time></div>${result ? `<div class="species">${h(species(result))} <span class="confidence">${h(confidence(result))}</span></div><p class="summary">${h(result.summary || "Classification completed without a summary.")}</p>` : `<div class="species">Awaiting result</div><p class="summary subtle">No completed classification.</p>`}<div class="badges"><span class="badge ${h(image.download_status)}">Download: ${h(image.download_status.replaceAll("_", " "))}</span><span class="badge ${h(image.processing_status)}">Model: ${h(image.processing_status.replaceAll("_", " "))}</span></div></div></a>`;
}

async function renderDetail(id) {
  const image = await api(`/api/v1/images/${id}`);
  const result = image.classifications[0];
  const imageView = image.content_url ? `<img src="${h(image.content_url)}" alt="${h(result?.summary || `Captured image from ${cameraName(image.camera)}`)}">` : `<div class="thumb-placeholder"><span class="icon">◫</span><span>Local image unavailable</span></div>`;
  content.innerHTML = `${heading("Image detail", cameraName(image.camera))}<div class="detail-layout"><section class="viewer">${imageView}</section><aside class="detail-panel">
    <p class="subtle"><time datetime="${h(image.captured_at)}">${h(localTime(image.captured_at))}</time> · Channel ${image.camera.channel}</p>
    ${result ? `<h2>Classification</h2><div class="species">${h(species(result))} <span class="confidence">${h(confidence(result))}</span></div>${speciesDetails(result)}<p class="detail-summary">${h(result.summary || "No summary returned.")}</p><div class="badges"><span class="badge ${result.contains_wildlife ? "done" : ""}">${result.contains_wildlife ? "Wildlife" : "No wildlife"}</span><span class="badge ${result.interesting ? "done" : ""}">${result.interesting ? "Interesting" : "Not marked interesting"}</span></div><p class="subtle">${h(result.model)} · ${h(result.prompt_version)} · ${h(localTime(result.request_completed_at))}</p>${result.structured ? `<details><summary>Structured model result</summary><pre class="json-view">${h(JSON.stringify(result.structured, null, 2))}</pre></details>` : ""}` : `<h2>Classification</h2><p class="subtle">${h(processingMessage(image.processing))}</p>`}
    <h2>NVR still image</h2>${image.nvr.image_url ? `<code class="url-field">${h(image.nvr.image_url)}</code><div class="link-actions"><a class="button primary" href="${h(image.nvr.image_url)}" target="_blank" rel="noopener noreferrer">Open NVR image</a><button class="button ghost" id="copy-image-url">Copy URL</button></div>` : `<p class="subtle">The stored NVR image URL is invalid or unavailable.</p>`}
    <h2>Video recording</h2><div id="recording-result"><p class="subtle">Find the NVR recording around this image timestamp.</p><button class="button primary" id="find-recording">Find recording</button></div>
    <h2>Current lifecycle</h2><div class="timeline">${timeline("Download", image.download)}${timeline("Classification", image.processing)}</div>
  </aside></div>`;
  $("#copy-image-url")?.addEventListener("click", () => copy(image.nvr.image_url, "NVR image URL copied"));
  $("#find-recording")?.addEventListener("click", () => findRecording(id));
}

function processingMessage(processing) {
  const messages = { new: "Waiting to be classified.", processing: "Classification is in progress.", retry_wait: `Classification will retry ${relative(processing.next_attempt_at)}.`, failed: "Classification failed permanently.", missing: "The local image file is missing." };
  return messages[processing.status] || `Classification state: ${processing.status}`;
}

function timeline(label, value) {
  const time = value.completed_at || value.downloaded_at || value.started_at || value.next_attempt_at;
  return `<div class="timeline-item"><strong>${h(label)} · ${h(value.status.replaceAll("_", " "))}</strong><span class="subtle">Attempt ${value.attempts}${time ? ` · ${h(localTime(time))}` : ""}</span>${value.last_error ? `<div class="error-text">${h(value.last_error)}</div>` : ""}</div>`;
}

async function findRecording(id) {
  const target = $("#recording-result");
  target.innerHTML = `<p class="subtle">Searching the NVR recording index…</p>`;
  try {
    const result = await api(`/api/v1/images/${id}/recording`);
    if (result.status === "not_found") { target.innerHTML = `<p class="subtle">No recording covers this timestamp.</p><button class="button ghost" id="retry-recording">Search again</button>`; $("#retry-recording").onclick = () => findRecording(id); return; }
    target.innerHTML = `<code class="url-field">${h(result.nvr_playback_uri)}</code><p class="subtle">Recording ${h(localTime(result.recording_start_at))} – ${h(localTime(result.recording_end_at))}</p><div class="link-actions"><a class="button primary" href="${h(result.nvr_playback_uri)}" rel="noopener noreferrer">Open in video player</a><button class="button ghost" id="copy-video-url">Copy URL</button></div>`;
    $("#copy-video-url").onclick = () => copy(result.nvr_playback_uri, "NVR video URL copied");
  } catch (error) { target.innerHTML = `<p class="error-text">${h(error.message)}</p><button class="button ghost" id="retry-recording">Try again</button>`; $("#retry-recording").onclick = () => findRecording(id); }
}

async function renderActivity() {
  const [activity, cameras] = await Promise.all([api(`/api/v1/activity?${queryString()}`), api("/api/v1/cameras")]);
  const visibleCameras = state.filters.cameras.length ? cameras.data.filter((camera) => state.filters.cameras.includes(String(camera.id))) : cameras.data;
  content.innerHTML = `${heading("Live operations", "Scan activity", activity.generated_at)}
    <section class="section-card"><div class="section-header"><h2>Queues and states</h2></div><div class="queue-grid">${activity.counts.map((item) => `<article class="queue-card"><span class="count">${Number(item.count).toLocaleString()}</span><span>${h(item.category)} · ${h(item.status.replaceAll("_", " "))}</span></article>`).join("") || `<div class="empty">No queued work.</div>`}</div></section>
    <section class="section-card"><div class="section-header"><h2>In progress</h2><span class="subtle">Refreshes every 10 seconds</span></div><div class="active-list">${activity.active.map(activeRow).join("") || `<div class="empty">No images are currently being downloaded or classified.</div>`}</div></section>
    <section class="section-card"><div class="section-header"><h2>Camera search cursors</h2></div>${cameraTable(visibleCameras)}</section>`;
  bindLinks(content);
}

function activeRow(item) {
  const processing = item.processing_status === "processing";
  const operation = processing ? "Classifying" : "Downloading";
  const lease = processing ? item.processing_lease_until : item.download_lease_until;
  const attempts = processing ? item.processing_attempts : item.download_attempts;
  const expired = lease && new Date(lease) < new Date();
  return `<a class="active-row" href="/images/${item.id}" data-link><span><strong>${h(operation)} · ${h(item.camera_name || `Camera ${item.channel}`)}</strong><br><span class="subtle">Captured ${h(localTime(item.captured_at))} · Attempt ${attempts}</span></span><span class="badge ${expired ? "failed" : "processing"}">${expired ? "Stale lease" : `Lease ${relative(lease)}`}</span></a>`;
}

async function renderAbout() {
  const config = state.config || await api("/api/v1/config");
  content.innerHTML = `${heading("System", "About Fauna Scan", config.generated_at)}<section class="section-card"><dl class="pipeline"><dt>Version</dt><dd>${h(config.version)}</dd><dt>NVR image links</dt><dd>${config.capabilities.nvr_still_url ? "Available" : "Unavailable"}</dd><dt>NVR recording lookup</dt><dd>${config.capabilities.nvr_recording_lookup ? "Available" : "Unavailable"}</dd><dt>Browser clip playback</dt><dd>${config.capabilities.browser_clip_playback ? "Available" : "External player required"}</dd><dt>Default clip window</dt><dd>${config.default_clip_pre_roll_seconds}s before / ${config.default_clip_post_roll_seconds}s after</dd></dl></section>`;
}

function updateHealth(health) {
  const button = $("#health-button");
  button.className = "health-pill ok";
  $("#health-label").textContent = health.active_downloads + health.active_classifications > 0 ? "Working" : "Healthy";
  button.title = `Downloader: ${health.active_downloads} active · Classifier: ${health.active_classifications} active`;
}

async function refreshHealth() {
  try {
    const health = await api("/api/v1/health");
    state.refreshFailures = 0; $("#connection-banner").hidden = true; updateHealth(health);
    if (location.pathname === "/activity") await renderActivity();
  } catch {
    state.refreshFailures += 1;
    if (state.refreshFailures >= 2) { $("#connection-banner").hidden = false; $("#health-button").className = "health-pill error"; $("#health-label").textContent = "Disconnected"; }
  }
}

async function route() {
  markNavigation(); readUrlFilters(); setupFilters(); content.innerHTML = `<div class="loading-panel" role="status">Loading…</div>`;
  try {
    const match = location.pathname.match(/^\/images\/(\d+)$/);
    if (match) await renderDetail(match[1]);
    else if (location.pathname === "/images") await renderImages();
    else if (location.pathname === "/activity") await renderActivity();
    else if (location.pathname === "/about") await renderAbout();
    else await renderOverview();
    bindLinks(content);
  } catch (error) { showError(error); }
}

async function start() {
  setDefaultRange(24);
  try {
    const [config, cameras] = await Promise.all([api("/api/v1/config"), api("/api/v1/cameras")]);
    state.config = config; state.cameras = cameras.data; readUrlFilters(); setupFilters(); await route();
  } catch (error) { showError(error); }
  bindLinks();
  $("#range-preset").addEventListener("change", applyPreset);
  $("#apply-filters").addEventListener("click", applyFilters);
  $("#reset-filters").addEventListener("click", () => { setDefaultRange(24); state.filters.cameras = []; writeUrlFilters(); setupFilters(); route(); });
  $("#health-button").addEventListener("click", () => { history.pushState({}, "", `/activity?${queryString()}`); route(); });
  window.addEventListener("popstate", route);
  window.setInterval(refreshHealth, 10_000);
}

start();
