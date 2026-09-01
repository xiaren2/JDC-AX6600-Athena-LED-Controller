// ==========================================
// 条件编译: unix 平台用真实 GPIO; 非 unix 平台加载模拟器 stub
// ==========================================
#[cfg(unix)]
mod led_screen;
#[cfg(not(unix))]
#[path = "led_screen_sim.rs"]
mod led_screen;

#[cfg(unix)]
mod char_dict;
#[cfg(not(unix))]
#[path = "char_dict_sim.rs"]
mod char_dict;

mod button;
mod control;
mod net_agent;

use anyhow::{Context, Result};
use clap::Parser;
use led_screen::{GpioBackend, LedScreen};
use net_agent::{AgentConfig, NetAgent};
use std::env;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use chrono::{Local, NaiveTime};

// --- 系统监控器 (纯本地数据采集, 网络数据已移至 NetAgent) ---
struct SystemMonitor {
    net_interface: String,
    last_rx_bytes: u64, last_tx_bytes: u64, last_net_check: Instant,
    last_cpu_total: u64, last_cpu_idle: u64,
}

impl SystemMonitor {
    fn new(net_dev: String) -> Result<Self> {
        Ok(Self {
            net_interface: net_dev,
            last_rx_bytes: 0, last_tx_bytes: 0, last_net_check: Instant::now(),
            last_cpu_total: 0, last_cpu_idle: 0,
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
    // ★ 不再内嵌公共测试 Key, 避免被滥用封禁 (用户自己在 LuCI 里配置)
    #[arg(long, default_value = "")] seniverse_key: String,
    #[arg(long, default_value = "")] stock_url: String,
    #[arg(long, default_value = "4")] temp_flag: String,
    #[arg(long, default_value = "")] sleep_start: String,
    #[arg(long, default_value = "")] sleep_end: String,
    #[arg(long, default_value = "simple")] weather_format: String,

    // ★ 网络请求缓存间隔 (秒): NetAgent 按这个周期刷新天气/IP/股票/HTTP
    //   调大可避免被第三方 API 拉黑; 小则及时. 默认 5 分钟 (300s)
    #[arg(long, default_value_t = 300)]
    http_cache_secs: u64,

    // ★ GPIO 后端: auto 优先字符设备，失败回退 sysfs (核心修复! 兼容 base≠512 的固件)
    #[arg(long, default_value = "auto")]
    gpio_backend: String,

    // [按键] GPIO 引脚偏移，AX6600 按键固定为 71
    #[arg(long, default_value = "71")]
    pub button_gpio: String,

    // [GPIO] 基址: sysfs 后端时换算全局编号 (auto / 512 / 432 / 0)
    #[arg(long, default_value = "auto")]
    pub gpio_base: String,

    // [Mesh 键]
    #[arg(long, default_value_t = 0)] pub enable_mesh_button: u8,
    #[arg(long, default_value = "72")] pub mesh_button_gpio: String,
    #[arg(long, default_value = "none")] pub mesh_short_action: String,
    #[arg(long, default_value = "none")] pub mesh_long_action: String,

    // [4 盏状态 LED 独立开关]
    #[arg(long, default_value_t = false)] pub disable_led_clock: bool,
    #[arg(long, default_value_t = false)] pub disable_led_medal: bool,
    #[arg(long, default_value_t = false)] pub disable_led_up: bool,
    #[arg(long, default_value_t = false)] pub disable_led_down: bool,
}

fn set_timezone_from_config() -> Result<()> {
    let content = fs::read_to_string("/etc/config/system").unwrap_or_default();
    let mut timezone_posix: Option<String> = None;
    let mut zonename_iana: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = parse_uci_option(trimmed, "timezone") {
            timezone_posix = Some(val);
        } else if let Some(val) = parse_uci_option(trimmed, "zonename") {
            zonename_iana = Some(val);
        }
    }
    if let Some(ref tz) = timezone_posix {
        if !tz.is_empty() && tz != "UTC" {
            env::set_var("TZ", tz);
            println!("🌍 [时区] 使用 timezone 字段: TZ={}", tz);
            return Ok(());
        }
    }
    if let Some(ref zn) = zonename_iana {
        if !zn.is_empty() {
            let zoneinfo_path = format!("/usr/share/zoneinfo/{}", zn);
            if std::path::Path::new(&zoneinfo_path).exists() {
                env::set_var("TZ", zn);
                println!("🌍 [时区] 使用 zonename (zoneinfo 数据包存在): TZ={}", zn);
                return Ok(());
            }
            if let Some(posix_tz) = iana_to_posix(zn) {
                env::set_var("TZ", &posix_tz);
                println!("🌍 [时区] 使用 zonename 转换: {} -> TZ={}", zn, posix_tz);
                return Ok(());
            }
            println!("⚠️ [时区] 未知 zonename '{}', 回退 UTC", zn);
        }
    }
    env::set_var("TZ", "UTC");
    println!("🌍 [时区] 未找到时区配置, 使用 UTC");
    Ok(())
}
fn parse_uci_option(line: &str, key: &str) -> Option<String> {
    let prefix = format!("option {}", key);
    let rest = line.trim().strip_prefix(&prefix)?;
    let rest = rest.trim().trim_matches('\'').trim_matches('"');
    if rest.is_empty() { None } else { Some(rest.to_string()) }
}
fn iana_to_posix(zonename: &str) -> Option<String> {
    let table: &[(&str, &str)] = &[
        ("Asia/Shanghai", "CST-8"), ("Asia/Chongqing", "CST-8"),
        ("Asia/Harbin", "CST-8"), ("Asia/Urumqi", "CST-8"),
        ("Asia/Kashgar", "CST-8"), ("Asia/Hong_Kong", "HKT-8"),
        ("Asia/Macau", "CST-8"), ("Asia/Taipei", "CST-8"),
        ("Asia/Singapore", "SGT-8"), ("Asia/Kuala_Lumpur", "MYT-8"),
        ("Asia/Manila", "PHT-8"), ("Asia/Makassar", "WITA-8"),
        ("Asia/Brunei", "BNT-8"),
        ("Asia/Tokyo", "JST-9"), ("Asia/Seoul", "KST-9"), ("Asia/Pyongyang", "KST-9"),
        ("Asia/Bangkok", "ICT-7"), ("Asia/Jakarta", "WIB-7"),
        ("Asia/Ho_Chi_Minh", "ICT-7"), ("Asia/Phnom_Penh", "ICT-7"), ("Asia/Vientiane", "ICT-7"),
        ("Asia/Dhaka", "BST-6"), ("Asia/Almaty", "ALMT-6"), ("Asia/Omsk", "OMST-6"),
        ("Asia/Kolkata", "IST-5:30"), ("Asia/Calcutta", "IST-5:30"), ("Asia/Colombo", "IST-5:30"),
        ("Asia/Karachi", "PKT-5"), ("Asia/Tashkent", "UZT-5"), ("Asia/Yekaterinburg", "YEKT-5"),
        ("Asia/Dubai", "GST-4"), ("Asia/Baku", "AZT-4"), ("Indian/Mauritius", "MUT-4"),
        ("Europe/Moscow", "MSK-3"), ("Asia/Riyadh", "AST-3"), ("Asia/Tehran", "IRST-3:30"), ("Africa/Nairobi", "EAT-3"),
        ("Europe/Athens", "EET-2"), ("Europe/Bucharest", "EET-2"), ("Africa/Cairo", "EET-2"),
        ("Europe/Helsinki", "EET-2"), ("Asia/Jerusalem", "IST-2"),
        ("Europe/Paris", "CET-1"), ("Europe/Berlin", "CET-1"), ("Europe/Rome", "CET-1"),
        ("Europe/Madrid", "CET-1"), ("Europe/Amsterdam", "CET-1"), ("Europe/Brussels", "CET-1"),
        ("Europe/Vienna", "CET-1"), ("Europe/Stockholm", "CET-1"), ("Europe/Oslo", "CET-1"),
        ("Europe/Copenhagen", "CET-1"), ("Europe/Warsaw", "CET-1"), ("Europe/Prague", "CET-1"),
        ("Europe/Zurich", "CET-1"), ("Africa/Lagos", "WAT-1"),
        ("Europe/London", "GMT0"), ("Europe/Dublin", "GMT0"), ("Atlantic/Reykjavik", "GMT0"),
        ("America/Sao_Paulo", "BRT3"), ("America/Argentina/Buenos_Aires", "ART3"), ("America/Montevideo", "UYT3"),
        ("America/Halifax", "AST4"), ("America/Caracas", "VET4"), ("America/Santiago", "CLT4"),
        ("America/New_York", "EST5"), ("America/Toronto", "EST5"), ("America/Miami", "EST5"),
        ("America/Bogota", "COT5"), ("America/Lima", "PET5"),
        ("America/Chicago", "CST6"), ("America/Mexico_City", "CST6"), ("America/Denver", "MST7"),
        ("America/Los_Angeles", "PST8"), ("America/Tijuana", "PST8"), ("America/Vancouver", "PST8"),
        ("Pacific/Honolulu", "HST10"),
        ("Australia/Sydney", "AEST-10"), ("Australia/Melbourne", "AEST-10"), ("Australia/Brisbane", "AEST-10"),
        ("Australia/Adelaide", "ACST-9:30"), ("Australia/Darwin", "ACST-9:30"),
        ("Pacific/Auckland", "NZST-12"), ("Pacific/Fiji", "FJT-12"),
        ("America/St_Johns", "NST3:30"),
    ];
    for (name, posix) in table {
        if zonename == *name { return Some(posix.to_string()); }
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

    // ----- 4 盏独立状态 LED 禁用掩码 -----
    let mut disabled_led_mask: u8 = 0;
    if args.disable_led_clock { disabled_led_mask |= 0x01; }
    if args.disable_led_medal { disabled_led_mask |= 0x02; }
    if args.disable_led_up    { disabled_led_mask |= 0x04; }
    if args.disable_led_down  { disabled_led_mask |= 0x08; }

    // ----- 解析 GPIO 后端 + 基址 -----
    let backend: GpioBackend = args.gpio_backend.parse().unwrap_or(GpioBackend::Auto);
    let base: u64 = if args.gpio_base.eq_ignore_ascii_case("auto") {
        led_screen::detect_gpio_base()
    } else {
        args.gpio_base.parse::<u64>().unwrap_or_else(|_| led_screen::detect_gpio_base())
    };
    println!("🔌 [GPIO] backend={:?}, base={}, disabled_led_mask={}", backend, base, disabled_led_mask);
    let mut screen = LedScreen::new(backend, base, disabled_led_mask)
        .context("Failed to init screen")?;
    screen.power(true, args.light_level)?;

    let mut monitor = SystemMonitor::new(args.net_interface.clone())
        .context("Failed to initialize system monitor")?;
    monitor.init();

    let running = Arc::new(AtomicBool::new(true));
    let running_for_listener = Arc::clone(&running);
    let running_for_net = Arc::clone(&running);

    let control_state = control::new_shared();

    // ==========================================
    // ★ 启动 NetAgent 后台任务 (独立 tokio 线程异步刷新网络数据)
    //    从此渲染循环不再等 HTTP 请求!
    // ==========================================
    let agent_cfg = AgentConfig {
        cache_secs: args.http_cache_secs.max(10),
        ip_url: args.ip_url.clone(),
        weather_city: args.weather_city.clone(),
        weather_source: args.weather_source.clone(),
        seniverse_key: args.seniverse_key.clone(),
        stock_url: args.stock_url.clone(),
        custom_http_url: args.custom_http_url.clone(),
        http_length: args.http_length,
    };
    println!("🌐 [NetAgent] 缓存间隔 {}s. 启动后台刷新...", agent_cfg.cache_secs);
    let agent = NetAgent::new(agent_cfg).context("Failed to init NetAgent")?;
    let net_snapshot: Arc<RwLock<net_agent::NetSnapshot>> = agent.snapshot();
    let agent_running = running_for_net;
    tokio::spawn(async move { agent.run(agent_running).await });

    // ==========================================
    // 按键监听器 (GPIO 引脚 71)
    // ==========================================
    let (tx, rx) = watch::channel(1i32);
    let rx_for_screen = Arc::new(Mutex::new(rx.clone()));
    screen.bind_interrupt_rx(Arc::clone(&rx_for_screen));

    button::spawn_button_listener(
        tx.clone(),
        running_for_listener,
        args.button_gpio.clone(),
        args.gpio_base.clone(),
        Arc::clone(&control_state),
    );
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
            _ = process_loop(&mut screen, &args, &mut monitor, rx.clone(),
                             Arc::clone(&control_state), Arc::clone(&running),
                             Arc::clone(&net_snapshot)) => {},
        }
    }
    running.store(false, Ordering::SeqCst);
    Ok(())
}

fn poll_cmd(rx: &watch::Receiver<i32>, last_seen: &mut i32) -> Option<i32> {
    let cmd = *rx.borrow();
    if cmd != *last_seen {
        *last_seen = cmd;
        Some(cmd)
    } else { None }
}

async fn process_loop(
    screen: &mut LedScreen,
    args: &Args,
    monitor: &mut SystemMonitor,
    mut rx: watch::Receiver<i32>,
    control_state: control::SharedControl,
    running: Arc<AtomicBool>,
    net_snapshot: Arc<RwLock<net_agent::NetSnapshot>>,
) -> Result<()> {
    let modules: Vec<&str> = args.display_order.split_whitespace().collect();
    if modules.is_empty() { return Ok(()); }

    let mut current_channel: usize = 1;
    let mut last_seen_cmd = *rx.borrow();
    let screen_on = Arc::new(AtomicBool::new(true));

    loop {
        if !running.load(Ordering::SeqCst) { return Ok(()); }

        if let Ok(mut st) = control_state.lock() {
            if st.go_home {
                st.go_home = false;
                println!("⏮️ [调度] go_home=true，跳回频道 1");
                current_channel = 1;
            }
        }

        if let Some(cmd) = poll_cmd(&rx, &mut last_seen_cmd) {
            if cmd == -1 {
                let was_on = screen_on.load(Ordering::SeqCst);
                let now_on = !was_on;
                screen_on.store(now_on, Ordering::SeqCst);
                if !now_on {
                    println!("🌙 [渲染] 息屏，等待唤醒...");
                    screen.write_data(b"        ", 0).await?;
                    wait_for_wakeup(&mut rx, Arc::clone(&screen_on), Arc::clone(&running)).await;
                    if !running.load(Ordering::SeqCst) { return Ok(()); }
                    println!("☀️ [渲染] 唤醒屏幕");
                    current_channel = 1;
                    screen.power(true, args.light_level)?;
                    continue;
                } else {
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
            screen.write_data(b"        ", 0).await?;
            wait_for_wakeup(&mut rx, Arc::clone(&screen_on), Arc::clone(&running)).await;
            if !running.load(Ordering::SeqCst) { return Ok(()); }
            current_channel = 1;
            screen.power(true, args.light_level)?;
            continue;
        }

        if is_sleep_time(&args.sleep_start, &args.sleep_end) {
            println!("🌙 [渲染] 定时休眠 ({}-{})", args.sleep_start, args.sleep_end);
            screen.write_data(b"        ", 0).await?;
            let sleep_sec = get_seconds_until_wake(&args.sleep_end);
            sleep_with_interrupt(sleep_sec, &mut rx, Arc::clone(&screen_on), Arc::clone(&running)).await;
            if !running.load(Ordering::SeqCst) { return Ok(()); }
            continue;
        }

        let module_idx = current_channel - 1;
        let module = modules[module_idx];

        let interrupted = show_module_with_interrupt(
            screen, args, monitor, module, &mut rx, &mut current_channel, &modules, Arc::clone(&net_snapshot),
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

// ==========================================================
// 模块渲染: ★ 所有网络模块(weather/ip/stock/http_custom)直接读 net_snapshot, 不阻塞!
// ==========================================================
async fn show_module_with_interrupt(
    screen: &mut LedScreen, args: &Args, monitor: &mut SystemMonitor,
    module: &str, rx: &mut watch::Receiver<i32>, current_channel: &mut usize, modules: &[&str],
    net_snapshot: Arc<RwLock<net_agent::NetSnapshot>>,
) -> Result<bool> {
    let total = modules.len();
    let mut last_seen = *rx.borrow();

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

    // 取一次 snapshot (RwLock 读锁很快)
    let snap = net_snapshot.read().map(|g| g.clone()).unwrap_or_default();

    match module {
        "year"  => screen.write_data(Local::now().format("%Y").to_string().as_bytes(), 0).await?,
        "date"  => screen.write_data(Local::now().format("%m-%d").to_string().as_bytes(), 0).await?,
        "time"  => screen.write_data(Local::now().format("%H:%M").to_string().as_bytes(), 1).await?,
        "timeBlink" => {
            let start = Instant::now();
            let mut time_flag = false;
            while start.elapsed() < Duration::from_secs(args.seconds) {
                if check_key() { return Ok(true); }
                let mut time_str = Local::now().format("%H:%M").to_string();
                if time_flag { time_str = time_str.replace(':', ";"); }
                screen.write_data(time_str.as_bytes(), 1).await?;
                time_flag = !time_flag;
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if check_key() { return Ok(true); }
                }
            }
            return Ok(false);
        }
        "uptime"         => screen.write_data(monitor.get_uptime_string().as_bytes(), 0).await?,
        "cpu"            => screen.write_data(monitor.get_cpu_usage_string().as_bytes(), 0).await?,
        "mem"            => screen.write_data(monitor.get_mem_string().as_bytes(), 0).await?,
        "load"           => screen.write_data(monitor.get_load_string().as_bytes(), 0).await?,
        "temp"           => screen.write_data(monitor.get_temps_by_ids(&args.temp_flag).as_bytes(), 0).await?,
        // ★ 直接读快照, 不做任何 HTTP 请求!
        "ip"             => screen.write_data(snap.ip.as_bytes(), 0).await?,
        "netspeed_down"  => screen.write_data(monitor.get_speed_string(0).as_bytes(), 8).await?,
        "netspeed_up"    => screen.write_data(monitor.get_speed_string(1).as_bytes(), 4).await?,
        "dev"            => screen.write_data(monitor.get_online_devices().as_bytes(), 0).await?,
        "banner" => {
            let t = if !args.custom_text.is_empty() {args.custom_text.clone()} else {"Welcome".to_string()};
            screen.write_data(t.as_bytes(), 0).await?;
        }
        // ★ 自定义 HTTP 也直接读快照
        "http_custom" => screen.write_data(snap.http_custom.as_bytes(), 0).await?,
        "traffic_down" => screen.write_data(monitor.get_total_rx_string().as_bytes(), 8).await?,
        "traffic_up"   => screen.write_data(monitor.get_total_tx_string().as_bytes(), 4).await?,
        "weather" => {
            let full_text = snap.weather;
            let (static_icon, raw_rest) = match full_text.split_once(' ') {
                Some((icon, rest)) => (icon, rest),
                None => { screen.write_data(full_text.as_bytes(), 0).await?; return Ok(sleep_with_key_check(args.seconds, rx, current_channel, total).await); }
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
                screen.write_data(display_text.as_bytes(), 0).await?;
                frame_flag = !frame_flag;
                for _ in 0..2 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    if check_key() { return Ok(true); }
                }
            }
            return Ok(false);
        }
        "stock" => {
            let (txt, flag) = snap.stock;
            screen.write_data(txt.as_bytes(), flag).await?;
        }
        _ => return Ok(false),
    }
    if check_key() { return Ok(true); }
    drop(check_key);
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
