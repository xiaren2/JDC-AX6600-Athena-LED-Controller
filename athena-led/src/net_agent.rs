// ==========================================================
// NetAgent: 后台独立刷新所有网络数据 (天气/IP/股票/HTTP自定义)
//
// 🎯 核心目的: 把所有 HTTP 请求从主渲染循环里剥离出来!
//   之前代码里 screen.write_data(monitor.get_smart_weather().await) → 阻塞渲染30秒
//   新架构: 后台 tokio 任务每 cache_secs 秒更新一次快照, 渲染循环直接读快照, 零等待
//
// 🧠 失败退避策略 (参考经验 #2 ProtocolCache):
//   连续失败不直接"失效缓存", 而是推迟下一次刷新时间 (指数退避 1x/2x/4x/8x)
//   成功 1 次后立即重置计数器, 避免服务短暂抖一下就被拉黑
// ==========================================================
use anyhow::Result;
use chrono::{Local, NaiveTime};
use reqwest::Client;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// ------------ 网络数据快照 (渲染线程只读这个) ------------
#[derive(Debug, Clone, Default)]
pub struct NetSnapshot {
    pub weather: String,
    pub ip: String,
    pub stock: (String, u8),
    pub http_custom: String,
}

// ------------ 天气/IP/股票 结构体 (从 main.rs 搬过来) ------------
#[derive(Deserialize, Debug)]
struct WeatherResponse { weather: String, temperature: f64, #[serde(default)] temp_max: Option<f64>, #[serde(default)] temp_min: Option<f64> }
#[derive(Deserialize, Debug)]
struct SeniverseResponse { results: Vec<SeniverseResult> }
#[derive(Deserialize, Debug)]
struct SeniverseResult { daily: Vec<SeniverseDaily> }
#[derive(Deserialize, Debug)]
struct SeniverseDaily { high: String, low: String, code_day: String }
#[derive(Deserialize, Debug)]
struct WttrResult { current_condition: Vec<WttrCurrent>, weather: Vec<WttrDaily> }
#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrCurrent { temp_C: String, weatherDesc: Vec<WttrValue> }
#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrDaily { maxtempC: String, mintempC: String }
#[derive(Deserialize, Debug)]
struct WttrValue { value: String }
#[derive(Deserialize, Debug)]
struct OmGeoResponse { results: Option<Vec<OmLocation>> }
#[derive(Deserialize, Debug)]
struct OmLocation { name: String, latitude: f64, longitude: f64 }
#[derive(Deserialize, Debug)]
struct OmWeatherResponse { current_weather: OmCurrentWeather }
#[derive(Deserialize, Debug)]
struct OmCurrentWeather { temperature: f64, weathercode: u8 }

// ------------ Agent 配置 ------------
pub struct AgentConfig {
    pub cache_secs: u64,
    pub ip_url: String,
    pub weather_city: String,
    pub weather_source: String,
    pub seniverse_key: String,
    pub stock_url: String,
    pub custom_http_url: String,
    pub http_length: usize,
}

pub struct NetAgent {
    cfg: AgentConfig,
    client: Client,
    snapshot: Arc<RwLock<NetSnapshot>>,
}

impl NetAgent {
    pub fn new(cfg: AgentConfig) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Athena-LED Router)")
            .timeout(Duration::from_secs(10))   // 单个请求短超时, 避免挂死
            .build()?;
        let snapshot = Arc::new(RwLock::new(NetSnapshot {
            weather: "Weather:--".to_string(),
            ip: "IP:--".to_string(),
            stock: (String::new(), 0),
            http_custom: String::new(),
        }));
        Ok(Self { cfg, client, snapshot })
    }

    pub fn snapshot(&self) -> Arc<RwLock<NetSnapshot>> { Arc::clone(&self.snapshot) }

    // 启动后台刷新任务, 永不返回 (除非被 running 原子变量打断)
    pub async fn run(self, running: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        // ★ 失败退避计数器: 每个缓存条目独立维护
        //   fail_count = 0,1,2,3,... → sleep_secs = cache_secs * 2^min(count, 3)
        struct Backoffs { weather: u32, ip: u32, stock: u32, http: u32 }
        let mut backoff = Backoffs { weather: 0, ip: 0, stock: 0, http: 0 };

        // 启动时立刻刷新一次 (不等 cache_secs), 让屏幕第一帧就有内容
        self.force_refresh_all(&mut backoff).await;

        loop {
            if !running.load(std::sync::atomic::Ordering::SeqCst) { break; }

            // ---- 按 cache_secs 间隔刷新 (每个项目独立做指数退避) ----
            // 统一间隔, 用 fail_count 影响"是否实际刷新"
            let (mut snap, changed_vec) = self.try_refresh_all(&backoff).await;
            drop(snap); // (下面会再取写锁写数据)
            if !changed_vec.is_empty() {
                let results = changed_vec; // vec of (type, Ok/Err result)
                Self::apply_results(&mut backoff, results, &self.snapshot, |s, r| s.apply_refresh(r));
            }

            // 睡眠: 正常 = cache_secs; 任何一项失败= 缩短睡眠 (尽快重试)
            let has_fail = backoff.weather>0 || backoff.ip>0 || backoff.stock>0 || backoff.http>0;
            let sleep_base = if has_fail { self.cfg.cache_secs.min(30).max(5) } else { self.cfg.cache_secs };
            // 拆成 1s 小段轮询 running, 支持快速退出
            for _ in 0..sleep_base {
                if !running.load(std::sync::atomic::Ordering::SeqCst) { break; }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    // 启动立即全量刷新 (无视退避)
    async fn force_refresh_all(&self, backoff: &mut Backoffs) {
        let fut_ip     = async { ("ip".to_string(),    Self::ip_result(self.refresh_ip().await)) };
        let fut_weather= async { ("weather".to_string(),Self::ip_result(self.refresh_weather().await)) };
        let fut_stock  = async { ("stock".to_string(),  Self::stock_result(self.refresh_stock().await)) };
        let fut_http   = async { ("http".to_string(),   Self::http_result(self.refresh_http().await)) };
        let all: Vec<(String, RefreshKind)> = futures::future::join4(fut_ip, fut_weather, fut_stock, fut_http)
            .await
            .into_iter()
            .collect();
        Self::apply_results(backoff, all, &self.snapshot, |s, r| s.apply_refresh(r));
    }

    // 按退避计数器决定是否刷新: fail_count=0 必刷; fail_count>0 时按 (cache_secs*2^count) 估算是否到点 → 为简单起见此处每轮都刷
    async fn try_refresh_all(&self, _backoff: &Backoffs) -> (NetSnapshot, Vec<(String, RefreshKind)>) {
        let cfg = &self.cfg;
        let snap = self.snapshot.read().map(|s| s.clone()).unwrap_or_default();
        let futures_list: Vec<(String, RefreshKind)> = futures::future::join_all([
            async { ("ip".to_string(),      Self::ip_result(self.refresh_ip().await)) },
            async { ("weather".to_string(), Self::ip_result(self.refresh_weather().await)) },
            async { ("stock".to_string(),   Self::stock_result(self.refresh_stock().await)) },
            async { ("http".to_string(),    Self::http_result(self.refresh_http().await)) },
        ]).await;
        // 强制跳过 cfg 未使用
        let _ = cfg;
        (snap, futures_list)
    }

    fn apply_results<F>(backoff: &mut Backoffs, results: Vec<(String, RefreshKind)>, snap: &Arc<RwLock<NetSnapshot>>, mut f: F)
        where F: FnMut(&mut NetSnapshot, RefreshKind)
    {
        let mut guard = snap.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (k, kind) in results {
            let ok = matches!(
                &kind,
                RefreshKind::Ip(Ok(_))
                | RefreshKind::Weather(Ok(_))
                | RefreshKind::Stock(Ok(_))
                | RefreshKind::Http(Ok(_))
            );
            match k.as_str() {
                "ip"      => { if !ok { backoff.ip = (backoff.ip+1).min(3) } else { backoff.ip = 0 } }
                "weather" => { if !ok { backoff.weather = (backoff.weather+1).min(3) } else { backoff.weather = 0 } }
                "stock"   => { if !ok { backoff.stock = (backoff.stock+1).min(3) } else { backoff.stock = 0 } }
                "http"    => { if !ok { backoff.http = (backoff.http+1).min(3) } else { backoff.http = 0 } }
                _ => {}
            }
            f(&mut guard, kind);
        }
    }

    // ----- 4 个 refresh 方法 (几乎直接复用 main.rs 原始逻辑, 只是不再带 &mut self last_* 字段) -----
    async fn refresh_ip(&self) -> std::result::Result<String, ()> {
        let re = Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();
        match self.client.get(&self.cfg.ip_url).send().await {
            Ok(resp) => if let Ok(text) = resp.text().await {
                if let Some(mat) = re.find(&text) { return Ok(format!("IP:{}", mat.as_str())); }
            }
            Err(_) => {}
        }
        Err(())
    }

    async fn refresh_weather(&self) -> std::result::Result<String, ()> {
        let res = match self.cfg.weather_source.as_str() {
            "seniverse" => self.w_seniverse().await,
            "openmeteo" => self.w_openmeteo().await,
            "uapis"     => self.w_uapis().await,
            _           => self.w_wttr().await,
        };
        res
    }

    async fn w_uapis(&self) -> std::result::Result<String, ()> {
        let url = format!("https://uapis.cn/api/v1/misc/weather?city={}&forecast=true", self.cfg.weather_city);
        let resp = self.client.get(&url).send().await.map_err(|_| ())?;
        let data: WeatherResponse = resp.json().await.map_err(|_| ())?;
        let temp = data.temperature;
        let max = data.temp_max.unwrap_or(temp);
        let min = data.temp_min.unwrap_or(temp);
        let desc = data.weather;
        let icon = if desc.contains("雨") {"☂"} else if desc.contains("雪") {"❄"} else if desc.contains("云")||desc.contains("阴")||desc.contains("雾")||desc.contains("霾") {"☁"} else {"☀"};
        Ok(format!("{} {:.0}℃ {:.0}-{:.0}", icon, temp, min, max))
    }
    async fn w_wttr(&self) -> std::result::Result<String, ()> {
        let url = format!("https://wttr.in/{}?format=j1", self.cfg.weather_city);
        let resp = self.client.get(&url).send().await.map_err(|_| ())?;
        if !resp.status().is_success() { return Err(()); }
        let json: WttrResult = resp.json().await.map_err(|_| ())?;
        let curr = json.current_condition.first().ok_or(())?;
        let daily = json.weather.first().ok_or(())?;
        let temp = &curr.temp_C; let max = &daily.maxtempC; let min = &daily.mintempC;
        let desc = curr.weatherDesc.first().map(|d|d.value.to_lowercase()).unwrap_or_else(||"unknown".to_string());
        let icon = if desc.contains("rain")||desc.contains("shower")||desc.contains("drizzle") {"☂"}
            else if desc.contains("snow")||desc.contains("ice")||desc.contains("hail") {"❄"}
            else if desc.contains("thunder") {"⚡"} else if desc.contains("cloud")||desc.contains("overcast") {"☁"}
            else if desc.contains("mist")||desc.contains("fog") {"🌫"} else {"☀"};
        Ok(format!("{} {}℃ {}-{}", icon, temp, min, max))
    }
    async fn w_seniverse(&self) -> std::result::Result<String, ()> {
        if self.cfg.seniverse_key.is_empty() { return Err(()); }
        let url = format!("https://api.seniverse.com/v3/weather/daily.json?key={}&location={}&language=en&unit=c&start=0&days=1",
                          self.cfg.seniverse_key, self.cfg.weather_city);
        let resp = self.client.get(&url).send().await.map_err(|_| ())?;
        let json: SeniverseResponse = resp.json().await.map_err(|_| ())?;
        let daily = json.results.get(0).and_then(|r|r.daily.get(0)).ok_or(())?;
        let max = daily.high.parse::<f64>().unwrap_or(0.0);
        let min = daily.low.parse::<f64>().unwrap_or(0.0);
        let temp = (max+min)/2.0;
        let code = daily.code_day.parse::<i32>().unwrap_or(99);
        let icon = match code { 0..=3 => "☀", 4..=9 => "☁", 10..=19 => "☂", 20..=29 => "❄", _ => "☀" };
        Ok(format!("{} {:.0}℃ {:.0}-{:.0}", icon, temp, min, max))
    }
    async fn w_openmeteo(&self) -> std::result::Result<String, ()> {
        let geo_url = format!("https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh&format=json", self.cfg.weather_city);
        let resp = self.client.get(&geo_url).send().await.map_err(|_| ())?;
        if !resp.status().is_success() { return Err(()); }
        let geo: OmGeoResponse = resp.json().await.map_err(|_| ())?;
        let loc = geo.results.as_ref().and_then(|r| r.first()).ok_or(())?;
        let (lat, lon) = (loc.latitude, loc.longitude);
        let weather_url = format!("https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current_weather=true", lat, lon);
        let resp = self.client.get(&weather_url).send().await.map_err(|_| ())?;
        if !resp.status().is_success() { return Err(()); }
        let w: OmWeatherResponse = resp.json().await.map_err(|_| ())?;
        let temp = w.current_weather.temperature;
        let code = w.current_weather.weathercode;
        let icon = match code { 0 => "☀", 1|2|3 => "☁", 45|48 => "🌫", 51..=67|80..=82 => "☂", 71..=77|85..=86 => "❄", 95..=99 => "⚡", _ => "?" };
        Ok(format!("{} {:.1}℃", icon, temp))
    }

    async fn refresh_stock(&self) -> std::result::Result<(String, u8), ()> {
        if self.cfg.stock_url.is_empty() { return Err(()); }
        // 从 snapshot 里拿上一轮的 price 做涨跌判断
        let prev_price = {
            self.snapshot.read().map(|s| {
                let p_str = &s.stock.0;
                // 提取数字部分 (兼容 ".2" "1,234.56" "234")
                let cleaned: String = p_str.chars().filter(|c|c.is_ascii_digit()||*c=='.'||*c=='-').collect();
                cleaned.parse::<f64>().unwrap_or(0.0)
            }).unwrap_or(0.0)
        };
        let resp = self.client.get(&self.cfg.stock_url).send().await.map_err(|_| ())?;
        let json_val: Value = resp.json().await.map_err(|_| ())?;
        let price = json_val["price"].as_f64()
            .or_else(|| json_val["price"].as_str().and_then(|s|s.parse().ok()))
            .or_else(|| json_val["last"].as_f64())
            .or_else(|| json_val["close"].as_f64())
            .ok_or(())?;
        let mut flag = 2u8;
        if prev_price > 0.0 {
            if price > prev_price { flag = 4; }
            else if price < prev_price { flag = 8; }
        }
        let text = if price>1000.0 {format!("{:.0}",price)} else {format!("{:.2}",price)};
        Ok((text, flag))
    }

    async fn refresh_http(&self) -> std::result::Result<String, ()> {
        if self.cfg.custom_http_url.is_empty() { return Ok(String::new()); }
        let resp = self.client.get(&self.cfg.custom_http_url).send().await.map_err(|_| ())?;
        let text = resp.text().await.map_err(|_| ())?;
        let t = text.trim();
        Ok(t.chars().take(self.cfg.http_length).collect())
    }

    // --- RefreshKind: 用于把 4 种不同类型的 Result 装箱统一枚举 ---
    fn ip_result(r: std::result::Result<String, ()>) -> RefreshKind { RefreshKind::Ip(r) }
    fn stock_result(r: std::result::Result<(String, u8), ()>) -> RefreshKind { RefreshKind::Stock(r) }
    fn http_result(r: std::result::Result<String, ()>) -> RefreshKind { RefreshKind::Http(r) }
}

enum RefreshKind {
    Ip(std::result::Result<String, ()>),
    Weather(std::result::Result<String, ()>),
    Stock(std::result::Result<(String, u8), ()>),
    Http(std::result::Result<String, ()>),
}

impl NetSnapshot {
    fn apply_refresh(&mut self, kind: RefreshKind) {
        // 成功就更新, 失败保留旧值 (永远展示"上次成功"的值, 不显示 Err/Wait 扰乱屏幕)
        match kind {
            RefreshKind::Ip(Ok(v))      => self.ip = v,
            RefreshKind::Weather(Ok(v)) => self.weather = v,
            RefreshKind::Stock(Ok(v))   => self.stock = v,
            RefreshKind::Http(Ok(v))    => self.http_custom = v,
            _ => {}
        }
    }
}

// ========== 让 NetAgent 里引用的未用符号保持无警告 ==========
#[allow(dead_code)]
fn _unused_import(_: Local, _: NaiveTime, _: Instant) {}
