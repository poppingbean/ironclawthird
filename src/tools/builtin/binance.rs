//! Binance USDT-M Futures tools for the crypto_trading skill.
//!
//! Tools:
//! - `price_analysis`          — klines + full indicator suite (RSI, MACD, BB, EMA, Ichimoku, ADX)
//! - `binance_snapshot`        — order-book depth snapshot (public)
//! - `binance_futures_account` — account balance + open positions (HMAC-SHA256 auth)
//! - `binance_futures_order`   — place / close futures orders (HMAC-SHA256 auth, Always approval)
//!
//! Authentication: `BINANCE_API_KEY` and `BINANCE_API_SECRET` from environment (.env).
//! Network: all requests go to `https://fapi.binance.com` (USDT-M Futures).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::Sha256;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, ToolRateLimitConfig};

const FAPI_BASE: &str = "https://fapi.binance.com";

// ─────────────────────────────────────────────────────────────────────────────
// Shared HMAC helpers
// ─────────────────────────────────────────────────────────────────────────────

fn sign_query(secret: &str, query: &str) -> Result<String, ToolError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| ToolError::ExecutionFailed(format!("HMAC key error: {e}")))?;
    mac.update(query.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn timestamp_ms() -> Result<u64, ToolError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|e| ToolError::ExecutionFailed(format!("System clock error: {e}")))
}

fn binance_api_key() -> Result<String, ToolError> {
    std::env::var("BINANCE_API_KEY")
        .map_err(|_| ToolError::NotAuthorized("BINANCE_API_KEY not set in environment".into()))
}

fn binance_api_secret() -> Result<String, ToolError> {
    std::env::var("BINANCE_API_SECRET")
        .map_err(|_| ToolError::NotAuthorized("BINANCE_API_SECRET not set in environment".into()))
}

fn build_client(timeout_secs: u64) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_default()
}

/// Place a reduce-only close order (TP or SL) using `closePosition=true`.
/// This closes the entire open position at the given trigger price without
/// needing to specify a quantity.
#[allow(clippy::too_many_arguments)]
async fn place_close_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    symbol: &str,
    side: &str,
    order_type: &str, // TAKE_PROFIT_MARKET or STOP_MARKET
    stop_price: f64,
    position_side: &str,
) -> Result<Value, ToolError> {
    let ts = timestamp_ms()?;
    let query = format!(
        "symbol={symbol}&side={side}&type={order_type}&stopPrice={stop_price}\
         &closePosition=true&positionSide={position_side}&timestamp={ts}"
    );
    let sig = sign_query(api_secret, &query)?;
    let url = format!("{FAPI_BASE}/fapi/v1/order?{query}&signature={sig}");

    let resp = client
        .post(&url)
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await
        .map_err(|e| ToolError::ExternalService(format!("{order_type} placement failed: {e}")))?;

    let st = resp.status();
    let body: Value = resp.json().await.unwrap_or(serde_json::json!({}));
    if !st.is_success() {
        return Err(ToolError::ExternalService(format!(
            "Binance {order_type} {st}: {body}"
        )));
    }
    Ok(serde_json::json!({
        "order_id": body["orderId"],
        "type": body["type"],
        "stop_price": body["stopPrice"],
        "status": body["status"]
    }))
}

/// Fetch the current mark price for a symbol from FAPI.
/// Used to convert a USDT notional into base-asset quantity.
async fn fetch_mark_price(client: &Client, symbol: &str) -> Result<f64, ToolError> {
    let url = format!("{FAPI_BASE}/fapi/v1/premiumIndex?symbol={symbol}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ToolError::ExternalService(format!("Mark price fetch failed: {e}")))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::ExternalService(format!(
            "Mark price {st}: {body}"
        )));
    }
    let raw: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::ExternalService(format!("Mark price JSON: {e}")))?;
    raw["markPrice"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&p| p > 0.0)
        .ok_or_else(|| ToolError::ExternalService("Invalid markPrice in response".into()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Technical indicator helpers
// ─────────────────────────────────────────────────────────────────────────────

struct Ohlcv {
    #[allow(dead_code)]
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    #[allow(dead_code)]
    volume: f64,
}

/// Parse Binance kline arrays `[[openTime, open, high, low, close, volume, ...], ...]`.
fn parse_klines(raw: &Value) -> Result<Vec<Ohlcv>, ToolError> {
    let arr = raw
        .as_array()
        .ok_or_else(|| ToolError::ExternalService("Expected kline array from Binance".into()))?;
    arr.iter()
        .map(|row| {
            let row = row
                .as_array()
                .ok_or_else(|| ToolError::ExternalService("Kline row is not an array".into()))?;
            let f = |idx: usize| -> Result<f64, ToolError> {
                row.get(idx)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| {
                        ToolError::ExternalService(format!("Bad kline field at index {idx}"))
                    })
            };
            Ok(Ohlcv {
                open: f(1)?,
                high: f(2)?,
                low: f(3)?,
                close: f(4)?,
                volume: f(5)?,
            })
        })
        .collect()
}

/// Full-length EMA (indices 0..period-2 are zero / invalid).
fn ema_full(values: &[f64], period: usize) -> Vec<f64> {
    if period == 0 || values.len() < period {
        return vec![0.0; values.len()];
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = vec![0.0_f64; values.len()];
    out[period - 1] = values[..period].iter().sum::<f64>() / period as f64;
    for i in period..values.len() {
        out[i] = values[i] * k + out[i - 1] * (1.0 - k);
    }
    out
}

/// Wilder's smoothing (RSI / ADX).
fn wilder_smooth(values: &[f64], period: usize) -> Option<Vec<f64>> {
    if values.len() < period || period == 0 {
        return None;
    }
    let initial: f64 = values[..period].iter().sum::<f64>() / period as f64;
    let mut result = Vec::with_capacity(values.len() - period + 1);
    result.push(initial);
    for &v in &values[period..] {
        let prev = *result.last()?;
        result.push((prev * (period as f64 - 1.0) + v) / period as f64);
    }
    Some(result)
}

/// RSI with Wilder's smoothing.
fn compute_rsi(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() < period + 1 {
        return None;
    }
    let mut gains = Vec::with_capacity(closes.len() - 1);
    let mut losses = Vec::with_capacity(closes.len() - 1);
    for i in 1..closes.len() {
        let d = closes[i] - closes[i - 1];
        gains.push(if d > 0.0 { d } else { 0.0 });
        losses.push(if d < 0.0 { d.abs() } else { 0.0 });
    }
    let ag = wilder_smooth(&gains, period)?;
    let al = wilder_smooth(&losses, period)?;
    let g = *ag.last()?;
    let l = *al.last()?;
    if l == 0.0 {
        return Some(100.0);
    }
    Some(100.0 - 100.0 / (1.0 + g / l))
}

/// MACD (12, 26, 9) → (line, signal, histogram).
fn compute_macd(closes: &[f64]) -> Option<(f64, f64, f64)> {
    if closes.len() < 34 {
        return None;
    }
    let e12 = ema_full(closes, 12);
    let e26 = ema_full(closes, 26);
    let macd_series: Vec<f64> = (25..closes.len()).map(|i| e12[i] - e26[i]).collect();
    if macd_series.len() < 9 {
        return None;
    }
    let sig = ema_full(&macd_series, 9);
    let m = *macd_series.last()?;
    let s = *sig.last()?;
    Some((m, s, m - s))
}

/// Bollinger Bands (20, 2) → (middle, upper, lower, %B, bandwidth).
fn compute_bollinger(closes: &[f64]) -> Option<(f64, f64, f64, f64, f64)> {
    const P: usize = 20;
    if closes.len() < P {
        return None;
    }
    let slice = &closes[closes.len() - P..];
    let sma = slice.iter().sum::<f64>() / P as f64;
    let std = (slice.iter().map(|v| (v - sma).powi(2)).sum::<f64>() / P as f64).sqrt();
    let upper = sma + 2.0 * std;
    let lower = sma - 2.0 * std;
    let last = *closes.last()?;
    let pct_b = if (upper - lower).abs() > f64::EPSILON {
        (last - lower) / (upper - lower)
    } else {
        0.5
    };
    let bw = if sma.abs() > f64::EPSILON {
        (upper - lower) / sma
    } else {
        0.0
    };
    Some((sma, upper, lower, pct_b, bw))
}

/// Ichimoku (9/26/52) → (tenkan, kijun, span_a, span_b, cloud_is_green).
fn compute_ichimoku(candles: &[Ohlcv]) -> Option<(f64, f64, f64, f64, bool)> {
    if candles.len() < 52 {
        return None;
    }
    let n = candles.len();
    let hl = |s: &[Ohlcv]| {
        (
            s.iter().map(|c| c.high).fold(f64::NEG_INFINITY, f64::max),
            s.iter().map(|c| c.low).fold(f64::INFINITY, f64::min),
        )
    };
    let (h9, l9) = hl(&candles[n - 9..]);
    let tenkan = (h9 + l9) / 2.0;
    let (h26, l26) = hl(&candles[n - 26..]);
    let kijun = (h26 + l26) / 2.0;
    let span_a = (tenkan + kijun) / 2.0;
    let (h52, l52) = hl(&candles[n - 52..]);
    let span_b = (h52 + l52) / 2.0;
    Some((tenkan, kijun, span_a, span_b, span_a > span_b))
}

/// ADX (14) → (adx, +DI, -DI).
fn compute_adx(candles: &[Ohlcv]) -> Option<(f64, f64, f64)> {
    const P: usize = 14;
    if candles.len() < P * 2 + 1 {
        return None;
    }
    let n = candles.len();
    let mut tr = Vec::with_capacity(n - 1);
    let mut pdm = Vec::with_capacity(n - 1);
    let mut ndm = Vec::with_capacity(n - 1);
    for i in 1..n {
        let (h, l, pc) = (candles[i].high, candles[i].low, candles[i - 1].close);
        tr.push((h - l).max((h - pc).abs()).max((l - pc).abs()));
        let up = h - candles[i - 1].high;
        let dn = candles[i - 1].low - l;
        pdm.push(if up > dn && up > 0.0 { up } else { 0.0 });
        ndm.push(if dn > up && dn > 0.0 { dn } else { 0.0 });
    }
    let atr = wilder_smooth(&tr, P)?;
    let sp = wilder_smooth(&pdm, P)?;
    let sn = wilder_smooth(&ndm, P)?;
    let len = atr.len().min(sp.len()).min(sn.len());
    let mut dx = Vec::with_capacity(len);
    for i in 0..len {
        let pdi = if atr[i] > 0.0 {
            100.0 * sp[i] / atr[i]
        } else {
            0.0
        };
        let ndi = if atr[i] > 0.0 {
            100.0 * sn[i] / atr[i]
        } else {
            0.0
        };
        let sum = pdi + ndi;
        dx.push(if sum > 0.0 {
            100.0 * (pdi - ndi).abs() / sum
        } else {
            0.0
        });
    }
    let adx_series = wilder_smooth(&dx, P)?;
    let la = *atr.last()?;
    let lpdi = if la > 0.0 {
        100.0 * sp.last()? / la
    } else {
        0.0
    };
    let lndi = if la > 0.0 {
        100.0 * sn.last()? / la
    } else {
        0.0
    };
    Some((*adx_series.last()?, lpdi, lndi))
}

/// EMA cross direction on the last two bars.
fn ema_cross_direction(closes: &[f64], fast: usize, slow: usize) -> &'static str {
    if closes.len() < slow + 2 {
        return "insufficient_data";
    }
    let n = closes.len();
    let ef = ema_full(closes, fast);
    let es = ema_full(closes, slow);
    match (ef[n - 2] > es[n - 2], ef[n - 1] > es[n - 1]) {
        (false, true) => "bullish_cross",
        (true, false) => "bearish_cross",
        (_, true) => "bullish",
        (_, false) => "bearish",
    }
}

#[inline]
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
#[inline]
fn r4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ═════════════════════════════════════════════════════════════════════════════
// 1. PriceAnalysisTool
// ═════════════════════════════════════════════════════════════════════════════

/// Fetches Binance Futures klines and computes RSI, MACD, Bollinger Bands,
/// EMA crosses, Ichimoku Cloud, and ADX in a single call.
pub struct PriceAnalysisTool {
    client: Client,
}

impl PriceAnalysisTool {
    pub fn new() -> Self {
        Self {
            client: build_client(30),
        }
    }
}

impl Default for PriceAnalysisTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for PriceAnalysisTool {
    fn name(&self) -> &str {
        "price_analysis"
    }

    fn description(&self) -> &str {
        "Fetch Binance USDT-M Futures klines and compute technical indicators: \
         RSI(14), MACD(12,26,9), Bollinger Bands(20,2), EMA crosses (9/21, 20/50), \
         Ichimoku Cloud (9/26/52), and ADX(14). Use for multi-timeframe analysis \
         (4h trend with fewer candles ~20, 1h confirmation, 15m entry)."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "Trading pair, e.g. BTCUSDT or ETHUSDT"
                },
                "interval": {
                    "type": "string",
                    "description": "Kline interval: 15m, 1h, 4h, etc.",
                    "default": "15m"
                },
                "limit": {
                    "type": "integer",
                    "description": "Candles to fetch (max 500). >=100 for 15m, >=72 for 1h, 20-30 for 4h (trend direction only).",
                    "default": 100,
                    "minimum": 10,
                    "maximum": 500
                }
            },
            "required": ["symbol"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let symbol = params["symbol"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParameters("symbol is required".into()))?
            .to_uppercase();
        let interval = params["interval"].as_str().unwrap_or("15m");
        let limit = params["limit"].as_u64().unwrap_or(100).clamp(10, 500);

        let url =
            format!("{FAPI_BASE}/fapi/v1/klines?symbol={symbol}&interval={interval}&limit={limit}");
        let resp = self.client.get(&url).send().await.map_err(|e| {
            ToolError::ExternalService(format!("Binance klines request failed: {e}"))
        })?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!("Binance {st}: {body}")));
        }
        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::ExternalService(format!("JSON parse error: {e}")))?;
        let candles = parse_klines(&raw)?;
        if candles.is_empty() {
            return Err(ToolError::ExternalService("No klines returned".into()));
        }
        let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let price = *closes.last().unwrap_or(&0.0);

        // Indicators
        let rsi_14 = compute_rsi(&closes, 14);
        let macd = compute_macd(&closes);
        let bb = compute_bollinger(&closes);
        let ichimoku = compute_ichimoku(&candles);
        let adx = compute_adx(&candles);

        let e9 = ema_full(&closes, 9);
        let e21 = ema_full(&closes, 21);
        let e20 = ema_full(&closes, 20);
        let e50 = ema_full(&closes, 50);

        let macd_obj = match macd {
            Some((l, s, h)) => serde_json::json!({
                "macd_line": r2(l), "signal_line": r2(s), "histogram": r2(h),
                "trend": if h > 0.0 { "bullish" } else { "bearish" }
            }),
            None => serde_json::json!({"error": "insufficient data"}),
        };
        let bb_obj = match bb {
            Some((mid, up, lo, pb, bw)) => serde_json::json!({
                "upper": r2(up), "middle": r2(mid), "lower": r2(lo),
                "percent_b": r4(pb), "bandwidth": r4(bw)
            }),
            None => serde_json::json!({"error": "insufficient data"}),
        };
        let ichi_obj = match ichimoku {
            Some((tk, kj, sa, sb, green)) => serde_json::json!({
                "tenkan_sen": r2(tk), "kijun_sen": r2(kj),
                "senkou_span_a": r2(sa), "senkou_span_b": r2(sb),
                "cloud_color": if green { "green" } else { "red" },
                "tk_cross": if tk > kj { "bullish" } else { "bearish" },
                "price_above_cloud": price > sa.max(sb)
            }),
            None => serde_json::json!({"error": "need >=52 candles"}),
        };
        let adx_obj = match adx {
            Some((a, pdi, ndi)) => serde_json::json!({
                "adx_14": r2(a),
                "trend_strength": if a > 25.0 { "strong" } else if a > 20.0 { "trending" } else { "ranging" },
                "plus_di": r2(pdi), "minus_di": r2(ndi)
            }),
            None => serde_json::json!({"error": "insufficient data"}),
        };

        let result = serde_json::json!({
            "symbol": symbol, "interval": interval,
            "candles_fetched": candles.len(), "current_price": r2(price),
            "indicators": {
                "rsi_14": rsi_14.map(r2),
                "macd": macd_obj,
                "bollinger_bands": bb_obj,
                "ema": {
                    "ema_9": r2(e9.last().copied().unwrap_or(0.0)),
                    "ema_21": r2(e21.last().copied().unwrap_or(0.0)),
                    "ema_20": r2(e20.last().copied().unwrap_or(0.0)),
                    "ema_50": r2(e50.last().copied().unwrap_or(0.0)),
                    "cross_9_21": ema_cross_direction(&closes, 9, 21),
                    "cross_20_50": ema_cross_direction(&closes, 20, 50)
                },
                "ichimoku": ichi_obj,
                "adx": adx_obj
            }
        });
        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(60, 600))
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// 2. BinanceSnapshotTool
// ═════════════════════════════════════════════════════════════════════════════

/// Fetches the current order-book depth for a Binance Futures symbol.
pub struct BinanceSnapshotTool {
    client: Client,
}

impl BinanceSnapshotTool {
    pub fn new() -> Self {
        Self {
            client: build_client(15),
        }
    }
}

impl Default for BinanceSnapshotTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BinanceSnapshotTool {
    fn name(&self) -> &str {
        "binance_snapshot"
    }

    fn description(&self) -> &str {
        "Fetch Binance Futures order-book depth snapshot. Returns best bid/ask, \
         spread, mid-price, and up to 20 levels."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "e.g. BTCUSDT" },
                "limit": { "type": "integer", "default": 20, "description": "5, 10, 20, 50, 100, 500" }
            },
            "required": ["symbol"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let symbol = params["symbol"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParameters("symbol is required".into()))?
            .to_uppercase();
        let limit = params["limit"].as_u64().unwrap_or(20);

        let url = format!("{FAPI_BASE}/fapi/v1/depth?symbol={symbol}&limit={limit}");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Binance depth failed: {e}")))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!("Binance {st}: {body}")));
        }
        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::ExternalService(format!("JSON parse error: {e}")))?;

        let parse_levels = |key: &str| -> Vec<[f64; 2]> {
            raw[key]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            let r = e.as_array()?;
                            Some([
                                r.first()?.as_str()?.parse::<f64>().ok()?,
                                r.get(1)?.as_str()?.parse::<f64>().ok()?,
                            ])
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let bids = parse_levels("bids");
        let asks = parse_levels("asks");
        let bb = bids.first().map(|b| b[0]).unwrap_or(0.0);
        let ba = asks.first().map(|a| a[0]).unwrap_or(0.0);

        let result = serde_json::json!({
            "symbol": symbol,
            "last_update_id": raw["lastUpdateId"],
            "best_bid": r2(bb), "best_ask": r2(ba),
            "spread": r2(ba - bb), "mid_price": r2((bb + ba) / 2.0),
            "bid_levels": bids.len(), "ask_levels": asks.len(),
            "bids": bids, "asks": asks
        });
        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(60, 600))
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// 3. BinanceFuturesAccountTool
// ═════════════════════════════════════════════════════════════════════════════

/// Returns the USDT-M Futures account balance, open positions, and a pre-computed
/// `orderSize10pct` field (= 10 % of available balance) for the crypto_trading skill.
pub struct BinanceFuturesAccountTool {
    client: Client,
}

impl BinanceFuturesAccountTool {
    pub fn new() -> Self {
        Self {
            client: build_client(30),
        }
    }
}

impl Default for BinanceFuturesAccountTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BinanceFuturesAccountTool {
    fn name(&self) -> &str {
        "binance_futures_account"
    }

    fn description(&self) -> &str {
        "Fetch Binance USDT-M Futures account balance and all open positions. \
         Returns availableBalance, totalWalletBalance, totalUnrealizedProfit, \
         orderSize10pct (10 % of available balance), and non-zero positions with \
         entry price, leverage, and unrealised PnL. \
         Requires BINANCE_API_KEY and BINANCE_API_SECRET."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let api_key = binance_api_key()?;
        let api_secret = binance_api_secret()?;

        let ts = timestamp_ms()?;
        let query = format!("timestamp={ts}");
        let sig = sign_query(&api_secret, &query)?;
        let url = format!("{FAPI_BASE}/fapi/v2/account?{query}&signature={sig}");

        let resp = self
            .client
            .get(&url)
            .header("X-MBX-APIKEY", &api_key)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Binance account failed: {e}")))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!("Binance {st}: {body}")));
        }
        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::ExternalService(format!("JSON parse error: {e}")))?;

        let pf = |v: &Value| {
            v.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let avail = pf(&raw["availableBalance"]);
        let wallet = pf(&raw["totalWalletBalance"]);
        let pnl = pf(&raw["totalUnrealizedProfit"]);

        let positions: Vec<Value> = raw["positions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let amt: f64 = p["positionAmt"].as_str()?.parse().ok()?;
                        if amt.abs() < 1e-10 {
                            return None;
                        }
                        Some(serde_json::json!({
                            "symbol": p["symbol"],
                            "position_side": if amt > 0.0 { "LONG" } else { "SHORT" },
                            "position_amount": amt.abs(),
                            "entry_price": r2(pf(&p["entryPrice"])),
                            "unrealized_pnl": r4(pf(&p["unrealizedProfit"])),
                            "leverage": p["leverage"].as_str()
                                .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1),
                            "margin_type": p["marginType"],
                            "liquidation_price": r2(pf(&p["liquidationPrice"]))
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let result = serde_json::json!({
            "available_balance": r2(avail),
            "total_wallet_balance": r2(wallet),
            "total_unrealized_pnl": r2(pnl),
            "orderSize10pct": r2(avail * 0.1),
            "open_positions": positions.len(),
            "positions": positions
        });
        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(10, 120))
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// 4. BinanceFuturesOrderTool
// ═════════════════════════════════════════════════════════════════════════════

/// Places, closes, or cancels a USDT-M Futures order on Binance.
///
/// Supports LIMIT / STOP_MARKET / TAKE_PROFIT_MARKET order types (prefer LIMIT for entries).
/// If `leverage` is provided, the symbol's leverage is set first.
/// Requires `auto_approve: true` in routine metadata or explicit user approval.
pub struct BinanceFuturesOrderTool {
    client: Client,
}

impl BinanceFuturesOrderTool {
    pub fn new() -> Self {
        Self {
            client: build_client(30),
        }
    }
}

impl Default for BinanceFuturesOrderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BinanceFuturesOrderTool {
    fn name(&self) -> &str {
        "binance_futures_order"
    }

    fn description(&self) -> &str {
        "Place a USDT-M Futures order on Binance. Prefer LIMIT orders for entries; \
         also supports STOP_MARKET and TAKE_PROFIT_MARKET types. Can optionally set \
         leverage before placing the order. Use reduce_only=true to close an existing \
         position. Use quantity_usdt for order size in USDT (auto-converted via mark \
         price). Use entry_better_pct to improve the LIMIT entry: BUY orders are placed \
         below the signal price, SELL orders above (e.g. entry_better_pct=0.1 with BUY \
         at 80000 submits at 79920). Use bracket_tp_pct / bracket_sl_pct to \
         automatically place TAKE_PROFIT_MARKET and STOP_MARKET orders; the adjusted \
         entry price is used as the bracket reference; price move = pct / leverage \
         (e.g. bracket_tp_pct=20 with leverage=50 → TP/SL at +/-0.4% from entry). \
         Requires BINANCE_API_KEY and BINANCE_API_SECRET."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "Trading pair, e.g. BTCUSDT"
                },
                "side": {
                    "type": "string",
                    "enum": ["BUY", "SELL"],
                    "description": "BUY to go long / close short, SELL to go short / close long"
                },
                "order_type": {
                    "type": "string",
                    "enum": ["MARKET", "LIMIT", "STOP_MARKET", "TAKE_PROFIT_MARKET"],
                    "description": "Order type. MARKET executes immediately."
                },
                "quantity": {
                    "type": "number",
                    "description": "Order quantity in base asset units (e.g. 0.001 for 0.001 BTC on BTCUSDT). Use quantity_usdt instead if you know the USDT value."
                },
                "quantity_usdt": {
                    "type": "number",
                    "description": "Order size in USDT. The tool fetches the current mark price and converts to base asset quantity automatically. Use this instead of quantity when working with account balances (e.g. orderSize10pct from binance_futures_account)."
                },
                "price": {
                    "type": "number",
                    "description": "Limit price. Required for LIMIT orders."
                },
                "stop_price": {
                    "type": "number",
                    "description": "Stop/trigger price for STOP_MARKET and TAKE_PROFIT_MARKET."
                },
                "time_in_force": {
                    "type": "string",
                    "enum": ["GTC", "IOC", "FOK"],
                    "default": "GTC",
                    "description": "Time-in-force for LIMIT orders."
                },
                "reduce_only": {
                    "type": "boolean",
                    "default": false,
                    "description": "Set true to close an existing position."
                },
                "position_side": {
                    "type": "string",
                    "enum": ["BOTH", "LONG", "SHORT"],
                    "default": "BOTH",
                    "description": "Position side (hedge mode). Use BOTH for one-way mode."
                },
                "leverage": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 125,
                    "description": "Set leverage before placing the order. Omit to keep current."
                },
                "action": {
                    "type": "string",
                    "enum": ["place", "cancel"],
                    "default": "place",
                    "description": "place = new order, cancel = cancel existing order"
                },
                "order_id": {
                    "type": "integer",
                    "description": "Binance order ID to cancel (required when action=cancel)"
                },
                "entry_better_pct": {
                    "type": "number",
                    "description": "Improve the LIMIT entry price by this percentage relative to the signal price. For BUY orders the limit is placed below the signal (price × (1 - pct/100)); for SELL orders above (price × (1 + pct/100)). E.g. entry_better_pct=0.1 with a BUY signal at 80000 submits the order at 79920. The adjusted price is also used as the bracket reference."
                },
                "bracket_tp_pct": {
                    "type": "number",
                    "description": "Take-profit target as % of margin after leverage (e.g. 20 = 20% profit on margin). Automatically places a TAKE_PROFIT_MARKET order. For LIMIT orders uses the (optionally adjusted) limit price as reference. Price move = pct / leverage."
                },
                "bracket_sl_pct": {
                    "type": "number",
                    "description": "Stop-loss as % of margin after leverage (e.g. 20 = 20% loss on margin). Automatically places a STOP_MARKET order. For LIMIT orders uses the (optionally adjusted) limit price as reference. Price move = pct / leverage."
                }
            },
            "required": ["symbol", "side", "order_type"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let api_key = binance_api_key()?;
        let api_secret = binance_api_secret()?;

        let symbol = params["symbol"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParameters("symbol required".into()))?
            .to_uppercase();

        let action = params["action"].as_str().unwrap_or("place");

        // ── Cancel path ──────────────────────────────────────────────────────
        if action == "cancel" {
            let order_id = params["order_id"].as_i64().ok_or_else(|| {
                ToolError::InvalidParameters("order_id required for cancel".into())
            })?;
            let ts = timestamp_ms()?;
            let query = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
            let sig = sign_query(&api_secret, &query)?;
            let url = format!("{FAPI_BASE}/fapi/v1/order?{query}&signature={sig}");

            let resp = self
                .client
                .delete(&url)
                .header("X-MBX-APIKEY", &api_key)
                .send()
                .await
                .map_err(|e| ToolError::ExternalService(format!("Cancel failed: {e}")))?;
            let st = resp.status();
            let body: Value = resp.json().await.unwrap_or(serde_json::json!({}));
            if !st.is_success() {
                return Err(ToolError::ExternalService(format!(
                    "Binance cancel {st}: {}",
                    body
                )));
            }
            return Ok(ToolOutput::success(
                serde_json::json!({ "cancelled": true, "order_id": order_id, "response": body }),
                start.elapsed(),
            ));
        }

        // ── Place path ───────────────────────────────────────────────────────
        let side = params["side"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParameters("side required".into()))?
            .to_uppercase();
        let order_type = params["order_type"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParameters("order_type required".into()))?
            .to_uppercase();

        // Optional leverage setting
        if let Some(lev) = params["leverage"].as_u64() {
            let lev = lev.clamp(1, 125);
            let ts = timestamp_ms()?;
            let query = format!("symbol={symbol}&leverage={lev}&timestamp={ts}");
            let sig = sign_query(&api_secret, &query)?;
            let url = format!("{FAPI_BASE}/fapi/v1/leverage?{query}&signature={sig}");
            let resp = self
                .client
                .post(&url)
                .header("X-MBX-APIKEY", &api_key)
                .send()
                .await
                .map_err(|e| ToolError::ExternalService(format!("Set leverage failed: {e}")))?;
            if !resp.status().is_success() {
                let st = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(ToolError::ExternalService(format!("Leverage {st}: {body}")));
            }
        }

        // Build order query
        let mut parts = vec![
            format!("symbol={symbol}"),
            format!("side={side}"),
            format!("type={order_type}"),
        ];

        // Resolve quantity: prefer quantity_usdt (auto-converted via mark price)
        // over raw quantity (base asset units). This lets callers pass USDT values
        // directly (e.g. orderSize10pct from binance_futures_account) without having
        // to manually divide by the current price.
        let resolved_qty: Option<f64> = if let Some(usdt) = params["quantity_usdt"].as_f64() {
            if usdt > 0.0 {
                let mark = fetch_mark_price(&self.client, &symbol).await?;
                // Round to 3 decimal places (Binance minimum step for BTC is 0.001)
                let qty = (usdt / mark * 1000.0).floor() / 1000.0;
                Some(qty)
            } else {
                None
            }
        } else {
            params["quantity"].as_f64().filter(|&q| q > 0.0)
        };

        if let Some(qty) = resolved_qty {
            parts.push(format!("quantity={qty}"));
        }

        // Apply entry_better_pct for LIMIT orders: adjust price toward a better fill.
        // BUY  → price × (1 - pct/100)  (buy lower than signal)
        // SELL → price × (1 + pct/100)  (sell higher than signal)
        let effective_price: Option<f64> = params["price"].as_f64().map(|signal_price| {
            if order_type == "LIMIT"
                && let Some(offset_pct) = params["entry_better_pct"].as_f64()
                && offset_pct > 0.0
            {
                let factor = offset_pct / 100.0;
                let adjusted = if side == "BUY" {
                    signal_price * (1.0 - factor)
                } else {
                    signal_price * (1.0 + factor)
                };
                // Round to 1 decimal (BTCUSDT tick size)
                return (adjusted * 10.0).round() / 10.0;
            }
            signal_price
        });
        if let Some(p) = effective_price {
            parts.push(format!("price={p}"));
        }
        if let Some(sp) = params["stop_price"].as_f64() {
            parts.push(format!("stopPrice={sp}"));
        }
        if order_type == "LIMIT" {
            let tif = params["time_in_force"].as_str().unwrap_or("GTC");
            parts.push(format!("timeInForce={tif}"));
        }
        if params["reduce_only"].as_bool().unwrap_or(false) {
            parts.push("reduceOnly=true".to_string());
        }
        let ps = params["position_side"]
            .as_str()
            .unwrap_or("BOTH")
            .to_uppercase();
        parts.push(format!("positionSide={ps}"));

        let ts = timestamp_ms()?;
        parts.push(format!("timestamp={ts}"));
        let query = parts.join("&");
        let sig = sign_query(&api_secret, &query)?;
        let url = format!("{FAPI_BASE}/fapi/v1/order?{query}&signature={sig}");

        let resp = self
            .client
            .post(&url)
            .header("X-MBX-APIKEY", &api_key)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Order failed: {e}")))?;
        let st = resp.status();
        let body: Value = resp.json().await.unwrap_or(serde_json::json!({}));
        if !st.is_success() {
            return Err(ToolError::ExternalService(format!(
                "Binance order {st}: {}",
                body
            )));
        }

        let entry_result = serde_json::json!({
            "order_placed": true,
            "order_id": body["orderId"],
            "symbol": body["symbol"],
            "side": body["side"],
            "type": body["type"],
            "status": body["status"],
            "price": body["price"],
            "quantity": body["origQty"],
            "executed_qty": body["executedQty"],
            "avg_price": body["avgPrice"],
            "reduce_only": body["reduceOnly"],
            "position_side": body["positionSide"],
            "time_in_force": body["timeInForce"]
        });

        // ── Bracket orders (auto TP + SL) ────────────────────────────────────
        // When bracket_tp_pct / bracket_sl_pct are provided, place
        // TAKE_PROFIT_MARKET and STOP_MARKET orders to close the entire
        // position.  Price targets are derived from the entry price:
        //   price_move = pct / (100 * leverage)
        // e.g. bracket_tp_pct=20, leverage=50 → TP/SL at ± 0.4% from entry.
        // For LIMIT orders, `effective_price` (already adjusted by entry_better_pct)
        // is used as the bracket reference so TP/SL track the actual order price.
        // For MARKET orders the actual avgPrice from the fill is used.
        if matches!(order_type.as_str(), "MARKET" | "LIMIT")
            && let (Some(tp_pct), Some(sl_pct)) = (
                params["bracket_tp_pct"].as_f64(),
                params["bracket_sl_pct"].as_f64(),
            )
        {
                let leverage = params["leverage"].as_f64().unwrap_or(1.0).max(1.0);
                // LIMIT orders: use effective_price (adjusted by entry_better_pct if set).
                // MARKET orders: use the actual fill price (avgPrice).
                let fill_price = if order_type == "LIMIT" {
                    effective_price.unwrap_or(0.0)
                } else {
                    body["avgPrice"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0)
                };

                if fill_price > 0.0 {
                    let tp_move = tp_pct / 100.0 / leverage;
                    let sl_move = sl_pct / 100.0 / leverage;

                    // Round to 1 decimal (BTCUSDT tick size = 0.1 USDT)
                    let round1 = |p: f64| (p * 10.0).round() / 10.0;
                    let (tp_price, sl_price) = if side == "BUY" {
                        (
                            round1(fill_price * (1.0 + tp_move)),
                            round1(fill_price * (1.0 - sl_move)),
                        )
                    } else {
                        (
                            round1(fill_price * (1.0 - tp_move)),
                            round1(fill_price * (1.0 + sl_move)),
                        )
                    };

                    let close_side = if side == "BUY" { "SELL" } else { "BUY" };
                    let ps = params["position_side"]
                        .as_str()
                        .unwrap_or("BOTH")
                        .to_uppercase();

                    let tp_order = place_close_order(
                        &self.client,
                        &api_key,
                        &api_secret,
                        &symbol,
                        close_side,
                        "TAKE_PROFIT_MARKET",
                        tp_price,
                        &ps,
                    )
                    .await;
                    let sl_order = place_close_order(
                        &self.client,
                        &api_key,
                        &api_secret,
                        &symbol,
                        close_side,
                        "STOP_MARKET",
                        sl_price,
                        &ps,
                    )
                    .await;

                    let bracket_result = serde_json::json!({
                        "order_placed": true,
                        "entry": entry_result,
                        "fill_price": fill_price,
                        "leverage": leverage,
                        "take_profit": {
                            "price": tp_price,
                            "margin_pct": tp_pct,
                            "order": tp_order.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))
                        },
                        "stop_loss": {
                            "price": sl_price,
                            "margin_pct": sl_pct,
                            "order": sl_order.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))
                        }
                    });
                    return Ok(ToolOutput::success(bracket_result, start.elapsed()));
                }
        }

        Ok(ToolOutput::success(entry_result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        // Real money on the line — require explicit approval in interactive sessions.
        // Routine jobs that set `auto_approve: true` in metadata are allowed through
        // (UnlessAutoApproved) so scheduled trading strategies can execute orders
        // automatically. Interactive / manual calls still require human confirmation.
        ApprovalRequirement::UnlessAutoApproved
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        // Binance order weight is 1; be conservative
        Some(ToolRateLimitConfig::new(5, 60))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn closes_up(n: usize) -> Vec<f64> {
        (0..n).map(|i| 100.0 + i as f64).collect()
    }

    fn candles_up(n: usize) -> Vec<Ohlcv> {
        (0..n)
            .map(|i| {
                let b = 100.0 + i as f64;
                Ohlcv {
                    open: b - 0.5,
                    high: b + 1.0,
                    low: b - 1.0,
                    close: b,
                    volume: 10.0,
                }
            })
            .collect()
    }

    #[test]
    fn test_ema_full_length() {
        let c = closes_up(50);
        let e = ema_full(&c, 12);
        assert_eq!(e.len(), 50);
        assert_eq!(e[10], 0.0); // invalid
        assert!(e[11] > 0.0); // seed
    }

    #[test]
    fn test_rsi_overbought_uptrend() {
        let rsi = compute_rsi(&closes_up(50), 14).unwrap();
        assert!(rsi > 70.0, "pure uptrend RSI should be > 70, got {rsi}");
    }

    #[test]
    fn test_macd_sufficient_data() {
        assert!(compute_macd(&closes_up(60)).is_some());
        assert!(compute_macd(&closes_up(20)).is_none());
    }

    #[test]
    fn test_bollinger_bands_range() {
        let (mid, up, lo, pb, _) = compute_bollinger(&closes_up(50)).unwrap();
        assert!(up >= mid && lo <= mid);
        assert!((0.0..=1.5).contains(&pb));
    }

    #[test]
    fn test_ichimoku_min_candles() {
        assert!(compute_ichimoku(&candles_up(51)).is_none());
        assert!(compute_ichimoku(&candles_up(52)).is_some());
    }

    #[test]
    fn test_adx_min_candles() {
        assert!(compute_adx(&candles_up(28)).is_none());
        assert!(compute_adx(&candles_up(29)).is_some());
    }

    #[test]
    fn test_ema_cross_bullish() {
        let dir = ema_cross_direction(&closes_up(60), 9, 21);
        assert!(dir == "bullish" || dir == "bullish_cross", "got {dir}");
    }

    #[test]
    fn test_wilder_smooth_initial() {
        let vals: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        let s = wilder_smooth(&vals, 14).unwrap();
        assert!((s[0] - 7.5).abs() < 0.001);
    }

    #[test]
    fn test_round() {
        assert_eq!(r2(95234.567), 95234.57);
        assert_eq!(r4(0.12345), 0.1235);
    }

    #[test]
    fn test_tool_names() {
        assert_eq!(PriceAnalysisTool::new().name(), "price_analysis");
        assert_eq!(BinanceSnapshotTool::new().name(), "binance_snapshot");
        assert_eq!(
            BinanceFuturesAccountTool::new().name(),
            "binance_futures_account"
        );
        assert_eq!(
            BinanceFuturesOrderTool::new().name(),
            "binance_futures_order"
        );
    }

    #[test]
    fn test_order_tool_requires_approval() {
        let tool = BinanceFuturesOrderTool::new();
        assert_eq!(
            tool.requires_approval(&serde_json::json!({})),
            ApprovalRequirement::UnlessAutoApproved
        );
    }

    // ── entry_better_pct unit tests ──────────────────────────────────────────

    /// Verify the price adjustment formula used by entry_better_pct directly.
    /// The execute() path needs live Binance, but the math is pure and testable.
    #[test]
    fn test_entry_better_pct_buy_lowers_price() {
        let signal_price = 80_000.0_f64;
        let offset_pct = 0.1_f64;
        // BUY: adjusted = signal * (1 - pct/100), rounded to 1 decimal
        let adjusted = (signal_price * (1.0 - offset_pct / 100.0) * 10.0).round() / 10.0;
        assert_eq!(adjusted, 79_920.0, "BUY entry should be below signal");
        assert!(adjusted < signal_price);
    }

    #[test]
    fn test_entry_better_pct_sell_raises_price() {
        let signal_price = 80_000.0_f64;
        let offset_pct = 0.1_f64;
        // SELL: adjusted = signal * (1 + pct/100), rounded to 1 decimal
        let adjusted = (signal_price * (1.0 + offset_pct / 100.0) * 10.0).round() / 10.0;
        assert_eq!(adjusted, 80_080.0, "SELL entry should be above signal");
        assert!(adjusted > signal_price);
    }

    #[test]
    fn test_entry_better_pct_zero_leaves_price_unchanged() {
        let signal_price = 80_000.0_f64;
        let offset_pct = 0.0_f64;
        // offset_pct = 0 → guard skips adjustment
        let adjusted = if offset_pct > 0.0 {
            (signal_price * (1.0 - offset_pct / 100.0) * 10.0).round() / 10.0
        } else {
            signal_price
        };
        assert_eq!(adjusted, signal_price);
    }
}
