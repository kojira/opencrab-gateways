// D-1106: the read-aloud dictionary editor, shared by the VC 設定 page (共通の辞書) and the bot page
// (この bot の辞書). Entries are zen-tts dictionary entries. The user types 言葉; 読み and the accent
// come from ZEN-TTS (`analyze`), the reading is shown as mora boxes with the high/low line, and a tap
// on a mora sets where the pitch drops. No accent number is shown or typed. A new word is
// 「おまかせ」 (no `accent` saved: ZEN-TTS decides) until the user taps. Fields imported from the
// zen-tts demo (phrase, strength, contour, pitch, length) are kept as they are (「抑揚あり」).
// Without ZEN-TTS only 言葉 / 読み are edited (VOICEVOX replaces the word with the reading).
(() => {
const dictAdvanced = ["phrase", "strength", "contour", "pitch", "length"];
const style = document.createElement("style");
style.textContent = `
.dict-row { border: 1px solid #8886; border-radius: 8px; padding: .5rem; margin: 0 0 .5rem; min-width: 0; }
.dict-fields { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: .5rem; }
.dict-fields input { margin-top: 0 !important; }
.dict-moras { display: flex; flex-wrap: wrap; align-items: stretch; margin: .5rem 0 .25rem; }
.dict-moras:empty { display: none; }
.dict-moras button.mora { min-width: 44px; min-height: 44px; margin: 0; padding: .2rem .45rem; font-size: 18px; border-radius: 0; background: none; border: 0; border-top: 3px solid transparent; border-bottom: 3px solid transparent; }
.dict-moras button.mora.hi { border-top-color: #2563eb; }
.dict-moras button.mora.lo { border-bottom-color: #2563eb; }
.dict-moras button.mora.rise { box-shadow: inset 3px 0 0 #2563eb; }
.dict-moras button.mora.drop { box-shadow: inset -3px 0 0 #dc2626; }
.dict-note { font-size: .9rem; color: #888; margin: 0 0 .25rem; }
.dict-actions { display: flex; flex-wrap: wrap; gap: .5rem; }
.dict-actions button { margin: 0; min-width: 44px; min-height: 44px; }
.dict-actions button.on { font-weight: 700; border-width: 2px; }
`;
document.head.appendChild(style);

async function errorOf(res) {
  try { return (await res.json()).error || res.statusText; } catch { return res.statusText; }
}

// box.dictConfig: { api, agent: () => agent id or null, zen: () => bool, report(text) }
window.dictSetup = (box, config) => { box.dictConfig = config; };

// Tokyo-style pitch: type 0 (平板) low then high, type 1 (頭高) high then low, type n low, high up
// to mora n, then low.
function high(k, accent) {
  if (accent === 1) return k === 1;
  if (k === 1) return false;
  return accent === 0 || k <= accent;
}

window.dictRow = (box, entry) => {
  const cfg = box.dictConfig || { api: "api/voice-settings", agent: () => null, zen: () => false, report: () => {} };
  const e = entry ? { ...entry } : { surface: "", reading: "" };
  const row = document.createElement("div");
  row.className = "dict-row";
  row.entry = e;
  row.accent = e.accent;      // undefined = おまかせ
  row.modelAccent = undefined;
  row.moras = [];
  const input = (cls, label, value, max) => {
    const el = document.createElement("input");
    el.className = cls; el.placeholder = label; el.setAttribute("aria-label", label); el.value = value ?? ""; el.maxLength = max;
    return el;
  };
  const fields = document.createElement("div");
  fields.className = "dict-fields";
  const surface = input("dict-surface", "言葉", e.surface, 64);
  const reading = input("dict-reading", "読み", e.reading, 128);
  fields.append(surface, reading);
  const moras = document.createElement("div");
  moras.className = "dict-moras";
  const note = document.createElement("p");
  note.className = "dict-note";
  const actions = document.createElement("div");
  actions.className = "dict-actions";
  const button = (text, onclick) => {
    const b = document.createElement("button");
    b.type = "button"; b.textContent = text; b.onclick = onclick;
    return b;
  };
  const zen = () => cfg.zen();
  const lookup = button("読みを調べる", () => analyze(true));
  const flat = button("平板", () => { row.accent = 0; draw(); });
  const auto = button("おまかせ", () => { row.accent = undefined; draw(); });
  const play = button("▶", () => preview());
  play.setAttribute("aria-label", "この言葉を聞く");
  const del = button("削除", () => row.remove());
  actions.append(lookup, flat, auto, play, del);
  row.append(fields, moras, note, actions);
  if (dictAdvanced.some(k => e[k] !== undefined)) {
    const mark = document.createElement("p");
    mark.className = "dict-note dict-mark"; mark.textContent = "抑揚あり";
    row.appendChild(mark);
  }

  function draw() {
    const show = zen();
    lookup.hidden = flat.hidden = auto.hidden = !show;
    moras.innerHTML = "";
    note.textContent = "";
    if (!show || !row.moras.length) return;
    if (row.accent !== undefined && row.accent > row.moras.length) row.accent = undefined;
    const accent = row.accent ?? row.modelAccent;
    row.moras.forEach((kana, i) => {
      const k = i + 1;
      const b = document.createElement("button");
      b.type = "button"; b.className = "mora"; b.textContent = kana;
      b.setAttribute("aria-label", `「${kana}」の後で下げる`);
      if (accent !== undefined) {
        const hi = high(k, accent);
        b.classList.add(hi ? "hi" : "lo");
        if (hi && k > 1 && !high(k - 1, accent)) b.classList.add("rise");
        if (accent === k) b.classList.add("drop");
      }
      b.onclick = () => { row.accent = k; draw(); };
      moras.appendChild(b);
    });
    const where = accent === undefined ? "" : accent === 0 ? "平板（下がらない）" : `「${row.moras[accent - 1]}」の後で下がる`;
    note.textContent = row.accent === undefined
      ? `おまかせ${where ? `：${where}` : ""}。下げたい音をタップすると決められます。`
      : `${where}。`;
    flat.classList.toggle("on", row.accent === 0);
    auto.classList.toggle("on", row.accent === undefined);
  }

  // force: the 読みを調べる button (the reading is replaced by ZEN-TTS's reading).
  async function analyze(force) {
    if (!zen()) return;
    const word = surface.value.trim();
    if (!word) return;
    const keep = !force && reading.value.trim();
    const body = { text: word };
    if (cfg.agent()) body.agent = cfg.agent();
    if (keep) body.dict = [{ surface: word, reading: reading.value.trim() }];
    const res = await fetch(`${cfg.api}/analyze`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
    }).catch(() => null);
    if (!res || !res.ok) { note.textContent = `読みを調べられません: ${res ? await errorOf(res) : "通信できません"}`; return; }
    const got = await res.json();
    if (!keep) reading.value = got.reading;
    row.moras = got.moras || [];
    row.modelAccent = got.accent;
    draw();
  }

  async function preview() {
    const entry = collectRow();
    if (!entry) { cfg.report("言葉を入れてください。"); return; }
    cfg.report("音声を用意しています…");
    const body = { text: `${entry.surface}です。`, dict: [entry] };
    if (cfg.agent()) body.agent = cfg.agent();
    const res = await fetch(`${cfg.api}/test-tts`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
    });
    if (!res.ok) { cfg.report(`再生できません: ${await errorOf(res)}`); return; }
    const url = URL.createObjectURL(await res.blob());
    const audio = new Audio(url);
    audio.onended = () => URL.revokeObjectURL(url);
    cfg.report(`「${entry.surface}です。」を再生しています（まだ保存していません）。`);
    try { await audio.play(); } catch { cfg.report("ブラウザが再生を止めました。もう一度押してください。"); }
  }

  function collectRow() {
    const word = surface.value.trim();
    if (!word) return null;
    const out = { ...row.entry, surface: word, reading: reading.value.trim() };
    if (row.accent === undefined) delete out.accent; else out.accent = row.accent;
    return out;
  }
  row.collect = collectRow;

  surface.addEventListener("change", () => analyze(false));
  reading.addEventListener("change", () => { row.accent = undefined; analyze(false); });
  box.appendChild(row);
  if (e.surface && e.reading) analyze(false); else draw();
  return row;
};

window.dictFill = (box, entries) => {
  box.innerHTML = "";
  for (const e of entries || []) dictRow(box, e);
};

window.dictCollect = box => [...box.querySelectorAll(".dict-row")].map(row => row.collect()).filter(Boolean);

window.dictExport = (box, name) => {
  const body = JSON.stringify({ format: "zen-tts-dictionary", version: 1, entries: dictCollect(box) }, null, 2);
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob([body], { type: "application/json" }));
  a.download = `${name}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
};

// Adds the entries of a zen-tts dictionary file (or a bare entry array). A surface already in the
// list is replaced. Saving is still the 保存 button.
window.dictImport = async (box, file, report) => {
  try {
    const data = JSON.parse(await file.text());
    const entries = Array.isArray(data) ? data : data.entries;
    if (!Array.isArray(entries)) throw new Error("entries がありません");
    const current = dictCollect(box);
    for (const e of entries) {
      if (!e || typeof e.surface !== "string" || typeof e.reading !== "string") throw new Error("surface と reading が要ります");
      const i = current.findIndex(c => c.surface === e.surface);
      if (i >= 0) current[i] = e; else current.push(e);
    }
    dictFill(box, current);
    report(`${entries.length} 語を読み込みました。保存すると反映されます。`);
  } catch (err) {
    report(`読み込めません: ${err.message}`);
  }
};
})();
