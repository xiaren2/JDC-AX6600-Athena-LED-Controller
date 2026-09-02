use anyhow::Result;
use sysfs_gpio::{Direction, Pin};
use crate::char_dict::CHAR_DICT;
use tokio::sync::watch;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;

const LOW: u8 = 0x00;
const HIGH: u8 = 0x01;

const COMMAND1: u8 = 0b00000011;
const COMMAND2: u8 = 0b01000000;
const COMMAND3: u8 = 0b11000000;

// ==========================================
// GPIO 后端抽象：cdev 字符设备（优先）/ sysfs_gpio（回退）
// ==========================================
pub enum GpioBackend {
    Cdev(PathBuf),  // /dev/gpiochipN 路径
    Sysfs(u64),     // TLMM base
}

pub enum GpioLine {
    #[cfg(unix)]
    Cdev(gpiocdev::Request, u32),
    Sysfs(Pin),
}

#[cfg(unix)]
impl GpioLine {
    pub fn set_value(&mut self, val: u8) -> Result<()> {
        match self {
            GpioLine::Sysfs(pin) => pin.set_value(val)?,
            GpioLine::Cdev(req, offset) => {
                let v = if val != 0 {
                    gpiocdev::line::Value::Active
                } else {
                    gpiocdev::line::Value::Inactive
                };
                req.set_value(*offset, v)?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for GpioLine {
    fn drop(&mut self) {
        if let GpioLine::Sysfs(pin) = self {
            let _ = pin.unexport();
        }
    }
}

// ==========================================
// LedScreen 主体
// ==========================================
pub struct LedScreen {
    left_screen: LedScreenUnit,
    right_screen: LedScreenUnit,
    disabled_led_mask: u8,
    interrupt_rx: Option<Arc<Mutex<watch::Receiver<i32>>>>,
    interrupt_last_seen: i32,
}

pub struct LedScreenUnit {
    stb: GpioLine,
    clk: GpioLine,
    dio: GpioLine,
}

impl LedScreen {
    pub fn new(stb_left: u64, stb_right: u64, clk: u64, dio: u64) -> Result<Self> {
        Self::new_with_mask(stb_left, stb_right, clk, dio, 0)
    }

    pub fn new_with_mask(
        stb_left: u64, stb_right: u64, clk: u64, dio: u64,
        disabled_led_mask: u8,
    ) -> Result<Self> {
        let left_screen = LedScreenUnit::new(stb_left, clk, dio)?;
        let right_screen = LedScreenUnit::new(stb_right, clk, dio)?;

        let mut screen = Self {
            left_screen,
            right_screen,
            disabled_led_mask,
            interrupt_rx: None,
            interrupt_last_seen: 0,
        };

        screen.set_show_model()?;
        screen.set_data_model()?;

        Ok(screen)
    }

    pub fn bind_interrupt_rx(&mut self, rx: Arc<Mutex<watch::Receiver<i32>>>) {
        self.interrupt_last_seen = *rx.lock().unwrap().borrow();
        self.interrupt_rx = Some(rx);
    }

    pub fn poll_interrupt(&mut self) -> Option<i32> {
        let rx = self.interrupt_rx.as_ref()?;
        let guard = rx.lock().ok()?;
        let current = *(*guard).borrow();
        if current != self.interrupt_last_seen {
            self.interrupt_last_seen = current;
            if current == 0 { return None; }
            return Some(current);
        }
        None
    }

    pub fn set_show_model(&mut self) -> Result<()> {
        self.left_screen.set_show_model()?;
        self.right_screen.set_show_model()?;
        Ok(())
    }

    pub fn set_data_model(&mut self) -> Result<()> {
        self.left_screen.set_data_model()?;
        self.right_screen.set_data_model()?;
        Ok(())
    }

    pub fn power(&mut self, run: bool, light_level: u8) -> Result<()> {
        self.left_screen.power(run, light_level)?;
        self.right_screen.power(run, light_level)?;
        Ok(())
    }

    pub fn write_data(&mut self, text: &[u8], status: u8) -> Result<()> {
        let mut display_data = Vec::new();

        let content = std::str::from_utf8(text).unwrap_or("");

        for ch in content.chars() {
            let key = ch.to_ascii_uppercase();

            if let Some(bytes) = CHAR_DICT.get(&key) {
                display_data.extend_from_slice(bytes);
                display_data.push(0x00);
            }
        }

        if !display_data.is_empty() {
            display_data.pop();
        }

        if display_data.len() > 27 {
            self.flow(&display_data, status)?;
        } else {
            self.static_display(&display_data, status)?;
        }
        Ok(())
    }

    fn flow(&mut self, data: &[u8], status: u8) -> Result<()> {
        if let Some(rx) = &self.interrupt_rx {
            if let Ok(guard) = rx.lock() {
                self.interrupt_last_seen = *(*guard).borrow();
            }
        }
        let mut start = 0;
        for i in 1..=data.len() {
            let mut off = [0u8; 27];
            if i > 27 {
                start += 1;
            }
            off[..i.min(27)].copy_from_slice(&data[start..start + i.min(27)]);
            self.do_write_data(&off, status)?;

            if self.poll_interrupt().is_some() {
                return Ok(());
            }
            let mut slept = 0u64;
            while slept < 128 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                slept += 20;
                if self.poll_interrupt().is_some() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn static_display(&mut self, data: &[u8], status: u8) -> Result<()> {
        let mut display_data = [0u8; 27];
        if data.len() < 27 {
            let offset = (27 - data.len()) / 2;
            display_data[offset..offset + data.len()].copy_from_slice(data);
        } else {
            display_data[..27].copy_from_slice(&data[..27]);
        }
        self.do_write_data(&display_data, status)?;
        Ok(())
    }

    fn do_write_data(&mut self, values: &[u8], status: u8) -> Result<()> {
        self.left_screen.printf(&values[..14])?;
        let mut right_data = values[14..27].to_vec();
        let filtered_status = status & !self.disabled_led_mask;
        right_data.push(filtered_status);
        self.right_screen.printf(&right_data)?;
        Ok(())
    }
}

impl LedScreenUnit {
    #[cfg(unix)]
    fn new(stb_offset: u64, clk_offset: u64, dio_offset: u64) -> Result<Self> {
        let backend = detect_gpio_backend();
        let stb = create_line(stb_offset, &backend)?;
        let clk = create_line(clk_offset, &backend)?;
        let dio = create_line(dio_offset, &backend)?;

        Ok(Self { stb, clk, dio })
    }

    #[cfg(not(unix))]
    fn new(stb_offset: u64, clk_offset: u64, dio_offset: u64) -> Result<Self> {
        let _ = (stb_offset, clk_offset, dio_offset);
        println!("📺 [Windows 模拟器] GPIO 后端跳过（空跑）");
        Ok(Self {
            stb: GpioLine::Sysfs(Pin::new(0)),
            clk: GpioLine::Sysfs(Pin::new(0)),
            dio: GpioLine::Sysfs(Pin::new(0)),
        })
    }

    fn set_show_model(&mut self) -> Result<()> {
        self.do_write_data(COMMAND1, &[])?;
        Ok(())
    }

    fn set_data_model(&mut self) -> Result<()> {
        self.do_write_data(COMMAND2, &[])?;
        Ok(())
    }

    fn power(&mut self, run: bool, light_level: u8) -> Result<()> {
        let command = if run {
            (light_level << 5 >> 5 | 0b11111000) & 0b10001111
        } else {
            0b10000000
        };
        self.do_write_data(command, &[])?;
        Ok(())
    }

    fn printf(&mut self, values: &[u8]) -> Result<()> {
        self.do_write_data(COMMAND3, values)?;
        Ok(())
    }

    fn do_write_data(&mut self, command: u8, values: &[u8]) -> Result<()> {
        self.stb.set_value(LOW)?;
        self.write_command_byte(command)?;

        for (i, &value) in values.iter().enumerate() {
            self.write_data_byte(value, i % 2 != 0)?;
        }

        self.stb.set_value(HIGH)?;
        Ok(())
    }

    fn write_command_byte(&mut self, value: u8) -> Result<()> {
        for i in 0..8 {
            let bit = (value >> i) & 0x01;
            self.write_bit(bit)?;
        }
        Ok(())
    }

    fn write_data_byte(&mut self, value: u8, fill_data: bool) -> Result<()> {
        for i in 0..5 {
            let bit = (value >> i) & 0x01;
            self.write_bit(bit)?;
        }

        if fill_data {
            for _ in 0..6 {
                self.write_bit(LOW)?;
            }
        }
        Ok(())
    }

    fn write_bit(&mut self, bit: u8) -> Result<()> {
        self.clk.set_value(LOW)?;
        self.dio.set_value(bit)?;
        self.clk.set_value(HIGH)?;
        Ok(())
    }
}

// ==========================================
// GPIO base 自动探测 (sysfs 路径)
// ==========================================
#[cfg(unix)]
pub fn detect_gpio_base() -> u64 {
    use std::fs;

    let mut best: Option<(u64, u64, bool)> = None;

    if let Ok(entries) = fs::read_dir("/sys/class/gpio") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("gpiochip") {
                continue;
            }
            let path = entry.path();
            let read_num = |file: &str| -> Option<u64> {
                fs::read_to_string(path.join(file))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
            };

            let base = match read_num("base") {
                Some(b) => b,
                None => continue,
            };
            let nggpio = read_num("ngpio").unwrap_or(read_num("nggpio").unwrap_or(0));
            let label = fs::read_to_string(path.join("label"))
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            let is_main = label.contains("pinctrl") || label.contains("tlmm");

            let better = match &best {
                None => true,
                Some((_, best_n, best_main)) => {
                    (is_main && !best_main) || (is_main == *best_main && nggpio > *best_n)
                }
            };
            if better {
                best = Some((base, nggpio, is_main));
            }
        }
    }

    match best {
        Some((base, nggpio, _)) => {
            println!("🔍 [GPIO] 自动探测到主控芯片 base={} (ngpio={})", base, nggpio);
            base
        }
        None => {
            println!("⚠️ [GPIO] 未能探测到 gpiochip，回退默认 base=512");
            512
        }
    }
}

#[cfg(not(unix))]
pub fn detect_gpio_base() -> u64 {
    512
}

// ==========================================
// 查找主控芯片的字符设备路径 /dev/gpiochipN
// ==========================================
#[cfg(unix)]
pub fn find_main_chip() -> Option<std::path::PathBuf> {
    use std::fs;
    use std::path::PathBuf;

    let mut best: Option<(PathBuf, u32, bool)> = None;

    if let Ok(entries) = fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("gpiochip") {
                continue;
            }
            let path = entry.path();
            let info = match gpiocdev::Chip::from_path(&path).and_then(|c| c.info()) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let label = info.label.to_lowercase();
            let is_main = label.contains("pinctrl") || label.contains("tlmm");

            let better = match &best {
                None => true,
                Some((_, best_n, best_main)) => {
                    (is_main && !best_main) || (is_main == *best_main && info.num_lines > *best_n)
                }
            };
            if better {
                best = Some((path, info.num_lines, is_main));
            }
        }
    }

    best.map(|(p, n, _)| {
        println!("🔍 [GPIO] 找到主控字符设备: {} (lines={})", p.display(), n);
        p
    })
}

#[cfg(not(unix))]
pub fn find_main_chip() -> Option<std::path::PathBuf> {
    None
}

// ==========================================
// 探测 GPIO 后端并创建单条 GPIO 线
// ==========================================
#[cfg(unix)]
fn detect_gpio_backend() -> GpioBackend {
    if let Some(chip_path) = find_main_chip() {
        println!("✅ [GPIO] 使用 cdev 字符设备后端");
        return GpioBackend::Cdev(chip_path);
    }
    let base = detect_gpio_base();
    println!("⚠️ [GPIO] cdev 不可用，回退 sysfs_gpio (base={})", base);
    GpioBackend::Sysfs(base)
}

#[cfg(unix)]
fn create_line(offset: u64, backend: &GpioBackend) -> Result<GpioLine> {
    match backend {
        GpioBackend::Cdev(chip_path) => {
            gpiocdev::Request::builder()
                .on_chip(chip_path.clone())
                .with_consumer("athena-led")
                .with_line(offset as u32)
                .as_output(gpiocdev::line::Value::Inactive)
                .request()
                .map(|req| GpioLine::Cdev(req, offset as u32))
                .map_err(|e| anyhow::anyhow!("cdev request line {} failed: {}", offset, e))
        }
        GpioBackend::Sysfs(base) => {
            let global = base + offset;
            let pin = Pin::new(global);
            pin.export()?;
            pin.set_direction(Direction::Out)?;
            Ok(GpioLine::Sysfs(pin))
        }
    }
}

#[cfg(not(unix))]
fn detect_gpio_backend() -> GpioBackend {
    GpioBackend::Sysfs(512)
}

#[cfg(not(unix))]
fn create_line(offset: u64, _backend: &GpioBackend) -> Result<GpioLine> {
    Ok(GpioLine::Sysfs(Pin::new(offset)))
}
