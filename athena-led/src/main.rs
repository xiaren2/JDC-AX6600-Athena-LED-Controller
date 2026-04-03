mod led_screen;
mod char_dict;

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::fs;
use std::time::{Duration, Instant};
use tokio::time;
use tokio::signal::unix::{signal, SignalKind};
use chrono::{Local, NaiveTime, Timelike};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use regex::Regex;

// --- 天气结构体保持不变 ---
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
struct SeniverseResponse {
    results: Vec<SeniverseResult>,
}

#[derive(Deserialize, Debug)]
struct SeniverseResult {
    daily: Vec<SeniverseDaily>,
}

#[derive(Deserialize, Debug)]
struct SeniverseDaily {
    high: String,
    low: String,
    code_day: String,
}

#[derive(Deserialize, Debug)]
struct WttrResult {
    current_condition: Vec<WttrCurrent>,
    weather: Vec<WttrDaily>,
}

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrCurrent {
    temp_C: String,
    weatherDesc: Vec<WttrValue>,
}

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct WttrDaily {
    maxtempC: String,
    mintempC: String,
}

#[derive(Deserialize, Debug)]
struct WttrValue {
    value: String,
}

#[derive(Deserialize, Debug)]
struct OmGeoResponse {
    results: Option<Vec<OmLocation>>,
}

#[derive(Deserialize, Debug)]
struct OmLocation {
    name: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize, Debug)]
struct OmWeatherResponse {
    current_weather: OmCurrentWeather,
}

#[derive(Deserialize, Debug)]
struct OmCurrentWeather {
    temperature: f64,
    weathercode: u8,
}

// --- 核心结构体保持不变 ---
struct SystemMonitor {
    net_interface: String,
    http_client: Client,
    
    last_rx_bytes: u64,
    last_tx_bytes: u64,
    last_net_check: Instant,
    
    last_cpu_total: u64,
    last_cpu_idle: u64,
    last_stock_price: f64,
    
    cached_weather: String,
    last_weather_time: Instant,
    
    cached_ip: String,
    last_ip_time: Instant,
}

impl SystemMonitor {
    fn new(net_dev: String) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Athena-LED Router)")
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            http_client: client,
            net_interface: net_dev,
            cached_weather: "Wait...".to_string(),
            last_weather_time: Instant::now() - Duration::from_secs(3600 * 24),
            cached_ip: "Checking...".to_string(),
            last_ip_time: Instant::now() - Duration::from_secs(3600 * 24),
            last_rx_bytes: 0,
            last_tx_bytes: 0,
            last_net_check: Instant::now(),
            last_cpu_total: 0,
            last_cpu_idle: 0,
            last_stock_price: 0.0,
        })
    }
    
    fn init(&mut self) {
        let (rx, tx) = self.read_net_bytes();
        self.last_rx_bytes = rx;
        self.last_tx_bytes = tx;
        
        let (total, idle) = self.read_cpu_stats();
        self.last_cpu_total = total;
        self.last_cpu_idle = idle;
    }
    
    fn get_total_traffic(&self) -> String {
        let (rx, tx) = self.read_net_bytes();
        let format_bytes = |bytes: u64| -> String {
            if bytes > 1024 * 1024 * 1024 {
                format!("{:.1}G", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
            } else {
                format!("{:.0}M", bytes as f64 / 1024.0 / 1024.0)
            }
        };
        format!("T:{}/{}", format_bytes(rx), format_bytes(tx))
    }
    
    fn get_animated_icon(&self, static_icon: &str, frame_toggle: bool) -> String {
        match static_icon {
            "☀" => if frame_toggle { "☀".to_string() } else { "☼".to_string() },
            "☂" => if frame_toggle { "☂".to_string() } else { "☔".to_string() },
            "☁" => if frame_toggle { "☁".to_string() } else { "🌥".to_string() },
            "❄" => if frame_toggle { "❄".to_string() } else { "❅".to_string() },
            "⚡" => if frame_toggle { "⚡".to_string() } else { "☇".to_string() },
            "🌫" => "🌫".to_string(),
            _ => static_icon.to_string(),
        }
    }
    
    fn read_net_bytes(&self) -> (u64, u64) {
        let path = "/proc/net/dev";
        let content = fs::read_to_string(path).unwrap_or_default();
        
        for line in content.lines() {
            if line.contains(&self.net_interface) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let rx_idx = if parts[0].contains(':') { 1 } else { 2 };
                let tx_idx = rx_idx + 8;
                
                if parts.len() > tx_idx {
                    let rx = parts[rx_idx].parse::<u64>().unwrap_or(0);
                    let tx = parts[tx_idx].parse::<u64>().unwrap_or(0);
                    return (rx, tx);
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
                    let iowait: u64 = parts.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let irq: u64 = parts.get(6).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let softirq: u64 = parts.get(7).and_then(|s| s.parse().ok()).unwrap_or(0);
                    
                    let total = user + nice + system + idle + iowait + irq + softirq;
                    return (total, idle);
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
            self.last_rx_bytes = curr_rx;
            self.last_tx_bytes = curr_tx;
            self.last_net_check = now;
            return format_bytes_speed(0.0);
        }
        
        let speed = if mode == 0 {
            (curr_rx.saturating_sub(self.last_rx_bytes)) as f64 / duration
        } else {
            (curr_tx.saturating_sub(self.last_tx_bytes)) as f64 / duration
        };
        
        self.last_rx_bytes = curr_rx;
        self.last_tx_bytes = curr_tx;
        self.last_net_check = now;
        
        format_bytes_speed(speed)
    }
    
    fn get_traffic_total_string(&self) -> String {
        let (curr_rx, curr_tx) = self.read_net_bytes();
        format!("T:{}", format_bytes_total(curr_rx + curr_tx))
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
        
        self.last_cpu_total = curr_total;
        self.last_cpu_idle = curr_idle;
        
        if diff_total == 0 { return "CPU:-".to_string(); }
        
        let usage = 100.0 * (1.0 - (diff_idle as f64 / diff_total as f64));
        format!("C:{:.0}%", usage)
    }
    
    fn get_mem_string(&self) -> String {
        let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mut total = 0.0;
        let mut available = 0.0;
        
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 { continue; }
            match parts[0] {
                "MemTotal:" => total = parts[1].parse().unwrap_or(0.0),
                "MemAvailable:" => available = parts[1].parse().unwrap_or(0.0),
                _ => {}
            }
        }
        
        if total > 0.0 {
            let usage_percent = 100.0 * (1.0 - (available / total));
            format!("M:{:.0}%", usage_percent)
        } else {
            "M:Err".to_string()
        }
    }
    
    fn get_load_string(&self) -> String {
        let content = fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let parts: Vec<&str> = content.split_whitespace().collect();
        if !parts.is_empty() {
            format!("L:{}", parts[0])
        } else {
            "L:Err".to_string()
        }
    }
    
    fn get_uptime_string(&self) -> String {
        if let Ok(content) = fs::read_to_string("/proc/uptime") {
            if let Some(sec_str) = content.split_whitespace().next() {
                if let Ok(seconds) = sec_str.parse::<f64>() {
                    let secs = seconds as u64;
                    let days = secs / 86400;
                    let hours = (secs % 86400) / 3600;
                    let mins = (secs % 3600) / 60;
                    
                    if days > 0 {
                        return format!("Up:{}d{}h", days, hours);
                    } else if hours > 0 {
                        return format!("Up:{}h{}m", hours, mins);
                    } else {
                        return format!("Up:{}m", mins);
                    }
                }
            }
        }
        "Up:Err".to_string()
    }
    
    fn get_temps_by_ids(&self, ids: &str) -> String {
        let mut results = Vec::new();
        let id_list: Vec<&str> = ids.split(|c| c == ' ' || c == ',')
                                  .filter(|s| !s.is_empty())
                                  .collect();
        
        for id in id_list {
            let type_path = format!("/sys/class/thermal/thermal_zone{}/type", id);
            let temp_path = format!("/sys/class/thermal/thermal_zone{}/temp", id);
            
            if let Ok(type_name_raw) = fs::read_to_string(&type_path) {
                let label = type_name_raw.trim().to_lowercase().replace("-thermal", "");
                
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(raw_temp) = temp_str.trim().parse::<f64>() {
                        let val = if raw_temp > 1000.0 { raw_temp / 1000.0 } else { raw_temp };
                        results.push(format!("{}:{:.0}℃", label, val));
                    }
                }
            }
        }
        
        if results.is_empty() {
            "Temp:--".to_string()
        } else {
            results.join(" ")
        }
    }
    
    fn get_online_devices(&self) -> String {
        if let Ok(content) = fs::read_to_string("/proc/net/arp") {
            let count = content.lines().count();
            if count > 1 {
                return format!("Dev:{}", count - 1);
            }
        }
        "Dev:0".to_string()
    }
    
    pub async fn get_http_text(&self, url: &str, prefix: &str, max_len: usize) -> String {
        if url.is_empty() {
            return String::new();
        }
        
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or(self.http_client.clone());
        
        match client.get(url).send().await {
            Ok(resp) => {
                match resp.text().await {
                    Ok(text) => {
                        let clean_text = text.trim();
                        let truncated: String = clean_text.chars().take(max_len).collect();
                        format!("{}{}", prefix, truncated)
                    }
                    Err(_) => format!("{}Err", prefix),
                }
            }
            Err(_) => format!("{}Wait", prefix),
        }
    }
    
    async fn get_public_ip(&mut self, ip_url: &str) -> String {
        if self.last_ip_time.elapsed() < Duration::from_secs(3600) {
            if !self.cached_ip.contains("Err") {
                return self.cached_ip.clone();
            }
        }
        
        let mut new_ip = "IP:Err".to_string();
        
        match self.http_client.get(ip_url).send().await {
            Ok(resp) => {
                if let Ok(text) = resp.text().await {
                    let re = Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();
                    if let Some(mat) = re.find(&text) {
                        new_ip = format!("IP:{}", mat.as_str());
                    }
                }
            }
            Err(e) => println!("IP Request error: {:?}", e),
        }
        
        if !new_ip.contains("Err") {
            self.cached_ip = new_ip.clone();
            self.last_ip_time = Instant::now();
        }
        
        new_ip
    }
    
    async fn get_stock_trend(&mut self, url: &str) -> (String, u8) {
        if url.is_empty() { return (String::new(), 0); }
        
        match self.http_client.get(url).send().await {
            Ok(resp) => {
                if let Ok(json_val) = resp.json::<Value>().await {
                    let price_opt = json_val["price"].as_f64()
                        .or_else(|| json_val["price"].as_str().and_then(|s| s.parse::<f64>().ok()))
                        .or_else(|| json_val["last"].as_f64())
                        .or_else(|| json_val["close"].as_f64());
                    
                    if let Some(current_price) = price_opt {
                        let mut flag = 2;
                        
                        if self.last_stock_price > 0.0 {
                            if current_price > self.last_stock_price {
                                flag = 4;
                            } else if current_price < self.last_stock_price {
                                flag = 8;
                            }
                        }
                        
                        self.last_stock_price = current_price;
                        
                        let text = if current_price > 1000.0 {
                            format!("{:.0}", current_price)
                        } else {
                            format!("{:.2}", current_price)
                        };
                        
                        return (text, flag);
                    }
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
            self.cached_weather = result.clone();
            self.last_weather_time = Instant::now();
        }
        
        result
    }
    
    async fn get_weather_from_uapis(&self, city: &str) -> String {
        let url = format!("https://uapis.cn/api/v1/misc/weather?city={}&forecast=true", city);
        
        match self.http_client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(data) = resp.json::<WeatherResponse>().await {
                    let temp = data.temperature;
                    let max = data.temp_max.unwrap_or(temp);
                    let min = data.temp_min.unwrap_or(temp);
                    
                    let desc = data.weather;
                    let icon = if desc.contains("雨") { "☂" }
                    else if desc.contains("雪") { "❄" }
                    else if desc.contains("云") || desc.contains("阴") || desc.contains("雾") || desc.contains("霾") { "☁" }
                    else { "☀" };
                    
                    return format!("{} {:.0}℃ {:.0}-{:.0}", icon, temp, min, max);
                }
            }
            Err(_) => {}
        }
        "W:Err(U)".to_string()
    }
    
    async fn get_weather_from_wttr(&self, city: &str) -> String {
        let url = format!("https://wttr.in/{}?format=j1", city);
        
        match self.http_client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    println!("DEBUG: Wttr failed status: {}", resp.status());
                    return format!("W:Err({})", resp.status().as_u16());
                }
                
                match resp.json::<WttrResult>().await {
                    Ok(json) => {
                        if let (Some(curr), Some(daily)) = (json.current_condition.first(), json.weather.first()) {
                            let temp = &curr.temp_C;
                            let max = &daily.maxtempC;
                            let min = &daily.mintempC;
                            
                            let desc = curr.weatherDesc.first()
                                .map(|d| d.value.to_lowercase())
                                .unwrap_or_else(|| "unknown".to_string());
                            
                            let icon = if desc.contains("rain") || desc.contains("shower") || desc.contains("drizzle") { "☂" }
                            else if desc.contains("snow") || desc.contains("ice") || desc.contains("hail") { "❄" }
                            else if desc.contains("thunder") { "⚡" }
                            else if desc.contains("cloud") || desc.contains("overcast") { "☁" }
                            else if desc.contains("mist") || desc.contains("fog") { "🌫" }
                            else { "☀" };
                            
                            return format!("{} {}℃ {}-{}", icon, temp, min, max);
                        }
                        println!("DEBUG: Wttr JSON structure mismatch (empty arrays)");
                        "W:DataErr".to_string()
                    }
                    Err(e) => {
                        println!("DEBUG: Wttr JSON Parse Error: {:?}", e);
                        "W:JsonErr".to_string()
                    }
                }
            }
            Err(e) => {
                println!("DEBUG: Wttr Network Error: {:?}", e);
                "W:NetErr".to_string()
            }
        }
    }
    
    async fn get_weather_from_seniverse(&self, location: &str, key: &str) -> String {
        let url = format!(
            "https://api.seniverse.com/v3/weather/daily.json?key={}&location={}&language=en&unit=c&start=0&days=1",
            key, location
        );
        
        match self.http_client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(json) = resp.json::<SeniverseResponse>().await {
                    if let Some(daily) = json.results.get(0).and_then(|r| r.daily.get(0)) {
                        let max = daily.high.parse::<f64>().unwrap_or(0.0);
                        let min = daily.low.parse::<f64>().unwrap_or(0.0);
                        let temp = (max + min) / 2.0;
                        
                        let code = daily.code_day.parse::<i32>().unwrap_or(99);
                        let icon = match code {
                            0..=3 => "☀",
                            4..=9 => "☁",
                            10..=19 => "☂",
                            20..=29 => "❄",
                            30..=36 => "☁",
                            _ => "☀",
                        };
                        
                        return format!("{} {:.0}℃ {:.0}-{:.0}", icon, temp, min, max);
                    }
                }
            }
            Err(_) => {}
        }
        "W:Err(S)".to_string()
    }
    
    async fn get_weather_from_open_meteo(&self, city: &str) -> String {
        let geo_url = format!(
            "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh&format=json",
            city
        );
        
        let (lat, lon) = match self.http_client.get(&geo_url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() { return "W:GeoErr".to_string(); }
                match resp.json::<OmGeoResponse>().await {
                    Ok(data) => {
                        if let Some(results) = data.results {
                            if let Some(loc) = results.first() {
                                (loc.latitude, loc.longitude)
                            } else { return "W:NoCity".to_string(); }
                        } else { return "W:NoCity".to_string(); }
                    }
                    Err(_) => return "W:GeoJson".to_string(),
                }
            }
            Err(_) => return "W:GeoNet".to_string(),
        };
        
        let weather_url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current_weather=true",
            lat, lon
        );
        
        match self.http_client.get(&weather_url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() { return "W:ApiErr".to_string(); }
                match resp.json::<OmWeatherResponse>().await {
                    Ok(data) => {
                        let temp = data.current_weather.temperature;
                        let code = data.current_weather.weathercode;
                        
                        let icon = match code {
                            0 => "☀",
                            1 | 2 | 3 => "☁",
                            45 | 48 => "🌫",
                            51..=67 | 80..=82 => "☂",
                            71..=77 | 85..=86 => "❄",
                            95..=99 => "⚡",
                            _ => "?",
                        };
                        
                        return format!("{} {:.1}℃", icon, temp);
                    }
                    Err(_) => "W:JsonErr".to_string(),
                }
            }
            Err(_) => "W:NetErr".to_string(),
        }
    }
}

fn format_bytes_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec > 1_048_576.0 {
        format!("{:.1}M", bytes_per_sec / 1_048_576.0)
    } else if bytes_per_sec > 1024.0 {
        format!("{:.0}K", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0}B", bytes_per_sec)
    }
}

fn format_bytes_total(bytes: u64) -> String {
    let b = bytes as f64;
    if b > 1_099_511_627_776.0 {
        format!("{:.2}T", b / 1_099_511_627_776.0)
    } else if b > 1_073_741_824.0 {
        format!("{:.2}G", b / 1_073_741_824.0)
    } else if b > 1_048_576.0 {
        format!("{:.1}M", b / 1_048_576.0)
    } else {
        format!("{:.0}K", b / 1024.0)
    }
}

fn get_seconds_until_wake(wake_time_str: &str) -> u64 {
    let now = Local::now();
    
    let wake_time = match NaiveTime::parse_from_str(wake_time_str, "%H:%M") {
        Ok(t) => t,
        Err(_) => return 60,
    };
    
    let mut target_dt = now.date_naive().and_time(wake_time).and_local_timezone(Local).unwrap();
    
    if target_dt <= now {
        target_dt = target_dt + chrono::Duration::days(1);
    }
    
    let duration = target_dt.signed_duration_since(now).num_seconds();
    
    if duration > 0 {
        (duration as u64) + 2
    } else {
        60
    }
}

fn is_sleep_time(start_str: &str, end_str: &str) -> bool {
    if start_str.is_empty() || end_str.is_empty() {
        return false;
    }
    
    let start = match NaiveTime::parse_from_str(start_str, "%H:%M") {
        Ok(t) => t,
        Err(_) => return false,
    };
    let end = match NaiveTime::parse_from_str(end_str, "%H:%M") {
        Ok(t) => t,
        Err(_) => return false,
    };
    
    let now = Local::now().time();
    
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t = 5)]
    seconds: u64,
    
    #[arg(long, default_value_t = 5)]
    light_level: u8,
    
    #[arg(long, default_value = "date timeBlink weather stock uptime netspeed_down netspeed_up cpu")]
    display_order: String,
    
    #[arg(long, default_value = "br-lan")]
    net_interface: String,
    
    #[arg(long, default_value = "http://members.3322.org/dyndns/getip")]
    ip_url: String,
    
    #[arg(long, default_value = "")]
    custom_text: String,
    
    #[arg(long, default_value = "")]
    custom_http_url: String,
    
    #[arg(long, default_value_t = 15)]
    http_length: usize,
    
    #[arg(long, default_value = "Beijing")]
    weather_city: String,
    
    #[arg(long, default_value = "uapis")]
    weather_source: String,
    
    #[arg(long, default_value = "S140W1C6_1_8R8_8c")]
    seniverse_key: String,
    
    #[arg(long, default_value = "")]
    stock_url: String,
    
    #[arg(long, default_value = "4")]
    temp_flag: String,
    
    #[arg(long, default_value = "")]
    sleep_start: String,
    
    #[arg(long, default_value = "")]
    sleep_end: String,
    
    #[arg(long, default_value = "simple")]
    weather_format: String,
}

fn set_timezone_from_config() -> Result<()> {
    let content = fs::read_to_string("/etc/config/system")?;
    for line in content.lines() {
        if line.contains("CST-8") { 
            env::set_var("TZ", "Asia/Shanghai"); 
            return Ok(()); 
        }
    }
    env::set_var("TZ", "UTC");
    Ok(())
}

// ================= 🔥 新增：按键处理函数 =================
fn handle_click() -> Result<()> {
    // 防止多进程冲突
    std::process::Command::new("killall")
        .arg("athena_led")
        .output()
        .ok();

    let mode_file = "/tmp/led_mode";
    let mode = std::fs::read_to_string(mode_file).unwrap_or("0".to_string());
    let mut mode_num: u8 = mode.trim().parse().unwrap_or(0);
    
    mode_num = (mode_num + 1) % 3;
    std::fs::write(mode_file, mode_num.to_string()).ok();
    
    std::process::Command::new("logger")
        .arg(format!("LED mode -> {}", mode_num))
        .output()
        .ok();

    // 重启主程序
    std::process::Command::new("/usr/bin/athena_led")
        .spawn()
        .ok();
    
    Ok(())
}

fn handle_long_press() -> Result<()> {
    // 杀掉程序
    std::process::Command::new("killall")
        .arg("athena_led")
        .output()
        .ok();
    
    // 关屏
    let mut screen = led_screen::LedScreen::new(581, 582, 585, 586)?;
    screen.power(false, 0)?;
    
    std::process::Command::new("logger")
        .arg("LED OFF by long press")
        .output()
        .ok();
    
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // ====== ✅【新增：按键入口】======
    let args_raw: Vec<String> = std::env::args().collect();
    
    if args_raw.len() > 1 {
        match args_raw[1].as_str() {
            "click" => {
                handle_click()?;
                return Ok(());
            }
            "long" => {
                handle_long_press()?;
                return Ok(());
            }
            _ => {}
        }
    }
    // =================================
    
    // ====== 原来的逻辑 ======
    let args = Args::parse();
    
    let mut screen = led_screen::LedScreen::new(581, 582, 585, 586)
        .context("Failed to init screen")?;
    
    screen.power(true, args.light_level)?;
    
    let mut monitor = SystemMonitor::new(args.net_interface.clone())
        .context("Failed to initialize system monitor")?;
    monitor.init();
    
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    
    loop {
        tokio::select! {
            _ = sigterm.recv() => { 
                screen.power(false, 0)?; 
                break; 
            },
            _ = sigint.recv() => { 
                screen.power(false, 0)?; 
                break; 
            },
            _ = process_loop(&mut screen, &args, &mut monitor) => {},
        }
    }
    
    Ok(())
}

async fn process_loop(
    screen: &mut led_screen::LedScreen,
    args: &Args,
    monitor: &mut SystemMonitor
) -> Result<()> {
    let modules: Vec<&str> = args.display_order.split_whitespace().collect();
    
    for module in modules {
        if is_sleep_time(&args.sleep_start, &args.sleep_end) {
            screen.write_data(b"        ", 0)?;
            let sleep_sec = get_seconds_until_wake(&args.sleep_end);
            tokio::time::sleep(tokio::time::Duration::from_secs(sleep_sec)).await;
            return Ok(());
        }
        
        let mut current_flag = 0;
        let mut text_to_show = String::new();
        
        match module {
            "year" => {
                text_to_show = Local::now().format("%Y").to_string();
            },
            "date" => {
                text_to_show = Local::now().format("%m-%d").to_string();
            },
            "time" => {
                text_to_show = Local::now().format("%H:%M").to_string();
                current_flag |= 1;
            },
            "timeBlink" => {
                current_flag |= 1;
                let start = Instant::now();
                let mut time_flag = false;
                
                while start.elapsed() < Duration::from_secs(args.seconds) {
                    let mut time_str = Local::now().format("%H:%M").to_string();
                    if time_flag {
                        time_str = time_str.replace(':', ";");
                    }
                    
                    screen.write_data(time_str.as_bytes(), current_flag)?;
                    
                    time_flag = !time_flag;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                continue;
            },
            "uptime" => {
                text_to_show = monitor.get_uptime_string();
            },
            "cpu" => {
                text_to_show = monitor.get_cpu_usage_string();
            },
            "mem" => {
                text_to_show = monitor.get_mem_string();
            },
            "load" => {
                text_to_show = monitor.get_load_string();
            },
            "temp" => {
                text_to_show = monitor.get_temps_by_ids(&args.temp_flag);
            },
            "ip" => {
                text_to_show = monitor.get_public_ip(&args.ip_url).await;
            },
            "netspeed_down" => {
                text_to_show = monitor.get_speed_string(0);
                current_flag |= 8;
            },
            "netspeed_up" => {
                text_to_show = monitor.get_speed_string(1);
                current_flag |= 4;
            },
            "traffic_down" => {
                text_to_show = monitor.get_total_rx_string();
                current_flag |= 8;
            },
            "traffic_up" => {
                text_to_show = monitor.get_total_tx_string();
                current_flag |= 4;
            },
            "dev" => {
                text_to_show = monitor.get_online_devices();
            },
            "banner" => {
                if !args.custom_text.is_empty() {
                    text_to_show = args.custom_text.clone();
                } else {
                    text_to_show = "Welcome".to_string();
                }
            },
            "http_custom" => {
                text_to_show = monitor.get_http_text(
                    &args.custom_http_url,
                    "",
                    args.http_length
                ).await;
            },
            "weather" => {
                let full_text = monitor.get_smart_weather(
                    &args.weather_city,
                    &args.weather_source,
                    &args.seniverse_key
                ).await;
                
                let (static_icon, raw_rest) = match full_text.split_once(' ') {
                    Some((icon, rest)) => (icon, rest),
                    None => {
                        screen.write_data(full_text.as_bytes(), current_flag)?;
                        continue;
                    }
                };
                
                let clean_rest = raw_rest.trim();
                let temp_part_str = if args.weather_format == "simple" {
                    let mut temp_val = String::new();
                    for (i, c) in clean_rest.chars().enumerate() {
                        if (i == 0 && c == '-') || c.is_ascii_digit() || c == '.' {
                            temp_val.push(c);
                        } else {
                            break;
                        }
                    }
                    if temp_val.starts_with('-') {
                        temp_val
                    } else {
                        format!("{}℃", temp_val)
                    }
                } else {
                    format!(" {}", clean_rest)
                };
                
                let start = Instant::now();
                let mut frame_flag = true;
                
                while start.elapsed() < Duration::from_secs(args.seconds) {
                    let dynamic_icon = monitor.get_animated_icon(static_icon, frame_flag);
                    let display_text = format!("{}{}", dynamic_icon, temp_part_str);
                    
                    screen.write_data(display_text.as_bytes(), current_flag)?;
                    
                    frame_flag = !frame_flag;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                continue;
            },
            "stock" => {
                let (txt, flag) = monitor.get_stock_trend(&args.stock_url).await;
                text_to_show = txt;
                current_flag |= flag;
            },
            _ => continue,
        }
        
        if !text_to_show.is_empty() {
            screen.write_data(text_to_show.as_bytes(), current_flag)?;
            tokio::time::sleep(tokio::time::Duration::from_secs(args.seconds)).await;
        }
    }
    
    Ok(())
}
