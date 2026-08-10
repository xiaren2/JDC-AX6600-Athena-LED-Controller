// ==========================================
// 🎮 button.rs — 物理按键监听器 (长短按分离 + 双击)
// 双后端: GPIO 字符设备直读 (优先) / debugfs 文本解析 (兜底)
// 通过 watch channel 与调度器通信: -1=息屏Toggle, +N=切台
// ==========================================

// ==========================================
// 🐧 Linux 环境下的监听器
// ==========================================
#[cfg(unix)]
pub fn spawn_button_listener(
    tx: tokio::sync::watch::Sender<i32>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    gpio_pin: String,
    gpio_base: String,
    control: crate::control::SharedControl,
) {
    use crate::led_screen;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    tokio::task::spawn_blocking(move || {
        let pin_num: u32 = gpio_pin.trim().parse().unwrap_or(71);

        // ==========================================
        // 后端 1: GPIO 字符设备直读 (现代内核标准接口)
        // 如果按键被内核 gpio-keys 驱动占用会返回 EBUSY，自动落入后端 2
        // ==========================================
        let cdev_req = led_screen::find_main_chip().and_then(|chip| {
            gpiocdev::Request::builder()
                .on_chip(chip)
                .with_consumer("athena-led-btn")
                .with_line(pin_num)
                .as_input()
                .request()
                .ok()
        });

        // ==========================================
        // 后端 2: debugfs 文本解析 (兜底，兼容老内核与被 gpio-keys 占用的引脚)
        // ==========================================
        let mut debugfs_file: Option<File> = None;
        let mut pin_patterns: Vec<String> = Vec::new();

        if cdev_req.is_some() {
            println!("🎮 [系统] 按键监听启动 (字符设备后端, 引脚 {})", pin_num);
        } else {
            match File::open("/sys/kernel/debug/gpio") {
                Ok(f) => {
                    let base: u64 = match gpio_base.trim() {
                        "" | "auto" => led_screen::detect_gpio_base(),
                        s => s.parse().unwrap_or_else(|_| led_screen::detect_gpio_base()),
                    };
                    let global_num = base + pin_num as u64;
                    pin_patterns = vec![
                        format!("gpio{}  :", pin_num),
                        format!("gpio-{} ", pin_num),
                        format!("gpio-{} ", global_num),
                        format!("gpio-{}(", global_num),
                    ];
                    debugfs_file = Some(f);
                    println!("🎮 [系统] 按键监听启动 (debugfs 后端, 引脚 {} / 全局 {})", pin_num, global_num);
                }
                Err(e) => {
                    println!("⚠️ [警告] 按键监听不可用: 字符设备请求失败，debugfs 也无法打开 ({})", e);
                    return;
                }
            }
        }

        // 判断单行是否报告“输入 + 低电平” (按键按下通常拉低)
        let line_is_pressed = |line: &str| -> bool {
            if !line.contains(" in ") { return false; }
            let trimmed = line.trim_end();
            line.contains(" low") || line.contains(" lo ") || trimmed.ends_with(" lo")
        };

        let mut buffer = String::with_capacity(4096);

        // 状态机变量
        let mut press_start: Option<Instant> = None;
        let mut long_press_handled = false;
        // 双击检测: 第一次短按松开后等待 350ms
        let mut pending_click_deadline: Option<Instant> = None;

        while running.load(Ordering::SeqCst) {
            // --- 读取当前按键电平 (按下 = 物理低电平) ---
            let is_pressed = if let Some(req) = &cdev_req {
                matches!(req.value(pin_num), Ok(gpiocdev::line::Value::Inactive))
            } else if let Some(file) = debugfs_file.as_mut() {
                buffer.clear();
                let _ = file.seek(SeekFrom::Start(0));
                if file.read_to_string(&mut buffer).is_ok() {
                    buffer.lines().any(|line| {
                        pin_patterns.iter().any(|p| line.contains(p.as_str())) && line_is_pressed(line)
                    })
                } else {
                    false
                }
            } else {
                false
            };

            if is_pressed {
                // 1️⃣ 刚刚按下瞬间，记录时间点
                if press_start.is_none() {
                    press_start = Some(Instant::now());
                    long_press_handled = false;

                    // [双击] 在等待窗口内再次按下 = 双击，立即回频道 1
                    if pending_click_deadline.is_some() {
                        pending_click_deadline = None;
                        println!("⏮️ [硬件交互] 双击触发！回到频道 1");
                        if let Ok(mut st) = control.lock() {
                            st.go_home = true;
                        }
                        let current = *tx.borrow();
                        let _ = tx.send(if current < 0 { 1 } else { current + 1 });
                        long_press_handled = true;
                    }
                }
                // 2️⃣ 一直按着没松手，检查是否达到长按阈值 (2 秒)
                else if !long_press_handled {
                    if press_start.unwrap().elapsed() >= Duration::from_secs(2) {
                        println!("🌙 [硬件交互] 检测到长按 2 秒！发送息屏/亮屏切换指令！");
                        let _ = tx.send(-1);
                        long_press_handled = true;
                    }
                }
            } else {
                // 3️⃣ 松开按键
                if let Some(start) = press_start {
                    let hold_time = start.elapsed();

                    // 如果没有触发过长按/双击，并且按下的时间大于 50ms (防物理抖动)
                    if !long_press_handled && hold_time > Duration::from_millis(50) {
                        let current = *tx.borrow();
                        if current < 0 {
                            // 休眠状态: 任何短按立即唤醒
                            println!("☀️ [硬件交互] 夜间休眠被打断，唤醒屏幕！");
                            let _ = tx.send(1);
                            pending_click_deadline = None;
                        } else {
                            // 挂起 350ms，看是否有第二击
                            pending_click_deadline = Some(Instant::now() + Duration::from_millis(350));
                        }
                    }

                    press_start = None;
                }

                // [双击] 等待窗口超时且无第二击 -> 判定为单击，正常切台
                if let Some(deadline) = pending_click_deadline {
                    if Instant::now() >= deadline {
                        pending_click_deadline = None;
                        println!("➡️ [硬件交互] 短按触发！准备切换频道...");
                        let current = *tx.borrow();
                        let _ = tx.send(if current < 0 { 1 } else { current + 1 });
                    }
                }
            }

            // 保持 100ms 的轮询频率，兼顾灵敏度与低 CPU 占用
            std::thread::sleep(Duration::from_millis(100));
        }

        println!("👋 [系统] 按钮监听线程已安全退出。");
    });
}

// ==========================================
// 🪟 Windows 环境下的“空壳”监听器 (防报错)
// ==========================================
#[cfg(not(unix))]
pub fn spawn_button_listener(
    _tx: tokio::sync::watch::Sender<i32>,
    _running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _gpio_pin: String,
    _gpio_base: String,
    _control: crate::control::SharedControl,
) {
    println!("📺 [Windows 模拟器] 按键监听已就绪（空跑模式）");
}

// ==========================================
// 🐧 Linux: Mesh 键监听器 (GPIO 72, 可自定义短按/长按动作)
// ==========================================
#[cfg(unix)]
pub fn spawn_mesh_button_listener(
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    gpio_pin: String,
    gpio_base: String,
    short_action: String,
    long_action: String,
) {
    use crate::led_screen;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    tokio::task::spawn_blocking(move || {
        let pin_num: u32 = gpio_pin.trim().parse().unwrap_or(72);

        // GPIO 字符设备直读
        let cdev_req = led_screen::find_main_chip().and_then(|chip| {
            gpiocdev::Request::builder()
                .on_chip(chip)
                .with_consumer("athena-led-mesh")
                .with_line(pin_num)
                .as_input()
                .request()
                .ok()
        });

        // debugfs 兜底
        let mut debugfs_file: Option<File> = None;
        let mut pin_patterns: Vec<String> = Vec::new();

        if cdev_req.is_some() {
            println!("📶 [Mesh键] 监听启动 (字符设备后端, 引脚 {})", pin_num);
        } else {
            match File::open("/sys/kernel/debug/gpio") {
                Ok(f) => {
                    let base: u64 = match gpio_base.trim() {
                        "" | "auto" => led_screen::detect_gpio_base(),
                        s => s.parse().unwrap_or_else(|_| led_screen::detect_gpio_base()),
                    };
                    let global_num = base + pin_num as u64;
                    pin_patterns = vec![
                        format!("gpio{}  :", pin_num),
                        format!("gpio-{} ", pin_num),
                        format!("gpio-{} ", global_num),
                        format!("gpio-{}(", global_num),
                    ];
                    debugfs_file = Some(f);
                    println!("📶 [Mesh键] 监听启动 (debugfs 后端, 引脚 {} / 全局 {})", pin_num, global_num);
                }
                Err(e) => {
                    println!("⚠️ [Mesh键] 监听不可用: 字符设备请求失败，debugfs 也无法打开 ({})", e);
                    return;
                }
            }
        }

        let line_is_pressed = |line: &str| -> bool {
            if !line.contains(" in ") { return false; }
            let trimmed = line.trim_end();
            line.contains(" low") || line.contains(" lo ") || trimmed.ends_with(" lo")
        };

        let mut buffer = String::with_capacity(4096);

        // 状态机
        let mut press_start: Option<Instant> = None;
        let mut long_press_handled = false;
        let mut pending_click_deadline: Option<Instant> = None;
        let mut double_click_handled = false;

        while running.load(Ordering::SeqCst) {
            let is_pressed = if let Some(req) = &cdev_req {
                matches!(req.value(pin_num), Ok(gpiocdev::line::Value::Inactive))
            } else if let Some(file) = debugfs_file.as_mut() {
                buffer.clear();
                let _ = file.seek(SeekFrom::Start(0));
                if file.read_to_string(&mut buffer).is_ok() {
                    buffer.lines().any(|line| {
                        pin_patterns.iter().any(|p| line.contains(p.as_str())) && line_is_pressed(line)
                    })
                } else {
                    false
                }
            } else {
                false
            };

            if is_pressed {
                if press_start.is_none() {
                    press_start = Some(Instant::now());
                    long_press_handled = false;

                    // 双击检测: 在等待窗口内再次按下
                    if pending_click_deadline.is_some() {
                        pending_click_deadline = None;
                        double_click_handled = true;
                        println!("📶 [Mesh键] 双击触发 (不执行动作)");
                    }
                }
                else if !long_press_handled {
                    if press_start.unwrap().elapsed() >= Duration::from_secs(2) {
                        println!("📶 [Mesh键] 长按触发！执行: {}", long_action);
                        execute_action(&long_action);
                        long_press_handled = true;
                    }
                }
            } else {
                if let Some(start) = press_start {
                    let hold_time = start.elapsed();

                    if !long_press_handled && !double_click_handled && hold_time > Duration::from_millis(50) {
                        pending_click_deadline = Some(Instant::now() + Duration::from_millis(350));
                    }

                    press_start = None;
                }

                // 等待窗口超时且无第二击 -> 单击
                if let Some(deadline) = pending_click_deadline {
                    if Instant::now() >= deadline {
                        pending_click_deadline = None;
                        if !double_click_handled {
                            println!("📶 [Mesh键] 短按触发！执行: {}", short_action);
                            execute_action(&short_action);
                        }
                        double_click_handled = false;
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        println!("📶 [Mesh键] 监听线程已退出。");
    });
}

/// 执行按键动作
#[cfg(unix)]
fn execute_action(action: &str) {
    use std::process::Command;
    match action {
        "reboot" => {
            println!("🔄 [Mesh键] 重启路由器...");
            let _ = Command::new("reboot").spawn();
        }
        "restart_network" => {
            println!("🌐 [Mesh键] 重启网络...");
            let _ = Command::new("/etc/init.d/network").arg("restart").spawn();
        }
        "restart_wifi" => {
            println!("📡 [Mesh键] 重启 Wi-Fi...");
            let _ = Command::new("/etc/init.d/wireless").arg("restart").spawn();
        }
        "restart_athena" => {
            println!("💡 [Mesh键] 重启 Athena LED...");
            let _ = Command::new("/etc/init.d/athena_led").arg("restart").spawn();
        }
        _ => {} // none 或未知动作
    }
}

// ==========================================
// 🪟 Windows: Mesh 键空壳
// ==========================================
#[cfg(not(unix))]
pub fn spawn_mesh_button_listener(
    _running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _gpio_pin: String,
    _gpio_base: String,
    _short_action: String,
    _long_action: String,
) {
    println!("📶 [Windows 模拟器] Mesh 键监听已就绪（空跑模式）");
}
