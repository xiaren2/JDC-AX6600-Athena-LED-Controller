#[cfg(unix)]
mod led_screen;
#[cfg(not(unix))]
mod led_screen;
#[cfg(unix)]
mod char_dict;
#[cfg(not(unix))]
mod char_dict;

mod button;
mod control;

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::signal::unix::{signal, SignalKind};
use chrono::{Local, NaiveTime};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use regex::Regex;

// --- 天气结构体 ---
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

// --- 系统监控器 ---
struct SystemMonitor {
    net_interface: String,
    http_client: Client,
    last_rx_bytes: u64, last_tx_bytes: u64, last_net_check: Instant,
    last_cpu_total: u64, last_cpu_idle: u64,
    last_stock_price: f64,
    cached_weather: String, last_weather_time: Instant,
    cached_ip: String, last_ip_time: Instant,
}

impl SystemMonitor {
    fn new(net_dev: String) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Athena-LED Router)")
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;
        Ok(Self {
            http_client: client, net_interface: net_dev,
            cached_weather: "Wait...".to_string(), last_weather_time: Instant::now() - Duration::from_secs(3600*24),
            cached_ip: "Checking...".to_string(), last_ip_time: Instant::now() - Duration::from_secs(3600*24),
            last_rx_bytes: 0, last_tx_bytes: 0, last_net_check: Instant::now(),
            last_cpu_total: 0, last_cpu_idle: 0, last_stock_price: 0.0,
        })
    }
    fn init(&mut self) {
        let (rx, tx) = self.read_net_bytes();
        self.last_rx_bytes = rx; self.last_tx_bytes = tx;
        let (total, idle) = self.read_cpu_stats();
        self.last_cpu_total = total; self.last_cpu_idle = idle;
    }
    fn get_animated_icon(&self, s: &str, f: bool) -> String {
        match s {
            "☀" => if f {"☀".to_string()} else {"☼".to_string()},
            "☂" => if f {"☂".to_string()} else {"☔".to_string()},
            "☁" => if f {"☁".to_string()} else {"🌥".to_string()},
            "❄" => if f {"❄".to_string()} else {"❅".to_string()},
            "⚡" => if f {"⚡".to_string()} else {"☇".to_string()},
            _ => s.to_string(),
        }
    }
    fn read_net_bytes(&self) -> (u64, u64) {
        let content = fs::read_to_string("/proc/net/dev").unwrap_or_default();
        for line in content.lines() {
            if line.contains(&self.net_interface) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let rx_idx = if parts[0].contains(':') {1} else {2};
                let tx_idx = rx_idx + 8;
                if parts.len() > tx_idx {
                    return (parts[rx_idx].parse().unwrap_or(0), parts[tx_idx].parse().unwrap_or(0));
                }
            }
        }
        (0, 0)
    }
    fn read_cpu_stats(&self) -> (u64, u64) {
        let content = fs::read_to_string("/proc/stat").unwrap_or_default();
        if let Some(line) = content.lines().next() {
            if line.starts_with("cpu ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 5 {
                    let user: u64 = parts[1].parse().unwrap_or(0);
                    let nice: u64 = parts[2].parse().unwrap_or(0);
                    let system: u64 = parts[3].parse().unwrap_or(0);
                    let idle: u64 = parts[4].parse().unwrap_or(0);
                    let iowait: u64 = parts.get(5).and_then(|s|s.parse().ok()).unwrap_or(0);
                    let irq: u64 = parts.get(6).and_then(|s|s.parse().ok()).unwrap_or(0);
                    let softirq: u64 = parts.get(7).and_then(|s|s.parse().ok()).unwrap_or(0);
                    return (user+nice+system+idle+iowait+irq+softirq, idle);
                }
            }
        }
        (0, 0)
    }
    fn get_speed_string(&mut self, mode: u8) -> String {
        let (curr_rx, curr_tx) = self.read_net_bytes();
        let now = Instant::now();
        let duration = now.duration_since(self.last_net_check).as_secs_f64();
        if duration < 0.1 { return "...".to_string(); }
        if self.last_rx_bytes == 0 || self.last_tx_bytes == 0 || duration > 30.0 {
            self.last_rx_bytes = curr_rx; self.last_tx_bytes = curr_tx; self.last_net_check = now;
            return format_bytes_speed(0.0);
        }
        let speed = if mode == 0 {
            (curr_rx.saturating_sub(self.last_rx_bytes)) as f64 / duration
        } else {
            (curr_tx.saturating_sub(self.last_tx_bytes)) as f64 / duration
        };
        self.last_rx_bytes = curr_rx; self.last_tx_bytes = curr_tx; self.last_net_check = now;
        format_bytes_speed(speed)
    }
    fn get_total_rx_string(&self) -> String {
        let (curr_rx, _) = self.read_net_bytes();
        format!("TD:{}", format_bytes_total(curr_rx))
    }
    fn get_total_tx_string(&self) -> String {
        let (_, curr_tx) = self.read_net_bytes();
        format!("TU:{}", format_bytes_total(curr_tx))
    }
    fn get_cpu_usage_string(&mut self) -> String {
        let (curr_total, curr_idle) = self.read_cpu_stats();
        let diff_total = curr_total.saturating_sub(self.last_cpu_total);
        let diff_idle = curr_idle.saturating_sub(self.last_cpu_idle);
        self.last_cpu_total = curr_total; self.last_cpu_idle = curr_idle;
        if diff_total == 0 { return "CPU:-".to_string(); }
        let usage = 100.0 * (1.0 - (diff_idle as f64 / diff_total as f64));
        format!("C:{:.0}%", usage)
    }
    fn get_mem_string(&self) -> String {
        let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mut total = 0.0f64; let mut available = 0.0f64;
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 { continue; }
            match parts[0] {
                "MemTotal:" => total = parts[1].parse().unwrap_or(0.0),
                "MemAvailable:" => available = parts[1].parse().unwrap_or(0.0),
                _ => {}
            }
        }
        if total > 0.0 { format!("M:{:.0}%", 100.0*(1.0 - available/total)) } else { "M:Err".to_string() }
    }
    fn get_load_string(&self) -> String {
        let content = fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let parts: Vec<&str> = content.split_whitespace().collect();
        if !parts.is_empty() { format!("L:{}", parts[0]) } else { "L:Err".to_string() }
    }
    fn get_uptime_string(&self) -> String {
        if let Ok(content) = fs::read_to_string("/proc/uptime") {
            if let Some(sec_str) = content.split_whitespace().next() {
                if let Ok(seconds) = sec_str.parse::<f64>() {
                    let secs = seconds as u64;
                    let d = secs/86400; let h = (secs%86400)/3600; let m = (secs%3600)/60;
                    return if d>0 {format!("Up:{}d{}h",d,h)} else if h>0 {format!("Up:{}h{}m",h,m)} else {format!("Up:{}m",m)};
                }
            }
        }
        "Up:Err".to_string()
    }
    fn get_temps_by_ids(&self, ids: &str) -> String {
        let mut results = Vec::new();
        let id_list: Vec<&str> = ids.split(|c|c==' '||c==',').filter(|s|!s.is_empty()).collect();
        for id in id_list {
            let type_path = format!("/sys/class/thermal/thermal_zone{}/type", id);
            let temp_path = format!("/sys/class/thermal/thermal_zone{}/temp", id);
            if let Ok(type_name_raw) = fs::read_to_string(&type_path) {
                let label = type_name_raw.trim().to_lowercase().replace("-thermal","");
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(raw_temp) = temp_str.trim().parse::<f64>() {
                        let val = if raw_temp>1000.0 {raw_temp/1000.0} else {raw_temp};
                        results.push(format!("{}:{:.0}℃", label, val));
                    }
                }
            }
        }
        if results.is_empty() {"Temp:--".to_string()} else {results.join(" ")}
    }
    fn get_online_devices(&self) -> String {
        if let Ok(content) = fs::read_to_string("/proc/net/arp") {
            let c = content.lines().count();
            if c > 1 { return format!("Dev:{}", c-1); }
        }
        "Dev:0".to_string()
    }
    pub async fn get_http_text(&self, url: &str, prefix: &str, max_len: usize) -> String {
        if url.is_empty() { return String::new(); }
        let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap_or(self.http_client.clone());
        match client.get(url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(text) => {
                    let t = text.trim();
                    let truncated: String = t.chars().take(max_len).collect();
                    format!("{}{}", prefix, truncated)
                }
                Err(_) => format!("{}Err", prefix),
            }
            Err(_) => format!("{}Wait", prefix),
        }
    }
    async fn get_public_ip(&mut self, ip_url: &str) -> String {
        if self.last_ip_time.elapsed() < Duration::from_secs(3600) {
            if !self.cached_ip.contains("Err") { return self.cached_ip.clone(); }
        }
        let mut new_ip = "IP:Err".to_string();
        match self.http_client.get(ip_url).send().await {
            Ok(resp) => if let Ok(text) = resp.text().await {
                let re = Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();
                if let Some(mat) = re.find(&text) { new_ip = format!("IP:{}", mat.as_str()); }
            }
            Err(e) => println!("IP error: {:?}", e),
        }
        if !new_ip.contains("Err") { self.cached_ip = new_ip.clone(); self.last_ip_time = Instant::now(); }
        new_ip
    }
    async fn get_stock_trend(&mut self, url: &str) -> (String, u8) {
        if url.is_empty() { return (String::new(), 0); }
        match self.http_client.get(url).send().await {
            Ok(resp) => if let Ok(json_val) = resp.json::<Value>().await {
                let price_opt = json_val["price"].as_f64()
                    .or_else(|| json_val["price"].as_str().and_then(|s|s.parse::<f64>().ok()))
                    .or_else(|| json_val["last"].as_f64())
                    .or_else(|| json_val["close"].as_f64());
                if let Some(current_price) = price_opt {
                    let mut flag = 2;
                    if self.last_stock_price > 0.0 {
                        if current_price > self.last_stock_price { flag = 4; }
                        else if current_price < self.last_stock_price { flag = 8; }
                    }
                    self.last_stock_price = current_price;
                    let text = if current_price>1000.0 {format!("{:.0}",current_price)} else {format!("{:.2}",current_price)};
                    return (text, flag);
                }
            }
            Err(_) => {}
        }
        ("Err".to_string(), 0)
    }
    async fn get_smart_weather(&mut self, location: &str, source: &str, key: &str) -> String {
        if self.last_weather_time.elapsed() < Duration::from_secs(1800) {
            if !self.cached_weather.contains("Err") && !self.cached_weather.contains("Wait") {
                return self.cached_weather.clone();
            }
        }
        let result = match source {
            "seniverse" => self.get_weather_from_seniverse(location, key).await,
            "openmeteo" => self.get_weather_from_open_meteo(location).await,
            "uapis" => self.get_weather_from_uapis(location).await,
            _ => self.get_weather_from_wttr(location).await,
        };
        if !result.contains("Err") && !result.contains("Wait") {
            self.cached_weather = result.clone(); self.last_weather_time = Instant::now();
        }
        result
    }
    async fn get_weather_from_uapis(&self, city: &str) -> String {
        let url = format!("https://uapis.cn/api/v1/misc/weather?city={}&forecast=true", city);
        if let Ok(resp) = self.http_client.get(&url).send().await {
            if let Ok(data) = resp.json::<WeatherResponse>().await {
                let temp = data.temperature;
                let max = data.temp_max.unwrap_or(temp);
                let min = data.temp_min.unwrap_or(temp);
                let desc = data.weather;
                let icon = if desc.contains("雨") {"☂"} else if desc.contains("雪") {"❄"} else if desc.contains("云")||desc.contains("阴")||desc.contains("雾")||desc.contains("霾") {"☁"} else {"☀"};
                return format!("{} {:.0}℃ {:.0}-{:.0}", icon, temp, min, max);
            }
        }
        "W:Err(U)".to_string()
    }
    async fn get_weather_from_wttr(&self, city: &str) -> String {
        let url = format!("https://wttr.in/{}?format=j1", city);
        if let Ok(resp) = self.http_client.get(&url).send().await {
            if !resp.status().is_success() { return format!("W:Err({})", resp.status().as_u16()); }
            if let Ok(json) = resp.json::<WttrResult>().await {
                if let (Some(curr), Some(daily)) = (json.current_condition.first(), json.weather.first()) {
                    let temp = &curr.temp_C; let max = &daily.maxtempC; let min = &daily.mintempC;
                    let desc = curr.weatherDesc.first().map(|d|d.value.to_lowercase()).unwrap_or_else(||"unknown".to_string());
                    let icon = if desc.contains("rain")||desc.contains("shower")||desc.contains("drizzle") {"☂"}
                        else if desc.contains("snow")||desc.contains("ice")||desc.contains("hail") {"❄"}
                        else if desc.contains("thunder") {"⚡"}
                        else if desc.contains("cloud")||desc.contains("overcast") {"☁"}
                        else if desc.contains("mist")||desc.contains("fog") {"🌫"} else {"☀"};
                    return format!("{} {}℃ {}-{}", icon, temp, min, max);
                }
            }
        }
        "W:NetErr".to_string()
    }
    async fn get_weather_from_seniverse(&self, location: &str, key: &str) -> String {
        let url = format!("https://api.seniverse.com/v3/weather/daily.json?key={}&location={}&language=en&unit=c&start=0&days=1", key, location);
        if let Ok(resp) = self.http_client.get(&url).send().await {
            if let Ok(json) = resp.json::<SeniverseResponse>().await {
                if let Some(daily) = json.results.get(0).and_then(|r|r.daily.get(0)) {
                    let max = daily.high.parse::<f64>().unwrap_or(0.0);
                    let min = daily.low.parse::<f64>().unwrap_or(0.0);
                    let temp = (max+min)/2.0;
                    let code = daily.code_day.parse::<i32>().unwrap_or(99);
                    let icon = match code { 0..=3 => "☀", 4..=9 => "☁", 10..=19 => "☂", 20..=29 => "❄", _ => "☀" };
                    return format!("{} {:.0}℃ {:.0}-{:.0}", icon, temp, min, max);
                }
            }
        }
        "W:Err(S)".to_string()
    }
    async fn get_weather_from_open_meteo(&self, city: &str) -> String {
        let geo_url = format!("https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh&format=json", city);
        let (lat, lon) = match self.http_client.get(&geo_url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() { return "W:GeoErr".to_string(); }
                match resp.json::<OmGeoResponse>().await {
                    Ok(data) => if let Some(results) = data.results {
                        if let Some(loc) = results.first() { (loc.latitude, loc.longitude) } else { return "W:NoCity".to_string(); }
                    } else { return "W:NoCity".to_string(); },
                    Err(_) => return "W:GeoJson".to_string(),
                }
            }
            Err(_) => return "W:GeoNet".to_string(),
        };
        let weather_url = format!("https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current_weather=true", lat, lon);
        match self.http_client.get(&weather_url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() { return "W:ApiErr".to_string(); }
                match resp.json::<OmWeatherResponse>().await {
                    Ok(data) => {
                        let temp = data.current_weather.temperature;
                        let code = data.current_weather.weathercode;
                        let icon = match code { 0 => "☀", 1|2|3 => "☁", 45|48 => "🌫", 51..=67|80..=82 => "☂", 71..=77|85..=86 => "❄", 95..=99 => "⚡", _ => "?" };
                        return format!("{} {:.1}℃", icon, temp);
                    }
                    Err(_) => "W:JsonErr".to_string(),
                }
            }
            Err(_) => "W:NetErr".to_string(),
        }
    }
}

fn format_bytes_speed(b: f64) -> String {
    if b > 1_048_576.0 { format!("{:.1}M", b/1_048_576.0) }
    else if b > 1024.0 { format!("{:.0}K", b/1024.0) }
    else { format!("{:.0}B", b) }
}
fn format_bytes_total(bytes: u64) -> String {
    let b = bytes as f64;
    if b > 1_099_511_627_776.0 { format!("{:.2}T", b/1_099_511_627_776.0) }
    else if b > 1_073_741_824.0 { format!("{:.2}G", b/1_073_741_824.0) }
    else if b > 1_048_576.0 { format!("{:.1}M", b/1_048_576.0) }
    else { format!("{:.0}K", b/1024.0) }
}
fn get_seconds_until_wake(wake_time_str: &str) -> u64 {
    let now = Local::now();
    let wake_time = match NaiveTime::parse_from_str(wake_time_str, "%H:%M") { Ok(t)=>t, Err(_)=>return 60 };
    let mut target_dt = now.date_naive().and_time(wake_time).and_local_timezone(Local).unwrap();
    if target_dt <= now { target_dt = target_dt + chrono::Duration::days(1); }
    let d = target_dt.signed_duration_since(now).num_seconds();
    if d > 0 { d as u64 + 2 } else { 60 }
}
fn is_sleep_time(start_str: &str, end_str: &str) -> bool {
    if start_str.is_empty() || end_str.is_empty() { return false; }
    let start = match NaiveTime::parse_from_str(start_str, "%H:%M") { Ok(t)=>t, Err(_)=>return false };
    let end = match NaiveTime::parse_from_str(end_str, "%H:%M") { Ok(t)=>t, Err(_)=>return false };
    let now = Local::now().time();
    if start < end { now >= start && now < end } else { now >= start || now < end }
}

// --- 参数定义 ---
#[derive(Parser, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t = 5)] seconds: u64,
    #[arg(long, default_value_t = 5)] light_level: u8,
    #[arg(long, default_value = "date timeBlink weather stock uptime netspeed_down netspeed_up cpu")] display_order: String,
    #[arg(long, default_value = "br-lan")] net_interface: String,
    #[arg(long, default_value = "http://members.3322.org/dyndns/getip")] ip_url: String,
    #[arg(long, default_value = "")] custom_text: String,
    #[arg(long, default_value = "")] custom_http_url: String,
    #[arg(long, default_value_t = 15)] http_length: usize,
    #[arg(long, default_value = "Beijing")] weather_city: String,
    #[arg(long, default_value = "uapis")] weather_source: String,
    #[arg(long, default_value = "S140W1C6_1_8R8_8c")] seniverse_key: String,
    #[arg(long, default_value = "")] stock_url: String,
    #[arg(long, default_value = "4")] temp_flag: String,
    #[arg(long, default_value = "")] sleep_start: String,
    #[arg(long, default_value = "")] sleep_end: String,
    #[arg(long, default_value = "simple")] weather_format: String,

    // [按键] GPIO 引脚偏移，AX6600 按键固定为 71
    #[arg(long, default_value = "71")]
    pub button_gpio: String,

    // [按键] GPIO 基址 (debugfs 后端换算全局编号用)
    #[arg(long, default_value = "auto")]
    pub gpio_base: String,

    // [Mesh 键] 是否启用 Mesh 键自定义动作
    #[arg(long, default_value_t = 0)]
    pub enable_mesh_button: u8,

    // [Mesh 键] GPIO 引脚偏移，AX6600 Mesh 键默认为 72
    #[arg(long, default_value = "72")]
    pub mesh_button_gpio: String,

    // [Mesh 键] 短按动作: none / reboot / restart_network / restart_wifi / restart_athena
    #[arg(long, default_value = "none")]
    pub mesh_short_action: String,

    // [Mesh 键] 长按动作: none / reboot / restart_network / restart_wifi / restart_athena
    #[arg(long, default_value = "none")]
    pub mesh_long_action: String,

    // [4 盏状态 LED 独立开关] flag 形式：传入即表示关闭对应灯
    // bit0(1)=时钟灯, bit1(2)=奖牌灯, bit2(4)=上箭头灯, bit3(8)=下箭头灯
    #[arg(long, default_value_t = false)]
    pub disable_led_clock: bool,

    #[arg(long, default_value_t = false)]
    pub disable_led_medal: bool,

    #[arg(long, default_value_t = false)]
    pub disable_led_up: bool,

    #[arg(long, default_value_t = false)]
    pub disable_led_down: bool,
}

/// 从 /etc/config/system 读取时区设置
///
/// 优先级:
/// 1. timezone 字段 (OpenWrt 原生就是 POSIX 格式, 如 'CST-8')
/// 2. zonename 字段 (IANA 格式, 如 'Asia/Shanghai') → 内置映射表转 POSIX
/// 3. 回退 UTC
///
/// 用 POSIX 格式 TZ 字符串, 不依赖 zoneinfo 数据包即可正确计算本地时间
fn set_timezone_from_config() -> Result<()> {
    let content = fs::read_to_string("/etc/config/system").unwrap_or_default();

    // 解析 OpenWrt UCI 格式: option timezone 'CST-8'
    let mut timezone_posix: Option<String> = None;  // POSIX 格式
    let mut zonename_iana: Option<String> = None;   // IANA 格式

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = parse_uci_option(trimmed, "timezone") {
            timezone_posix = Some(val);
        } else if let Some(val) = parse_uci_option(trimmed, "zonename") {
            zonename_iana = Some(val);
        }
    }

    // 优先用 timezone 字段 (已经是 POSIX 格式, 可直接用)
    if let Some(ref tz) = timezone_posix {
        if !tz.is_empty() && tz != "UTC" {
            env::set_var("TZ", tz);
            println!("🌍 [时区] 使用 timezone 字段: TZ={}", tz);
            return Ok(());
        }
    }

    // 其次用 zonename 字段 (IANA 格式, 需要转换)
    if let Some(ref zn) = zonename_iana {
        if !zn.is_empty() {
            // 如果系统装了 zoneinfo 数据包, 直接用 IANA 名称 (支持夏令时)
            let zoneinfo_path = format!("/usr/share/zoneinfo/{}", zn);
            if std::path::Path::new(&zoneinfo_path).exists() {
                env::set_var("TZ", zn);
                println!("🌍 [时区] 使用 zonename (zoneinfo 数据包存在): TZ={}", zn);
                return Ok(());
            }
            // 没装 zoneinfo 数据包, 用内置映射表转 POSIX 格式
            if let Some(posix_tz) = iana_to_posix(zn) {
                env::set_var("TZ", &posix_tz);
                println!("🌍 [时区] 使用 zonename 转换: {} -> TZ={}", zn, posix_tz);
                return Ok(());
            }
            // 映射表没命中, 打个日志方便排查
            println!("⚠️ [时区] 未知 zonename '{}', 回退 UTC", zn);
        }
    }

    env::set_var("TZ", "UTC");
    println!("🌍 [时区] 未找到时区配置, 使用 UTC");
    Ok(())
}

/// 解析 UCI 格式行: "option key 'value'" → value
fn parse_uci_option(line: &str, key: &str) -> Option<String> {
    let prefix = format!("option {}", key);
    let rest = line.trim().strip_prefix(&prefix)?;
    let rest = rest.trim();
    // 去掉引号
    let rest = rest.trim_matches('\'').trim_matches('"');
    if rest.is_empty() { None } else { Some(rest.to_string()) }
}

/// IANA 时区名转 POSIX TZ 字符串 (无 zoneinfo 数据包时使用)
///
/// POSIX 格式: <名字><偏移>
/// 偏移符号与直觉相反: CST-8 = UTC+8 (东八区), PST8 = UTC-8 (西八区)
/// 注意: 此映射表只提供固定偏移, 不含夏令时规则
fn iana_to_posix(zonename: &str) -> Option<String> {
    // (IANA 名称, POSIX TZ 字符串)
    // 偏移规则: 东半球为负, 西半球为正
    let table: &[(&str, &str)] = &[
        // UTC+8 中国及周边
        ("Asia/Shanghai", "CST-8"), ("Asia/Chongqing", "CST-8"),
        ("Asia/Harbin", "CST-8"), ("Asia/Urumqi", "CST-8"),
        ("Asia/Kashgar", "CST-8"), ("Asia/Hong_Kong", "HKT-8"),
        ("Asia/Macau", "CST-8"), ("Asia/Taipei", "CST-8"),
        ("Asia/Singapore", "SGT-8"), ("Asia/Kuala_Lumpur", "MYT-8"),
        ("Asia/Manila", "PHT-8"), ("Asia/Makassar", "WITA-8"),
        ("Asia/Brunei", "BNT-8"),
        // UTC+9 日韩
        ("Asia/Tokyo", "JST-9"), ("Asia/Seoul", "KST-9"),
        ("Asia/Pyongyang", "KST-9"),
        // UTC+7 东南亚
        ("Asia/Bangkok", "ICT-7"), ("Asia/Jakarta", "WIB-7"),
        ("Asia/Ho_Chi_Minh", "ICT-7"), ("Asia/Phnom_Penh", "ICT-7"),
        ("Asia/Vientiane", "ICT-7"),
        // UTC+6
        ("Asia/Dhaka", "BST-6"), ("Asia/Almaty", "ALMT-6"),
        ("Asia/Omsk", "OMST-6"),
        // UTC+5:30 印度
        ("Asia/Kolkata", "IST-5:30"), ("Asia/Calcutta", "IST-5:30"),
        ("Asia/Colombo", "IST-5:30"),
        // UTC+5
        ("Asia/Karachi", "PKT-5"), ("Asia/Tashkent", "UZT-5"),
        ("Asia/Yekaterinburg", "YEKT-5"),
        // UTC+4
        ("Asia/Dubai", "GST-4"), ("Asia/Baku", "AZT-4"),
        ("Indian/Mauritius", "MUT-4"),
        // UTC+3
        ("Europe/Moscow", "MSK-3"), ("Asia/Riyadh", "AST-3"),
        ("Asia/Tehran", "IRST-3:30"), ("Africa/Nairobi", "EAT-3"),
        // UTC+2
        ("Europe/Athens", "EET-2"), ("Europe/Bucharest", "EET-2"),
        ("Africa/Cairo", "EET-2"), ("Europe/Helsinki", "EET-2"),
        ("Asia/Jerusalem", "IST-2"),
        // UTC+1 欧洲中部
        ("Europe/Paris", "CET-1"), ("Europe/Berlin", "CET-1"),
        ("Europe/Rome", "CET-1"), ("Europe/Madrid", "CET-1"),
        ("Europe/Amsterdam", "CET-1"), ("Europe/Brussels", "CET-1"),
        ("Europe/Vienna", "CET-1"), ("Europe/Stockholm", "CET-1"),
        ("Europe/Oslo", "CET-1"), ("Europe/Copenhagen", "CET-1"),
        ("Europe/Warsaw", "CET-1"), ("Europe/Prague", "CET-1"),
        ("Europe/Zurich", "CET-1"), ("Africa/Lagos", "WAT-1"),
        // UTC+0 英国/爱尔兰
        ("Europe/London", "GMT0"), ("Europe/Dublin", "GMT0"),
        ("Atlantic/Reykjavik", "GMT0"),
        // UTC-3 美洲
        ("America/Sao_Paulo", "BRT3"), ("America/Argentina/Buenos_Aires", "ART3"),
        ("America/Montevideo", "UYT3"),
        // UTC-4
        ("America/Halifax", "AST4"), ("America/Caracas", "VET4"),
        ("America/Santiago", "CLT4"),
        // UTC-5 美国东部
        ("America/New_York", "EST5"), ("America/Toronto", "EST5"),
        ("America/Miami", "EST5"), ("America/Bogota", "COT5"),
        ("America/Lima", "PET5"),
        // UTC-6 美国中部
        ("America/Chicago", "CST6"), ("America/Mexico_City", "CST6"),
        ("America/Denver", "MST7"),
        // UTC-8 美国太平洋
        ("America/Los_Angeles", "PST8"), ("America/Tijuana", "PST8"),
        ("America/Vancouver", "PST8"),
        // UTC-10 夏威夷
        ("Pacific/Honolulu", "HST10"),
        // UTC+10 澳洲东部
        ("Australia/Sydney", "AEST-10"), ("Australia/Melbourne", "AEST-10"),
        ("Australia/Brisbane", "AEST-10"),
        // UTC+9:30 澳洲中部
        ("Australia/Adelaide", "ACST-9:30"), ("Australia/Darwin", "ACST-9:30"),
        // UTC+12 新西兰
        ("Pacific/Auckland", "NZST-12"), ("Pacific/Fiji", "FJT-12"),
        // UTC-3:30 纽芬兰
        ("America/St_Johns", "NST3:30"),
    ];
    for (name, posix) in table {
        if zonename == *name {
            return Some(posix.to_string());
        }
    }
    None
}

// ==========================================
// 按键指令约定: 正数N=切台, -1=息屏Toggle, 0=无操作
// ==========================================

#[tokio::main]
async fn main() -> Result<()> {
    let _ = set_timezone_from_config();
    let args = Args::parse();

    // 4 盏独立状态 LED 禁用掩码
    let mut disabled_led_mask: u8 = 0;
    if args.disable_led_clock { disabled_led_mask |= 0x01; }
    if args.disable_led_medal { disabled_led_mask |= 0x02; }
    if args.disable_led_up    { disabled_led_mask |= 0x04; }
    if args.disable_led_down  { disabled_led_mask |= 0x08; }

    let mut screen = led_screen::LedScreen::new_with_mask(
        581, 582, 585, 586, disabled_led_mask,
    )
        .context("Failed to init screen")?;
    screen.power(true, args.light_level)?;

    let mut monitor = SystemMonitor::new(args.net_interface.clone())
        .context("Failed to initialize system monitor")?;

    let running = Arc::new(AtomicBool::new(true));
    let running_for_listener = Arc::clone(&running);

    // 共享控制状态（按键双击 go_home 标志）
    let control_state = control::new_shared();

    let (tx, rx) = watch::channel(1i32);

    // 共享 watch receiver 给屏幕底层 flow 滚动检测按键中断 (flow() 里 20ms 轮询)
    let rx_for_screen = Arc::new(Mutex::new(rx.clone()));
    screen.bind_interrupt_rx(Arc::clone(&rx_for_screen));

    // ==========================================
    // 🎮 启动按键监听器 (GPIO 引脚 71)
    // 双后端: 字符设备 /dev/gpiochipN (优先) / debugfs 兜底
    // ==========================================
    button::spawn_button_listener(
        tx.clone(),
        running_for_listener,
        args.button_gpio.clone(),
        args.gpio_base.clone(),
        Arc::clone(&control_state),
    );

    // Mesh 键监听器 (GPIO 引脚 72, 可自定义短按/长按动作)
    if args.enable_mesh_button != 0 {
        let running_for_mesh = Arc::clone(&running);
        button::spawn_mesh_button_listener(
            running_for_mesh,
            args.mesh_button_gpio.clone(),
            args.gpio_base.clone(),
            args.mesh_short_action.clone(),
            args.mesh_long_action.clone(),
        );
    }

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                println!("🛑 收到 SIGTERM，关屏退出...");
                screen.power(false, 0)?; break;
            },
            _ = sigint.recv() => {
                println!("🛑 收到 SIGINT，关屏退出...");
                screen.power(false, 0)?; break;
            },
            _ = process_loop(&mut screen, &args, &mut monitor, rx.clone(), Arc::clone(&control_state), Arc::clone(&running)) => {},
        }
    }
    running.store(false, Ordering::SeqCst);
    Ok(())
}

// ==========================================
// 辅助函数: 用值比较检测 watch channel 变化
// 不依赖 has_changed()，避免 borrow_and_update 消费变化标志后
// 其他检查点漏掉指令
// ==========================================
fn poll_cmd(rx: &watch::Receiver<i32>, last_seen: &mut i32) -> Option<i32> {
    let cmd = *rx.borrow();
    if cmd != *last_seen {
        *last_seen = cmd;
        Some(cmd)
    } else {
        None
    }
}

// 主渲染循环
async fn process_loop(
    screen: &mut led_screen::LedScreen,
    args: &Args,
    monitor: &mut SystemMonitor,
    mut rx: watch::Receiver<i32>,
    control_state: control::SharedControl,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let modules: Vec<&str> = args.display_order.split_whitespace().collect();
    if modules.is_empty() { return Ok(()); }

    let mut current_channel: usize = 1;
    let mut last_seen_cmd = *rx.borrow();
    // 屏幕亮灭状态（与按键 -1 Toggle 同步）
    let screen_on = Arc::new(AtomicBool::new(true));

    loop {
        if !running.load(Ordering::SeqCst) { return Ok(()); }

        // ==========================================
        // [双击] 消费 go_home 标志: 回到频道 1
        // ==========================================
        if let Ok(mut st) = control_state.lock() {
            if st.go_home {
                st.go_home = false;
                println!("⏮️ [调度] go_home=true，跳回频道 1");
                current_channel = 1;
            }
        }

        // 1. 检查按键指令 (用值比较，不依赖 has_changed)
        if let Some(cmd) = poll_cmd(&rx, &mut last_seen_cmd) {
            if cmd == -1 {
                // 息屏 Toggle
                let was_on = screen_on.load(Ordering::SeqCst);
                let now_on = !was_on;
                screen_on.store(now_on, Ordering::SeqCst);
                if !now_on {
                    println!("🌙 [渲染] 息屏，等待唤醒...");
                    screen.write_data(b"        ", 0)?;
                    wait_for_wakeup(&mut rx, Arc::clone(&screen_on), Arc::clone(&running)).await;
                    if !running.load(Ordering::SeqCst) { return Ok(()); }
                    println!("☀️ [渲染] 唤醒屏幕");
                    current_channel = 1;
                    screen.power(true, args.light_level)?;
                    continue;
                } else {
                    // Toggle 开屏
                    current_channel = 1;
                }
            } else if cmd > 0 {
                let total = modules.len();
                let idx_1based = ((cmd as usize - 1) % total) + 1;
                if idx_1based != current_channel {
                    println!("🔄 [渲染] 切台：频道 {} -> {}", current_channel, idx_1based);
                    current_channel = idx_1based;
                }
            }
        }

        if !screen_on.load(Ordering::SeqCst) {
            screen.write_data(b"        ", 0)?;
            wait_for_wakeup(&mut rx, Arc::clone(&screen_on), Arc::clone(&running)).await;
            if !running.load(Ordering::SeqCst) { return Ok(()); }
            current_channel = 1;
            screen.power(true, args.light_level)?;
            continue;
        }

        // 2. 定时休眠检查
        if is_sleep_time(&args.sleep_start, &args.sleep_end) {
            println!("🌙 [渲染] 定时休眠 ({}-{})", args.sleep_start, args.sleep_end);
            screen.write_data(b"        ", 0)?;
            let sleep_sec = get_seconds_until_wake(&args.sleep_end);
            sleep_with_interrupt(sleep_sec, &mut rx, Arc::clone(&screen_on), Arc::clone(&running)).await;
            if !running.load(Ordering::SeqCst) { return Ok(()); }
            continue;
        }

        // 3. 显示当前模块
        let module_idx = current_channel - 1;
        let module = modules[module_idx];

        let interrupted = show_module_with_interrupt(
            screen, args, monitor, module, &mut rx, &mut current_channel, &modules,
        ).await?;

        if interrupted { continue; }

        current_channel = if current_channel >= modules.len() { 1 } else { current_channel + 1 };
    }
}

async fn wait_for_wakeup(
    rx: &mut watch::Receiver<i32>,
    screen_on: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    let mut last_seen = *rx.borrow();
    loop {
        if !running.load(Ordering::SeqCst) { return; }
        if screen_on.load(Ordering::SeqCst) { return; }
        if let Some(cmd) = poll_cmd(rx, &mut last_seen) {
            if cmd >= 1 || cmd == -1 {
                // 短按(>=1)唤醒，或长按(-1 Toggle)也唤醒
                screen_on.store(true, Ordering::SeqCst);
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn sleep_with_interrupt(
    total_secs: u64, rx: &mut watch::Receiver<i32>,
    screen_on: Arc<AtomicBool>, running: Arc<AtomicBool>,
) {
    let start = Instant::now();
    let total = Duration::from_secs(total_secs);
    let mut last_seen = *rx.borrow();
    while start.elapsed() < total {
        if !running.load(Ordering::SeqCst) { return; }
        if screen_on.load(Ordering::SeqCst) { return; }
        if let Some(cmd) = poll_cmd(rx, &mut last_seen) {
            if cmd >= 1 || cmd == -1 {
                screen_on.store(true, Ordering::SeqCst);
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn show_module_with_interrupt(
    screen: &mut led_screen::LedScreen, args: &Args, monitor: &mut SystemMonitor,
    module: &str, rx: &mut watch::Receiver<i32>, current_channel: &mut usize, modules: &[&str],
) -> Result<bool> {
    let total = modules.len();
    // 局部变量跟踪本函数内最后看到的指令值
    let mut last_seen = *rx.borrow();

    // 检查按键是否变化，变化则返回 true 表示需要打断
    let mut check_key = || -> bool {
        if let Some(cmd) = poll_cmd(rx, &mut last_seen) {
            if cmd == -1 { return true; }
            if cmd > 0 {
                *current_channel = ((cmd as usize - 1) % total) + 1;
                return true;
            }
        }
        false
    };

    match module {
        "year" => { screen.write_data(Local::now().format("%Y").to_string().as_bytes(), 0)?; }
        "date" => { screen.write_data(Local::now().format("%m-%d").to_string().as_bytes(), 0)?; }
        "time" => { screen.write_data(Local::now().format("%H:%M").to_string().as_bytes(), 1)?; }
        "timeBlink" => {
            let start = Instant::now();
            let mut time_flag = false;
            while start.elapsed() < Duration::from_secs(args.seconds) {
                if check_key() { return Ok(true); }
                let mut time_str = Local::now().format("%H:%M").to_string();
                if time_flag { time_str = time_str.replace(':', ";"); }
                screen.write_data(time_str.as_bytes(), 1)?;
                time_flag = !time_flag;
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if check_key() { return Ok(true); }
                }
            }
            return Ok(false);
        }
        "uptime" => screen.write_data(monitor.get_uptime_string().as_bytes(), 0)?,
        "cpu" => screen.write_data(monitor.get_cpu_usage_string().as_bytes(), 0)?,
        "mem" => screen.write_data(monitor.get_mem_string().as_bytes(), 0)?,
        "load" => screen.write_data(monitor.get_load_string().as_bytes(), 0)?,
        "temp" => screen.write_data(monitor.get_temps_by_ids(&args.temp_flag).as_bytes(), 0)?,
        "ip" => screen.write_data(monitor.get_public_ip(&args.ip_url).await.as_bytes(), 0)?,
        "netspeed_down" => screen.write_data(monitor.get_speed_string(0).as_bytes(), 8)?,
        "netspeed_up" => screen.write_data(monitor.get_speed_string(1).as_bytes(), 4)?,
        "dev" => screen.write_data(monitor.get_online_devices().as_bytes(), 0)?,
        "banner" => {
            let t = if !args.custom_text.is_empty() {args.custom_text.clone()} else {"Welcome".to_string()};
            screen.write_data(t.as_bytes(), 0)?;
        }
        "http_custom" => {
            let t = monitor.get_http_text(&args.custom_http_url, "", args.http_length).await;
            screen.write_data(t.as_bytes(), 0)?;
        }
        "traffic_down" => screen.write_data(monitor.get_total_rx_string().as_bytes(), 8)?,
        "traffic_up" => screen.write_data(monitor.get_total_tx_string().as_bytes(), 4)?,
        "weather" => {
            let full_text = monitor.get_smart_weather(&args.weather_city, &args.weather_source, &args.seniverse_key).await;
            let (static_icon, raw_rest) = match full_text.split_once(' ') {
                Some((icon, rest)) => (icon, rest),
                None => { screen.write_data(full_text.as_bytes(), 0)?; return Ok(sleep_with_key_check(args.seconds, rx, current_channel, total).await); }
            };
            let clean_rest = raw_rest.trim();
            let temp_part_str = if args.weather_format == "simple" {
                let mut temp_val = String::new();
                for (i, c) in clean_rest.chars().enumerate() {
                    if (i==0 && c=='-') || c.is_ascii_digit() || c=='.' { temp_val.push(c); } else { break; }
                }
                if temp_val.starts_with('-') { temp_val } else { format!("{}℃", temp_val) }
            } else { format!(" {}", clean_rest) };

            let start = Instant::now();
            let mut frame_flag = true;
            while start.elapsed() < Duration::from_secs(args.seconds) {
                if check_key() { return Ok(true); }
                let dynamic_icon = monitor.get_animated_icon(static_icon, frame_flag);
                let display_text = format!("{}{}", dynamic_icon, temp_part_str);
                screen.write_data(display_text.as_bytes(), 0)?;
                frame_flag = !frame_flag;
                for _ in 0..2 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    if check_key() { return Ok(true); }
                }
            }
            return Ok(false);
        }
        "stock" => {
            let (txt, flag) = monitor.get_stock_trend(&args.stock_url).await;
            screen.write_data(txt.as_bytes(), flag)?;
        }
        _ => return Ok(false),
    }
    // [修复] write_data 内部 flow 滚动可能被按键打断
    // flow 的 poll_interrupt 用屏幕自己的 last_seen, 不影响本函数的 last_seen
    // 所以这里用 check_key 能检测到 flow 期间发生的按键事件, 立即返回中断
    if check_key() { return Ok(true); }
    drop(check_key);  // 释放对 rx 的借用, 让 sleep_with_key_check 能使用
    Ok(sleep_with_key_check(args.seconds, rx, current_channel, total).await)
}

async fn sleep_with_key_check(secs: u64, rx: &mut watch::Receiver<i32>, current_channel: &mut usize, total_modules: usize) -> bool {
    if secs == 0 { return false; }
    let start = Instant::now();
    let total = Duration::from_secs(secs);
    let mut last_seen = *rx.borrow();
    while start.elapsed() < total {
        if let Some(cmd) = poll_cmd(rx, &mut last_seen) {
            if cmd == -1 { return true; }
            if cmd > 0 {
                let idx = ((cmd as usize - 1) % total_modules) + 1;
                *current_channel = idx;
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}
