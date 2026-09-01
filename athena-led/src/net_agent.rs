// ==========================================
// 🛰️ net_agent.rs — 后台网络数据代理 (修复版)
//
// 本次修复项:
//   ✅ 移除 futures crate 依赖，改用 tokio 自带的 tokio::join!
//   ✅ Backoffs 结构体 + RefreshKind 枚举提到"模块顶部"
//      (之前在 run() 函数内部定义, impl 方法签名看不到类型, 报 3× E0425)
//   ✅ try_refresh_all 原写法: futures::future::join_all([async {...}, async {...}])
//      数组内 4 个 async block 都是独有的匿名类型 → E0308
//      现在改成 tokio::join!(a,b,c,d), 4 个独立变量再 vec! 收
//   ✅ 顺便修了一个 BUG: 第二个 async block 把 weather 结果误传进了 ip_result
// ==========================================

use crate::Args;
use anyhow::{Context, Result};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// ==========================================
// 📦 各类 API JSON 结构体 (照抄原来的, 没改)
// ==========================================
#[derive(Deserialize, Debug)]
struct WeatherResponse {
    weather: String,
    temperature: f64,
    #[serde(default)]
    temp_max: Option<f64>,
    #[serde(default)]
    temp_min: Option<f64>,
}
#[derive(Deserialize, Debug)]
struct SeniverseResponse { results: Vec<SeniverseResult> }
#[derive(Deserialize, Debug)]
struct SeniverseResult   { daily: Vec<SeniverseDaily> }
#[derive(Deserialize, Debug)]
struct SeniverseDaily    { high: String; low: String; code_day: String }
#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrResult {
    current_condition: Vec<WttrCurrent>,
    weather: Vec<WttrDaily>,
}
#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrCurrent { temp_C: String; weatherDesc: Vec<WttrValue> }
#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrDaily   { maxtempC: String; mintempC: String }
#[derive(Deserialize, Debug)]
struct WttrValue   { value: String }
#[derive(Deserialize, Debug)]
struct OmGeoResponse   { results: Option<Vec<OmLocation>> }
#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct OmLocation { name: String; latitude: f64; longitude: f64 }
#[derive(Deserialize, Debug)]
struct OmWeatherResponse {
    current_weather: OmCurrentWeather,
    #[serde(default)]
    daily: Option<OmDaily>,
}
#[derive(Deserialize, Debug)]
struct OmCurrentWeather { temperature: f64; weathercode: u8 }
#[derive(Deserialize, Debug)]
struct OmDaily {
    #[serde(default)]
    temperature_2m_max: Vec<f64>,
    #[serde(default)]
    temperature_2m_min: Vec<f64>,
}

// ==========================================
// 📸 共享快照 (后台写, 渲染读)
// ==========================================
#[derive(Debug, Clone)]
pub struct NetSnapshot {
    pub weather: String,
    pub ip: String,
    pub stock_text: String,
    pub http_text: String,
    pub pings: HashMap<String, String>,
    pub sun: String,
}
impl Default for NetSnapshot {
    fn default() -> Self {
        Self {
            weather: "Wait...".to_string(),
            ip: "IP:Wait".to_string(),
            stock_text: String::new(),
            http_text: String::new(),
            pings: HashMap::new(),
            sun: "SUN:--".to_string(),
        }
    }
}

// ==========================================
// 🔁 失败退避计数器 (每类独立)
// ⚠️ 必须放在模块顶部，force_refresh_all / try_refresh_all / apply_results
//    三个 impl 方法才能引用到类型（之前放在 run() 函数内部 → E0425 ×3）
// ==========================================
#[derive(Default, Clone, Copy)]
pub struct Backoffs {
    pub weather: u32,
    pub ip: u32,
    pub stock: u32,
    pub http: u32,
}
#[derive(Clone, Copy, Debug)]
pub enum RefreshKind { Ip, Weather, Stock, Http }

// ==========================================
// ⚙️ 启动参数 (由 main.rs 组装后传入)
// ==========================================
pub struct AgentCfg {
    pub cache_secs: u64,
    pub weather_city: String,
    pub weather_source: String,
    pub seniverse_key: String,
    pub ip_url: String,
    pub custom_http_url: String,
    pub http_length: usize,
    pub stock_url: String,
    pub want_weather: bool,
    pub want_ip: bool,
    pub want_stock: bool,
    pub want_http: bool,
}

impl AgentCfg {
    pub fn from_args(args: &Args) -> Self {
        let mut want_weather = false;
        let mut want_ip = false;
        let mut want_http = false;
        let mut want_stock = false;
        for p in &args.profile {
            for token in p.split_whitespace() {
                let name = token.split('#').next().unwrap_or("").split(':').next().unwrap_or("");
                match name {
                    "weather" => want_weather = true,
                    "ip"      => want_ip = true,
                    "http_custom" => want_http = true,
                    "stock"   => want_stock = true,
                    _ => {}
                }
            }
        }
        Self {
            cache_secs: if args.http_cache_secs < 10 { 10 } else { args.http_cache_secs },
            weather_city:   args.weather_city.clone(),
            weather_source: args.weather_source.clone(),
            seniverse_key:  args.seniverse_key.clone(),
            ip_url:         args.ip_url.clone(),
            custom_http_url: args.custom_http_url.clone(),
            http_length:    args.http_length,
            stock_url:      args.stock_url.clone(),
            want_weather, want_ip, want_stock, want_http,
        }
    }
}

// ==========================================
// 🧠 后台代理
// ==========================================
pub struct NetAgent {
    snapshot: Arc<RwLock<NetSnapshot>>,
    cfg: AgentCfg,
    client: Client,
    last_stock_price: f64,
    cached_weather_text: String,
    cached_weather_time: Instant,
    cached_ip_text: String,
    cached_ip_time: Instant,
    cached_http_text: String,
    cached_http_time: Instant,
}

impl NetAgent {
    pub fn new(cfg: AgentCfg) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Athena-LED Router)")
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Ok(Self {
            snapshot: Arc::new(RwLock::new(NetSnapshot::default())),
            cfg,
            client,
            last_stock_price: 0.0,
            cached_weather_text: "Wait...".to_string(),
            cached_weather_time: Instant::now(),
            cached_ip_text: "IP:Err".to_string(),
            cached_ip_time: Instant::now(),
            cached_http_text: String::new(),
            cached_http_time: Instant::now(),
        })
    }

    pub fn snapshot(&self) -> Arc<RwLock<NetSnapshot>> {
        Arc::clone(&self.snapshot)
    }

    // 🚀 主循环 (外部: tokio::spawn(async move { agent.run(running).await });)
    pub async fn run(self, running: Arc<AtomicBool>) {
        let Self { snapshot, cfg, client, .. } = self;
        let mut agent = NetAgentInner {
            snapshot, cfg, client,
            backoff: Backoffs::default(),
            last_stock_price: 0.0,
            cached_weather_text: "Wait...".into(),
            cached_weather_time: Instant::now(),
            cached_ip_text: "IP:Err".into(),
            cached_ip_time: Instant::now(),
            cached_http_text: String::new(),
            cached_http_time: Instant::now(),
        };
        // 启动立刻刷一次（不等 cache_secs 秒）
        agent.force_refresh_all().await;
        loop {
            if !running.load(Ordering::Relaxed) { break }
            agent.tick().await;
            // 正常等待 cache_secs；有任何连续失败时缩短(快速重试)最多 min(30, cache_secs)
            let shortest = agent
                .backoff_sleep_secs()
                .unwrap_or(agent.cfg.cache_secs);
            let sleep = std::cmp::min(shortest, agent.cfg.cache_secs.max(10));
            // 分片 sleep, 让 running 旗标能被及时响应
            let slices = (sleep / 2).max(1);
            for _ in 0..slices {
                if !running.load(Ordering::Relaxed) { break }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

struct NetAgentInner {
    snapshot: Arc<RwLock<NetSnapshot>>,
    cfg: AgentCfg,
    client: Client,
    backoff: Backoffs,
    last_stock_price: f64,
    cached_weather_text: String,
    cached_weather_time: Instant,
    cached_ip_text: String,
    cached_ip_time: Instant,
    cached_http_text: String,
    cached_http_time: Instant,
}

impl NetAgentInner {
    fn backoff_sleep_secs(&self) -> Option<u64> {
        // 有连续失败项 → 缩短为 30 秒尽快重试
        let max_b = [
            self.backoff.weather, self.backoff.ip, self.backoff.stock, self.backoff.http
        ].into_iter().max().unwrap_or(0);
        if max_b > 0 { Some(30u64.saturating_mul(1u64 << (max_b.min(3)))) } else { None }
    }

    async fn tick(&mut self) {
        // 根据 backoff 决定本次是否跳过某类 (指数退避 1x/2x/4x/8x)
        // 正常到达 cache_secs 周期则刷新；否则跳过
        let try_weather = self.cfg.want_weather && self.weather_should_try();
        let try_ip      = self.cfg.want_ip      && self.ip_should_try();
        let try_stock   = self.cfg.want_stock   && self.stock_should_try();
        let try_http    = self.cfg.want_http    && self.http_should_try();

        if try_weather || try_ip || try_stock || try_http {
            self.try_refresh_all(try_ip, try_weather, try_stock, try_http).await;
        }
    }

    fn weather_should_try(&self) -> bool {
        if self.backoff.weather == 0 {
            self.cached_weather_time.elapsed().as_secs() >= self.cfg.cache_secs
        } else {
            let mult = 1u64.saturating_shl(self.backoff.weather.min(3) as u32);
            self.cached_weather_time.elapsed().as_secs() >= self.cfg.cache_secs.saturating_mul(mult)
        }
    }
    fn ip_should_try(&self) -> bool {
        if self.backoff.ip == 0 {
            self.cached_ip_time.elapsed().as_secs() >= self.cfg.cache_secs
        } else {
            let mult = 1u64.saturating_shl(self.backoff.ip.min(3) as u32);
            self.cached_ip_time.elapsed().as_secs() >= self.cfg.cache_secs.saturating_mul(mult)
        }
    }
    fn stock_should_try(&self) -> bool {
        if self.backoff.stock == 0 { true } else {
            self.cfg.cache_secs.saturating_mul(1u64.saturating_shl(self.backoff.stock.min(3) as u32))
                <= std::u64::MAX // 始终允许, 这里只做示例
        }
    }
    fn http_should_try(&self) -> bool {
        if self.backoff.http == 0 {
            self.cached_http_time.elapsed().as_secs() >= self.cfg.cache_secs
        } else {
            let mult = 1u64.saturating_shl(self.backoff.http.min(3) as u32);
            self.cached_http_time.elapsed().as_secs() >= self.cfg.cache_secs.saturating_mul(mult)
        }
    }

    // ==========================================
    // 🔁 force_refresh_all — 启动立刻刷 4 类
    // (修复: futures::future::join4 → tokio::join! ; 无需 futures crate)
    // ==========================================
    async fn force_refresh_all(&mut self) {
        let fut_ip      = async { (RefreshKind::Ip,      self.refresh_ip().await) };
        let fut_weather = async { (RefreshKind::Weather, self.refresh_weather().await) };
        let fut_stock   = async { (RefreshKind::Stock,   self.refresh_stock().await) };
        let fut_http    = async { (RefreshKind::Http,    self.refresh_http().await) };

        // ✅ 修复前: futures::future::join4(...) → 找不到 crate futures
        // ✅ 修复后: tokio::join! (Cargo 里已有 tokio, 零新增依赖)
        let (r_ip, r_weather, r_stock, r_http) = tokio::join!(fut_ip, fut_weather, fut_stock, fut_http);
        let results: Vec<(String, RefreshKind)> = vec![r_ip, r_weather, r_stock, r_http];
        self.apply_results(results);
    }

    // ==========================================
    // 🔁 try_refresh_all — 按退避策略选择性刷新
    // (修复: join_all([async{}, async{}]) → 4 个 async 类型不同 E0308, 用 tokio::join! 拆开)
    // ==========================================
    async fn try_refresh_all(&mut self, wip: bool, ww: bool, ws: bool, wh: bool) {
        let (r_ip, r_weather, r_stock, r_http) = tokio::join!(
            async { if wip      { (RefreshKind::Ip,      self.refresh_ip().await)      } else { (RefreshKind::Ip,      String::new()) } },
            async { if ww       { (RefreshKind::Weather, self.refresh_weather().await) } else { (RefreshKind::Weather, String::new()) } },
            // ✅ 顺手修 BUG: 之前写的是 Self::ip_result(self.refresh_weather().await)
            //    → 把天气结果传进了 IP_result, 现在用 RefreshKind::Weather 正确对应
            async { if ws       { (RefreshKind::Stock,   self.refresh_stock().await)   } else { (RefreshKind::Stock,   String::new()) } },
            async { if wh       { (RefreshKind::Http,    self.refresh_http().await)    } else { (RefreshKind::Http,    String::new()) } },
        );
        let mut results: Vec<(String, RefreshKind)> = Vec::with_capacity(4);
        if wip { results.push(r_ip); }
        if ww  { results.push(r_weather); }
        if ws  { results.push(r_stock); }
        if wh  { results.push(r_http); }
        self.apply_results(results);
    }

    fn apply_results(&mut self, results: Vec<(String, RefreshKind)>) {
        let mut snap = match self.snapshot.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        for (text, kind) in results {
            let good = match kind {
                RefreshKind::Ip => {
                    let ok = !text.is_empty() && !text.contains("Err") && !text.contains("Wait");
                    if ok {
                        snap.ip = text.clone();
                        self.cached_ip_text = text;
                        self.cached_ip_time = Instant::now();
                    }
                    ok
                }
                RefreshKind::Weather => {
                    let ok = !text.is_empty() && !text.starts_with("W:") && !text.contains("Wait");
                    if ok {
                        snap.weather = text.clone();
                        self.cached_weather_text = text;
                        self.cached_weather_time = Instant::now();
                    }
                    ok
                }
                RefreshKind::Stock => {
                    let ok = !text.is_empty() && !text.contains("Err");
                    if ok {
                        snap.stock_text = text.clone();
                    }
                    ok
                }
                RefreshKind::Http => {
                    let ok = !text.contains("Err") && !text.contains("Wait");
                    if ok {
                        snap.http_text = text.clone();
                        self.cached_http_text = text;
                        self.cached_http_time = Instant::now();
                    }
                    ok
                }
            };
            // 成功 → 清零退避；失败 → +1 (最大 3, 对应 8x)
            let slot = match kind {
                RefreshKind::Ip      => &mut self.backoff.ip,
                RefreshKind::Weather => &mut self.backoff.weather,
                RefreshKind::Stock   => &mut self.backoff.stock,
                RefreshKind::Http    => &mut self.backoff.http,
            };
            if good { *slot = 0; } else { *slot = (*slot + 1).min(3); }
        }
    }

    // ---------- 4 个刷新函数 (简化版, 参考 main 项目里的实现逻辑) ----------

    async fn refresh_ip(&mut self) -> String {
        // 先用缓存 (IP 1h 天然不怎么变)
        if self.cached_ip_time.elapsed().as_secs() < 3600
            && !self.cached_ip_text.contains("Err") && !self.cached_ip_text.is_empty()
        {
            return self.cached_ip_text.clone();
        }
        match self.client.get(&self.cfg.ip_url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(text) => {
                    let re = Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();
                    let out = re.find(&text).map(|m| format!("IP:{}", m.as_str())).unwrap_or_else(|| "IP:Err".into());
                    if !out.contains("Err") {
                        self.cached_ip_text = out.clone();
                        self.cached_ip_time = Instant::now();
                    }
                    out
                }
                Err(_) => "IP:Err".to_string(),
            },
            Err(_) => "IP:NetErr".to_string(),
        }
    }

    async fn refresh_weather(&mut self) -> String {
        let city = if self.cfg.weather_city.is_empty() { "北京".into() } else { self.cfg.weather_city.clone() };
        let src  = if self.cfg.weather_source.is_empty() { "uapis" } else { &self.cfg.weather_source };

        let result = match src {
            "uapis" => self.weather_uapis(&city).await,
            "wttr"  => self.weather_wttr(&city).await,
            "seniverse" => self.weather_seniverse(&city).await,
            "openmeteo" => self.weather_openmeteo(&city).await,
            _ => self.weather_uapis(&city).await,
        };
        let good = !result.starts_with("W:") && !result.contains("Wait");
        if good {
            self.cached_weather_text = result.clone();
            self.cached_weather_time = Instant::now();
        }
        result
    }

    async fn weather_uapis(&self, city: &str) -> String {
        let url = format!("https://uapis.cn/api/v1/misc/weather?city={}&forecast=true", city);
        match self.client.get(&url).send().await {
            Ok(r) => match r.json::<WeatherResponse>().await {
                Ok(d) => {
                    let t = d.temperature;
                    let h = d.temp_max.unwrap_or(t);
                    let l = d.temp_min.unwrap_or(t);
                    let icon = if d.weather.contains("雨") { "☂" }
                        else if d.weather.contains("雪") { "❄" }
                        else if d.weather.contains("云")||d.weather.contains("阴")||d.weather.contains("雾")||d.weather.contains("霾") { "☁" }
                        else { "☀" };
                    format!("{} {:.0}℃ {:.0}-{:.0}", icon, t, l, h)
                }
                Err(_) => "W:JsonErr".into(),
            },
            Err(_) => "W:NetErr".into(),
        }
    }

    async fn weather_wttr(&self, city: &str) -> String {
        match self.client.get(format!("https://wttr.in/{}?format=j1", city)).send().await {
            Ok(r) => match r.json::<WttrResult>().await {
                Ok(j) => {
                    let curr = j.current_condition.first();
                    let day = j.weather.first();
                    match (curr, day) {
                        (Some(c), Some(d)) => {
                            let desc = c.weatherDesc.first().map(|x| x.value.to_lowercase()).unwrap_or_default();
                            let icon = if desc.contains("rain")||desc.contains("shower")||desc.contains("drizzle") { "☂" }
                                else if desc.contains("snow")||desc.contains("ice")||desc.contains("hail") { "❄" }
                                else if desc.contains("thunder") { "⚡" }
                                else if desc.contains("cloud")||desc.contains("overcast") { "☁" }
                                else if desc.contains("mist")||desc.contains("fog") { "🌫" }
                                else { "☀" };
                            format!("{} {}℃ {}-{}", icon, c.temp_C, d.mintempC, d.maxtempC)
                        }
                        _ => "W:DataErr".into(),
                    }
                }
                Err(_) => "W:JsonErr".into(),
            },
            Err(_) => "W:NetErr".into(),
        }
    }

    async fn weather_seniverse(&self, city: &str) -> String {
        if self.cfg.seniverse_key.trim().is_empty() { return "W:NoKey".into(); }
        let url = format!(
            "https://api.seniverse.com/v3/weather/daily.json?key={}&location={}&language=en&unit=c&start=0&days=1",
            self.cfg.seniverse_key, city
        );
        match self.client.get(&url).send().await {
            Ok(r) => match r.json::<SeniverseResponse>().await {
                Ok(j) => {
                    if let Some(d) = j.results.into_iter().next().and_then(|r| r.daily.into_iter().next()) {
                        let h: f64 = d.high.parse().unwrap_or(0.0);
                        let l: f64 = d.low.parse().unwrap_or(0.0);
                        let t = (h + l) / 2.0;
                        let code: i32 = d.code_day.parse().unwrap_or(99);
                        let icon = match code {
                            0..=3 => "☀", 4..=9 => "☁", 10..=19 => "☂", 20..=29 => "❄", 30..=36 => "☁", _ => "☀",
                        };
                        return format!("{} {:.0}℃ {:.0}-{:.0}", icon, t, l, h);
                    }
                    "W:DataErr".into()
                }
                Err(_) => "W:JsonErr".into(),
            },
            Err(_) => "W:NetErr".into(),
        }
    }

    async fn weather_openmeteo(&self, city: &str) -> String {
        let geo = format!(
            "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh&format=json",
            city
        );
        let (lat, lon) = match self.client.get(&geo).send().await {
            Ok(r) => match r.json::<OmGeoResponse>().await {
                Ok(g) => match g.results.and_then(|v| v.into_iter().next()) {
                    Some(l) => (l.latitude, l.longitude),
                    None => return "W:NoCity".into(),
                },
                Err(_) => return "W:GeoJson".into(),
            },
            Err(_) => return "W:GeoNet".into(),
        };
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current_weather=true&daily=temperature_2m_max,temperature_2m_min&forecast_days=1&timezone=auto",
            lat, lon
        );
        match self.client.get(&url).send().await {
            Ok(r) => match r.json::<OmWeatherResponse>().await {
                Ok(d) => {
                    let t = d.current_weather.temperature;
                    let icon = match d.current_weather.weathercode {
                        0 => "☀", 1|2|3 => "☁", 45|48 => "🌫",
                        51..=67 | 80..=82 => "☂", 71..=77 | 85..=86 => "❄", 95..=99 => "⚡",
                        _ => "☁",
                    };
                    if let Some(dd) = d.daily {
                        let h = dd.temperature_2m_max.first().copied().unwrap_or(t);
                        let l = dd.temperature_2m_min.first().copied().unwrap_or(t);
                        return format!("{} {:.0}℃ {:.0}-{:.0}", icon, t, l, h);
                    }
                    format!("{} {:.1}℃", icon, t)
                }
                Err(_) => "W:JsonErr".into(),
            },
            Err(_) => "W:NetErr".into(),
        }
    }

    async fn refresh_stock(&mut self) -> String {
        if self.cfg.stock_url.is_empty() { return String::new(); }
        match self.client.get(&self.cfg.stock_url).send().await {
            Ok(r) => match r.json::<Value>().await {
                Ok(v) => {
                    let price = v["price"].as_f64()
                        .or_else(|| v["price"].as_str().and_then(|s| s.parse().ok()))
                        .or_else(|| v["last"].as_f64())
                        .or_else(|| v["close"].as_f64());
                    if let Some(p) = price {
                        let txt = if p > 1000.0 { format!("{:.0}", p) } else { format!("{:.2}", p) };
                        self.last_stock_price = p;
                        return txt;
                    }
                    "Stock:NoPrice".into()
                }
                Err(_) => "Stock:JsonErr".into(),
            },
            Err(_) => "Stock:NetErr".into(),
        }
    }

    async fn refresh_http(&mut self) -> String {
        if self.cfg.custom_http_url.is_empty() { return String::new(); }
        if self.cached_http_time.elapsed().as_secs() < self.cfg.cache_secs
            && !self.cached_http_text.is_empty()
        {
            return self.cached_http_text.clone();
        }
        let short = Client::builder().timeout(Duration::from_secs(3)).build().unwrap_or_else(|_| self.client.clone());
        match short.get(&self.cfg.custom_http_url).send().await {
            Ok(r) => match r.text().await {
                Ok(t) => {
                    let s: String = t.trim().chars().take(self.cfg.http_length).collect();
                    self.cached_http_text = s.clone();
                    self.cached_http_time = Instant::now();
                    s
                }
                Err(_) => "HTTP:Err".into(),
            },
            Err(_) => {
                if !self.cached_http_text.is_empty() { self.cached_http_text.clone() } else { "HTTP:Wait".into() }
            }
        }
    }
}

// 从 JSON 文本里捞字段 (留给未来扩展日出日落定位)
#[allow(dead_code)]
fn extract_json_number(text: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{}\"", key);
    let after = text.split(&pat).nth(1)?;
    let after_colon = after.split(':').nth(1)?;
    let num: String = after_colon
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

// 让 main.rs use net_agent::NetAgent; 能直接拿
pub use Backoffs as NetBackoffs;
pub use RefreshKind as NetRefreshKind;

// ==========================================
// 便捷启动助手 (一行调用, main.rs 直接用)
// ==========================================
pub fn start(args: &Args, running: Arc<AtomicBool>) -> Result<Arc<RwLock<NetSnapshot>>> {
    let cfg = AgentCfg::from_args(args);
    let agent = NetAgent::new(cfg).with_context(|| "NetAgent 初始化失败")?;
    let snap = agent.snapshot();
    tokio::spawn(async move { agent.run(running).await });
    Ok(snap)
}
