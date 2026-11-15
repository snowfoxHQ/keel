// Keel 回测工作台前端（原生 JS，零依赖，金额以"分"整数传输，仅展示层做除法）
const $ = (id) => document.getElementById(id);

$("strategy").addEventListener("change", () => {
  const kind = $("strategy").value;
  $("param_day").classList.toggle("hidden", kind !== "day_cycle");
  $("param_ma").classList.toggle("hidden", kind !== "ma_cross");
});
document.querySelectorAll('input[name="source"]').forEach((r) =>
  r.addEventListener("change", () => {
    const v = document.querySelector('input[name="source"]:checked').value;
    $("file").disabled = v !== "file";
    $("param_web").classList.toggle("hidden", v !== "web");
  })
);

let fileCsv = null;
$("file").addEventListener("change", (ev) => {
  const f = ev.target.files[0];
  if (!f) return;
  const reader = new FileReader();
  reader.onload = () => { fileCsv = reader.result; };
  reader.readAsText(f);
});

$("run").addEventListener("click", async () => {
  const btn = $("run");
  btn.disabled = true; btn.textContent = "回测中…";
  $("error").classList.add("hidden");
  try {
    const source = document.querySelector('input[name="source"]:checked').value;
    if (source === "file" && !fileCsv) throw new Error("请先选择一个 CSV 文件");
    const params = new URLSearchParams();
    params.set("source", source);
    if (source === "file") params.set("csv", fileCsv);
    if (source === "web") {
      params.set("symbol", $("symbol").value.trim());
      params.set("date_start", $("date_start").value.trim());
      params.set("date_end", $("date_end").value.trim());
    }
    params.set("strategy", $("strategy").value);
    for (const id of ["hold_days", "fast", "slow", "cash_yuan", "commission_bp", "stamp_bp", "transfer_bp", "slippage_cents", "stop_yuan"]) {
      if ($(id)) params.set(id, $(id).value.trim());
    }
    const res = await fetch("/api/backtest", { method: "POST", body: params.toString() });
    const data = await res.json();
    if (!data.ok) throw new Error(data.error || "回测失败");
    $("welcome").classList.add("hidden");
    $("result").classList.remove("hidden");
    render(data);
  } catch (e) {
    $("error").textContent = "出错了：" + e.message;
    $("error").classList.remove("hidden");
  } finally {
    btn.disabled = false; btn.textContent = "开始回测";
  }
});

const yuan = (cents) => (cents / 100).toLocaleString("zh-CN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const date = (ts) => String(ts).replace(/^(\d{4})(\d{2})(\d{2})$/, "$1-$2-$3");

function render(d) {
  const pct = (d.return_bp / 100).toFixed(2);
  const cls = d.total_cents >= 0 ? "pos" : "neg";
  const sign = d.total_cents >= 0 ? "+" : "";
  $("cards").innerHTML = [
    ["总盈亏", sign + yuan(d.total_cents) + " 元", cls],
    ["收益率", sign + pct + "%", cls],
    ["成交笔数", d.fills, ""],
    ["手续费合计", yuan(d.fees_cents) + " 元", ""],
    ["最终现金", yuan(d.final_cash_cents) + " 元", ""],
    ["最终持仓", d.net_shares + " 股", ""],
  ].map(([t, v, c]) => '<div class="card"><div class="t">' + t + '</div><div class="v ' + c + '">' + v + "</div></div>").join("");

  const tb = $("trades").querySelector("tbody");
  tb.innerHTML = d.trades.map((r) =>
    '<tr><td>' + r[0] + "</td><td>" + date(r[1]) + '</td><td class="' + (r[2] === "买" ? "buy" : "sell") + '">' + r[2] +
    "</td><td>" + yuan(r[3]) + "</td><td>" + r[4] + "</td><td>" + yuan(r[5]) + "</td><td>" + yuan(r[6]) + "</td></tr>"
  ).join("");

  $("fingerprint").textContent = "过程指纹（事件哈希）" + d.event_hash + " · 事实链摘要" + d.chain_hash +
    " · 指纹相同 = 交易过程完全相同";

  drawChart(d);
}
function drawChart(d) {
  const cv = $("chart");
  const ctx = cv.getContext("2d");
  const W = cv.width, H = cv.height, L = 64, R = 16, T = 16, B = 30;
  const eq = d.curve.map((p) => p[1]);
  const pr = d.price.map((p) => p[1]);
  // 大文件时 Math.min(...大数组) 会超参数上限，用循环求极值
  let lo = Infinity, hi = -Infinity;
  for (const v of eq.concat(pr)) { if (v < lo) lo = v; if (v > hi) hi = v; }
  lo = lo * 0.998; hi = hi * 1.002;
  const x = (i, n) => L + ((W - L - R) * i) / Math.max(n - 1, 1);
  const y = (v) => T + ((H - T - B) * (hi - v)) / (hi - lo);
  const ts0 = d.curve[0][0], ts1 = d.curve[d.curve.length - 1][0];

  ctx.clearRect(0, 0, W, H);
  ctx.font = "11px sans-serif";
  // 网格与刻度（4 条）
  ctx.strokeStyle = "#eef1f6"; ctx.fillStyle = "#8b99b3";
  for (let i = 0; i <= 4; i++) {
    const v = lo + ((hi - lo) * i) / 4, yy = y(v);
    ctx.beginPath(); ctx.moveTo(L, yy); ctx.lineTo(W - R, yy); ctx.stroke();
    ctx.fillText(v.toFixed(0), 4, yy + 4);
  }
  // 首尾日期
  ctx.fillText(date(ts0), L, H - 8);
  const endW = ctx.measureText(date(ts1)).width;
  ctx.fillText(date(ts1), W - R - endW, H - 8);

  // 收盘价（灰）
  ctx.strokeStyle = "#b3bfce"; ctx.lineWidth = 1.2; ctx.beginPath();
  d.price.forEach((p, i) => { i === 0 ? ctx.moveTo(x(i, d.price.length), y(p[1])) : ctx.lineTo(x(i, d.price.length), y(p[1])); });
  ctx.stroke();
  // 资金曲线（蓝）
  ctx.strokeStyle = "#2b6cb0"; ctx.lineWidth = 2; ctx.beginPath();
  d.curve.forEach((p, i) => { i === 0 ? ctx.moveTo(x(i, d.curve.length), y(p[1])) : ctx.lineTo(x(i, d.curve.length), y(p[1])); });
  ctx.stroke();
  // 买点（红▲）卖点（绿▼）
  const eqAt = (ts) => { const p = d.curve.find((q) => q[0] === ts); return p ? p[1] : null; };
  const prAt = (ts) => { const p = d.price.find((q) => q[0] === ts); return p ? p[1] : null; };
  for (const ts of d.buys) drawMark(ctx, x(d.curve.findIndex((q) => q[0] === ts), d.curve.length), y(prAt(ts)), "#c53030", true);
  for (const ts of d.sells) drawMark(ctx, x(d.curve.findIndex((q) => q[0] === ts), d.curve.length), y(prAt(ts)), "#2f855a", false);
  // 图例
  ctx.fillStyle = "#2b6cb0"; ctx.fillRect(L, 0, 10, 3);
  ctx.fillStyle = "#3c4a63"; ctx.fillText("资金曲线", L + 14, 6);
  ctx.fillStyle = "#b3bfce"; ctx.fillRect(L + 70, 0, 10, 3);
  ctx.fillStyle = "#3c4a63"; ctx.fillText("收盘价", L + 84, 6);
  ctx.fillStyle = "#c53030"; ctx.fillText("▲买", L + 140, 6);
  ctx.fillStyle = "#2f855a"; ctx.fillText("▼卖", L + 175, 6);
}

function drawMark(ctx, px, py, color, up) {
  ctx.fillStyle = color;
  ctx.beginPath();
  if (up) { ctx.moveTo(px, py - 10); ctx.lineTo(px - 5, py); ctx.lineTo(px + 5, py); }
  else { ctx.moveTo(px, py + 10); ctx.lineTo(px - 5, py); ctx.lineTo(px + 5, py); }
  ctx.closePath(); ctx.fill();
}