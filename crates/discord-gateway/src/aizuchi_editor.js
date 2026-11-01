// D-1107: the aizuchi list of one bot, one row per item. An item is a word and, for a ZEN-TTS bot,
// an optional way of saying it (intonation): where the pitch drops (tap a sound, 平板, おまかせ), each
// sound raised or lowered (▲▼, one semitone a step, up to 6) and stretched or shortened (のばす /
// ちぢめる). The same word may be listed again with another way of saying it (「同じ言葉で別の言い方」).
// No number is shown or typed. An item without intonation is saved as a plain string, as before.
(() => {
const MAX_PITCH = 6;
const LENGTHS = [{ value: 1, label: "ふつう" }, { value: 1.5, label: "のばす" }, { value: 0.7, label: "ちぢめる" }];
const COL = 64;
const style = document.createElement("style");
style.textContent = `
.az-list { margin: 0 0 var(--gap); }
.az-row { border: 1px solid #8886; border-radius: 8px; padding: .5rem; margin: 0 0 .5rem; min-width: 0; }
.az-head { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; }
.az-head input { flex: 1 1 8rem; min-width: 0; margin: 0 !important; }
.az-head button, .az-actions button { margin: 0; min-width: 44px; min-height: 44px; }
.az-mark { font-size: .9rem; color: #2563eb; }
.az-tone { overflow-x: auto; margin: .5rem 0 .25rem; }
.az-tone[hidden] { display: none; }
.az-track { position: relative; display: inline-block; }
.az-track svg { display: block; }
.az-track svg polyline { fill: none; stroke: #2563eb; stroke-width: 3; stroke-linejoin: round; }
.az-track svg circle { fill: #2563eb; }
.az-track svg line.drop { stroke: #dc2626; stroke-width: 3; }
.az-cols { display: flex; }
.az-col { width: ${COL}px; flex: none; display: flex; flex-direction: column; align-items: stretch; gap: 2px; }
.az-col button { margin: 0; min-height: 44px; padding: 0; font-size: 16px; }
.az-col button.kana { font-size: 20px; font-weight: 600; }
.az-col button.kana.drop { box-shadow: inset -3px 0 0 #dc2626; }
.az-col button.len { font-size: 14px; }
.az-col button.len.on { font-weight: 700; border-width: 2px; }
.az-note { font-size: .9rem; color: #888; margin: .25rem 0; }
.az-actions { display: flex; flex-wrap: wrap; gap: .5rem; margin-top: .25rem; }
.az-actions button.on { font-weight: 700; border-width: 2px; }
`;
document.head.appendChild(style);

function high(k, accent) {
  if (accent === 1) return k === 1;
  if (k === 1) return false;
  return accent === 0 || k <= accent;
}
function surfaceOf(text) { return text.trim().replace(/[、。,.…]+$/u, "").trim(); }
async function errorOf(res) {
  try { return (await res.json()).error || res.statusText; } catch { return res.statusText; }
}

// cfg: { analyzeUrl, previewUrl, agent, zen: () => bool, report(text) }
window.aizuchiList = (box, items, cfg) => {
  box.classList.add("az-list");
  box.azConfig = cfg;
  box.innerHTML = "";
  for (const item of items || []) aizuchiRow(box, item);
};

window.aizuchiCollect = box => [...box.querySelectorAll(".az-row")].map(row => row.collect()).filter(Boolean);

window.aizuchiRedraw = () => document.querySelectorAll(".az-row").forEach(row => row.draw());

window.aizuchiRow = (box, item, after) => {
  const cfg = box.azConfig;
  const src = typeof item === "string" ? { text: item } : { ...(item || { text: "" }) };
  const row = document.createElement("div");
  row.className = "az-row";
  const st = {
    reading: src.reading || "",
    accent: src.accent,
    pitch: src.pitch ? [...src.pitch] : null,
    length: src.length ? [...src.length] : null,
    moras: [], modelAccent: undefined, open: false,
  };
  const button = (text, onclick, label) => {
    const b = document.createElement("button");
    b.type = "button"; b.textContent = text; b.onclick = onclick;
    if (label) b.setAttribute("aria-label", label);
    return b;
  };
  const head = document.createElement("div");
  head.className = "az-head";
  const word = document.createElement("input");
  word.className = "az-word"; word.value = src.text || ""; word.maxLength = 20;
  word.placeholder = "相づち"; word.setAttribute("aria-label", "相づちの言葉");
  const play = button("▶", () => preview(), "この言い方で聞く");
  const toggle = button("言い方", () => { st.open = !st.open; if (st.open && !st.moras.length) analyze(); draw(); });
  const mark = document.createElement("span");
  mark.className = "az-mark";
  head.append(word, play, toggle, mark);
  const tone = document.createElement("div");
  tone.className = "az-tone";
  const note = document.createElement("p");
  note.className = "az-note";
  const actions = document.createElement("div");
  actions.className = "az-actions";
  const flat = button("平板", () => { st.accent = 0; draw(); });
  const auto = button("おまかせ", () => { st.accent = undefined; draw(); });
  const clear = button("言い方を消す", () => { st.accent = undefined; st.pitch = null; st.length = null; draw(); });
  const toneActions = document.createElement("div");
  toneActions.className = "az-actions";
  toneActions.append(flat, auto, clear);
  const variant = button("同じ言葉で別の言い方", () => {
    const copy = aizuchiRow(box, row.collect() || { text: word.value }, row);
    copy.open();
  });
  const del = button("削除", () => row.remove());
  actions.append(variant, del);
  row.append(head, tone, toneActions, note, actions);

  const hasTone = () => st.accent !== undefined
    || (st.pitch || []).some(p => p !== 0) || (st.length || []).some(l => l !== 1);

  function draw() {
    const zen = cfg.zen();
    toggle.hidden = !zen;
    variant.hidden = !zen;
    mark.textContent = hasTone() ? "言い方あり" : "";
    const show = zen && st.open;
    tone.hidden = !show;
    toneActions.hidden = !show;
    tone.innerHTML = "";
    note.textContent = "";
    if (!show) return;
    if (!st.moras.length) { note.textContent = "読みを調べています…"; return; }
    const n = st.moras.length;
    if (!st.pitch || st.pitch.length !== n) st.pitch = new Array(n).fill(0);
    if (!st.length || st.length.length !== n) st.length = new Array(n).fill(1);
    if (st.accent !== undefined && st.accent > n) st.accent = undefined;
    const accent = st.accent ?? st.modelAccent;
    const track = document.createElement("div");
    track.className = "az-track";
    // The pitch line: high/low from where it drops, plus each sound's ▲▼.
    const h = 90, mid = 45, step = 5;
    const svgNs = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(svgNs, "svg");
    svg.setAttribute("width", n * COL); svg.setAttribute("height", h);
    svg.setAttribute("aria-hidden", "true");
    const pts = st.moras.map((_, i) => {
      const base = accent === undefined ? 0 : high(i + 1, accent) ? 1 : -1;
      const y = Math.max(4, Math.min(h - 4, mid - base * 12 - st.pitch[i] * step));
      return [i * COL + COL / 2, y];
    });
    const line = document.createElementNS(svgNs, "polyline");
    line.setAttribute("points", pts.map(p => p.join(",")).join(" "));
    svg.appendChild(line);
    for (const [x, y] of pts) {
      const c = document.createElementNS(svgNs, "circle");
      c.setAttribute("cx", x); c.setAttribute("cy", y); c.setAttribute("r", 5);
      svg.appendChild(c);
    }
    if (accent) {
      const d = document.createElementNS(svgNs, "line");
      d.setAttribute("class", "drop");
      d.setAttribute("x1", accent * COL); d.setAttribute("x2", accent * COL);
      d.setAttribute("y1", 0); d.setAttribute("y2", h);
      svg.appendChild(d);
    }
    track.appendChild(svg);
    const cols = document.createElement("div");
    cols.className = "az-cols";
    st.moras.forEach((kana, i) => {
      const k = i + 1;
      const col = document.createElement("div");
      col.className = "az-col";
      const up = button("▲", () => { st.pitch[i] = Math.min(MAX_PITCH, st.pitch[i] + 1); draw(); }, `「${kana}」を高く`);
      up.disabled = st.pitch[i] >= MAX_PITCH;
      const k_ = button(kana, () => { st.accent = k; draw(); }, `「${kana}」の後で下げる`);
      k_.className = "kana";
      if (accent === k) k_.classList.add("drop");
      const down = button("▼", () => { st.pitch[i] = Math.max(-MAX_PITCH, st.pitch[i] - 1); draw(); }, `「${kana}」を低く`);
      down.disabled = st.pitch[i] <= -MAX_PITCH;
      const now = LENGTHS.findIndex(l => l.value === st.length[i]);
      const len = button(LENGTHS[Math.max(0, now)].label, () => {
        st.length[i] = LENGTHS[(Math.max(0, now) + 1) % LENGTHS.length].value; draw();
      }, `「${kana}」の長さを変える`);
      len.className = "len";
      if (st.length[i] !== 1) len.classList.add("on");
      col.append(up, k_, down, len);
      cols.appendChild(col);
    });
    track.appendChild(cols);
    tone.appendChild(track);
    const where = accent === undefined ? "" : accent === 0 ? "平板（下がらない）" : `「${st.moras[accent - 1]}」の後で下がる`;
    note.textContent = (st.accent === undefined ? `おまかせ${where ? `：${where}` : ""}。音をタップすると、そこで下がります。` : `${where}。`)
      + "▲▼で高さ、下のボタンで長さを変えられます。▶で聞けます。";
    flat.classList.toggle("on", st.accent === 0);
    auto.classList.toggle("on", st.accent === undefined);
  }

  async function analyze() {
    const surface = surfaceOf(word.value);
    if (!surface) { st.moras = []; note.textContent = "言葉を入れてください。"; return; }
    const body = { text: surface, agent: cfg.agent };
    if (st.reading) body.dict = [{ surface, reading: st.reading }];
    const res = await fetch(cfg.analyzeUrl, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
    }).catch(() => null);
    if (!res || !res.ok) { note.textContent = `読みを調べられません: ${res ? await errorOf(res) : "通信できません"}`; return; }
    const got = await res.json();
    if (!st.reading) st.reading = got.reading;
    st.moras = got.moras || [];
    st.modelAccent = got.accent;
    draw();
  }

  row.collect = () => {
    const text = word.value.trim();
    if (!text) return null;
    if (!hasTone() || !st.reading) return text;
    const out = { text, reading: st.reading };
    if (st.accent !== undefined) out.accent = st.accent;
    if ((st.pitch || []).some(p => p !== 0)) out.pitch = [...st.pitch];
    if ((st.length || []).some(l => l !== 1)) out.length = [...st.length];
    return out;
  };
  row.draw = draw;
  row.open = () => { st.open = true; if (!st.moras.length) analyze(); draw(); };

  async function preview() {
    const item = row.collect();
    if (!item) { cfg.report("言葉を入れてください。"); return; }
    cfg.report("音声を用意しています…");
    const body = typeof item === "string" ? { text: item } : item;
    const res = await fetch(cfg.previewUrl, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
    });
    if (!res.ok) { cfg.report(await errorOf(res)); return; }
    const url = URL.createObjectURL(await res.blob());
    const audio = new Audio(url);
    audio.onended = () => URL.revokeObjectURL(url);
    cfg.report("");
    try { await audio.play(); } catch { cfg.report("ブラウザが再生を止めました。もう一度押してください。"); }
  }

  word.addEventListener("change", () => { st.reading = ""; st.moras = []; st.pitch = null; st.length = null; st.accent = undefined; if (st.open) analyze(); draw(); });
  if (after && after.nextSibling) box.insertBefore(row, after.nextSibling); else box.appendChild(row);
  draw();
  if (hasTone() && cfg.zen()) analyze();
  return row;
};
})();
