use anyhow::Result;
use crate::char_dict::CHAR_DICT;
use tokio::sync::watch;
use std::sync::{Arc, Mutex};

const LOW: u8 = 0x00;
const HIGH: u8 = 0x01;

// Display mode commands
const COMMAND1: u8 = 0b00000011; // Display mode
const COMMAND2: u8 = 0b01000000; // Data mode
const COMMAND3: u8 = 0b11000000; // Display address

// ===================== GPIO 物理引脚偏移（AX6600 Athena）=====================
// 参考设备树 pinctrl:  主控芯片 GPIO 相对编号（物理偏移）
// 不要硬编码 581/582/585/586 这种全局编号! 基址按固件内核版本变化:
//   - 内核 6.1 (iStoreOS 新): gpiochip base = 512 → 69+512=581
//   - 内核 5.4 (QWRT 等)   : gpiochip base = 432 → 69+432=501
//   - 内核 5.10 特殊版     : gpiochip base = 0   → 69+0=69
pub const PIN_STB_LEFT:  u32 = 69;
pub const PIN_STB_RIGHT: u32 = 70;
pub const PIN_CLK:       u32 = 73;
pub const PIN_DIO:       u32 = 74;

// ===================== GPIO 后端枚举 =====================
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpioBackend { Auto, Cdev, Sysfs }

impl std::str::FromStr for GpioBackend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto"  => Ok(GpioBackend::Auto),
            "cdev"  => Ok(GpioBackend::Cdev),
            "sysfs" => Ok(GpioBackend::Sysfs),
            other   => Err(format!("unknown gpio backend: {}", other)),
        }
    }
}

// ===================== 抽象 GPIO 总线 trait =====================
trait GpioBus: Send {
    fn write(&mut self, pin_offset: u32, value: u8) -> Result<()>;
    fn shutdown(&mut self) {}
}

// ===================== 字符设备后端 /dev/gpiochipN =====================
#[cfg(unix)]
struct CdevBus {
    req: gpiocdev::Request,
    offsets: [u32; 4], // stb_l, stb_r, clk, dio
}

#[cfg(unix)]
impl CdevBus {
    fn new(chip_path: &std::path::Path, stb_l: u32, stb_r: u32, clk: u32, dio: u32) -> Result<Self> {
        use gpiocdev::line::{Direction, Value};
        let offsets = [stb_l, stb_r, clk, dio];
        let req = gpiocdev::Builder::new()
            .on_chip(chip_path)
            .with_lines(&offsets)
            .with_consumer("athena-led")
            .as_output(Value::Inactive)
            .direction(Direction::Output)
            .request()?;
        Ok(Self { req, offsets })
    }

    fn idx(&self, pin_offset: u32) -> Result<usize> {
        self.offsets.iter().position(|&p| p == pin_offset)
            .ok_or_else(|| anyhow::anyhow!("pin offset {} not in cdev request", pin_offset))
    }
}

#[cfg(unix)]
impl GpioBus for CdevBus {
    fn write(&mut self, pin_offset: u32, value: u8) -> Result<()> {
        use gpiocdev::line::Value;
        let idx = self.idx(pin_offset)?;
        let v = if value == LOW { Value::Inactive } else { Value::Active };
        self.req.set_line(self.offsets[idx], v)?;
        Ok(())
    }
}

// ===================== sysfs 后端（/sys/class/gpio/gpioN）=====================
#[cfg(unix)]
struct SysfsBus {
    base: u64,
    pins: std::collections::HashMap<u32, sysfs_gpio::Pin>,
}

#[cfg(unix)]
impl SysfsBus {
    fn new(base: u64, stb_l: u32, stb_r: u32, clk: u32, dio: u32) -> Result<Self> {
        use sysfs_gpio::{Direction, Pin};
        let mut pins = std::collections::HashMap::new();
        for off in [stb_l, stb_r, clk, dio] {
            let global = base + off as u64;
            let p = Pin::new(global);
            let _ = p.export();
            std::thread::sleep(std::time::Duration::from_millis(5)); // udev 规则同步
            p.set_direction(Direction::Out).map_err(|e| {
                anyhow::anyhow!("Failed to set gpio{} (base={}+off={}) direction: {}", global, base, off, e)
            })?;
            pins.insert(off, p);
        }
        Ok(Self { base, pins })
    }
}

#[cfg(unix)]
impl GpioBus for SysfsBus {
    fn write(&mut self, pin_offset: u32, value: u8) -> Result<()> {
        let p = self.pins.get_mut(&pin_offset)
            .ok_or_else(|| anyhow::anyhow!("sysfs pin off {} not initialized (base={})", pin_offset, self.base))?;
        p.set_value(value)?;
        Ok(())
    }
    fn shutdown(&mut self) {
        for (_, pin) in &self.pins { let _ = pin.unexport(); }
    }
}

// ===================== LED Screen 结构 =====================
pub struct LedScreen {
    left_screen: LedScreenUnit,
    right_screen: LedScreenUnit,
    disabled_led_mask: u8,
    interrupt_rx: Option<Arc<Mutex<watch::Receiver<i32>>>>,
    interrupt_last_seen: i32,
}

pub struct LedScreenUnit {
    bus: Arc<Mutex<Box<dyn GpioBus>>>,
    stb_off: u32,
    clk_off: u32,
    dio_off: u32,
}

// ===================== GPIO 后端选择工厂 =====================
#[cfg(unix)]
fn build_bus(backend: GpioBackend, base: u64) -> Result<Arc<Mutex<Box<dyn GpioBus>>>> {
    let try_cdev = || -> Result<Box<dyn GpioBus>> {
        let chip = find_main_chip().ok_or_else(|| anyhow::anyhow!("No pinctrl/tlmm gpiochip character device found"))?;
        Ok(Box::new(CdevBus::new(&chip, PIN_STB_LEFT, PIN_STB_RIGHT, PIN_CLK, PIN_DIO)?))
    };
    let try_sysfs = || -> Result<Box<dyn GpioBus>> {
        Ok(Box::new(SysfsBus::new(base, PIN_STB_LEFT, PIN_STB_RIGHT, PIN_CLK, PIN_DIO)?))
    };

    let bus: Box<dyn GpioBus> = match backend {
        GpioBackend::Cdev  => try_cdev()?,
        GpioBackend::Sysfs => try_sysfs()?,
        GpioBackend::Auto  => match try_cdev() {
            Ok(b) => { println!("🔌 [GPIO] 使用字符设备后端 /dev/gpiochipN (优先)"); b }
            Err(e) => {
                println!("⚠️ [GPIO] 字符设备后端失败 ({}), 回退到 sysfs 后端 (base={})", e, base);
                try_sysfs()?
            }
        },
    };
    Ok(Arc::new(Mutex::new(bus)))
}

#[cfg(not(unix))]
fn build_bus(_backend: GpioBackend, _base: u64) -> Result<Arc<Mutex<Box<dyn GpioBus>>>> {
    // Windows/Mac 下无真实 GPIO，由 led_screen_sim 接管；此处仅占位
    // (实际 led_screen_sim.rs 会完全重写 LedScreen，此处不会被调用)
    unimplemented!("Real GPIO unavailable on non-unix platform; use led_screen_sim.rs")
}

impl LedScreen {
    /// 使用 GPIO 后端 + gpio_base 构造屏幕 (不硬编码全局引脚编号)
    pub fn new(backend: GpioBackend, gpio_base: u64, disabled_led_mask: u8) -> Result<Self> {
        let bus = build_bus(backend, gpio_base)?;
        let left_screen  = LedScreenUnit::new(bus.clone(), PIN_STB_LEFT,  PIN_CLK, PIN_DIO);
        let right_screen = LedScreenUnit::new(bus,         PIN_STB_RIGHT, PIN_CLK, PIN_DIO);

        let mut screen = Self {
            left_screen, right_screen, disabled_led_mask,
            interrupt_rx: None, interrupt_last_seen: 0,
        };
        screen.set_show_model()?;
        screen.set_data_model()?;
        Ok(screen)
    }

    // 兼容旧接口 (backward compat)
    pub fn new_with_mask(_stb_l: u64, _stb_r: u64, _clk: u64, _dio: u64, mask: u8) -> Result<Self> {
        // 旧调用方: 用 Auto + 自动探测 base
        let base = detect_gpio_base();
        Self::new(GpioBackend::Auto, base, mask)
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

    pub async fn write_data(&mut self, text: &[u8], status: u8) -> Result<()> {
        let mut display_data = Vec::new();
        let content = std::str::from_utf8(text).unwrap_or("");

        for ch in content.chars() {
            let key = ch.to_ascii_uppercase();
            if let Some(bytes) = CHAR_DICT.get(&key) {
                display_data.extend_from_slice(bytes);
                display_data.push(0x00); // 1 列字间距
            }
        }
        // ★ 修复: 去掉最后一个多余的字间距空格, 刚好 27 列时不触发滚动
        if display_data.last() == Some(&0x00) {
            display_data.pop();
        }

        if display_data.len() > 27 {
            self.flow(&display_data, status).await?;
        } else {
            self.static_display(&display_data, status)?;
        }
        Ok(())
    }

    async fn flow(&mut self, data: &[u8], status: u8) -> Result<()> {
        if let Some(rx) = &self.interrupt_rx {
            if let Ok(guard) = rx.lock() {
                self.interrupt_last_seen = *(*guard).borrow();
            }
        }
        let mut start = 0;
        for i in 1..=data.len() {
            let mut off = [0u8; 27];
            if i > 27 { start += 1; }
            off[..i.min(27)].copy_from_slice(&data[start..start + i.min(27)]);
            self.do_write_data(&off, status)?;

            if self.poll_interrupt().is_some() { return Ok(()); }
            // ★ 修复: 用 tokio::time::sleep, 不再阻塞整个 tokio 线程池
            // 把 128ms 切成 20ms 小段, 提高按键响应
            let mut slept = 0u64;
            while slept < 128 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                slept += 20;
                if self.poll_interrupt().is_some() { return Ok(()); }
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
    fn new(bus: Arc<Mutex<Box<dyn GpioBus>>>, stb_off: u32, clk_off: u32, dio_off: u32) -> Self {
        Self { bus, stb_off, clk_off, dio_off }
    }

    fn set_show_model(&mut self) -> Result<()> { self.do_write_data(COMMAND1, &[]) }
    fn set_data_model(&mut self) -> Result<()> { self.do_write_data(COMMAND2, &[]) }

    fn power(&mut self, run: bool, light_level: u8) -> Result<()> {
        let command = if run {
            (light_level << 5 >> 5 | 0b11111000) & 0b10001111
        } else {
            0b10000000
        };
        self.do_write_data(command, &[])
    }

    fn printf(&mut self, values: &[u8]) -> Result<()> { self.do_write_data(COMMAND3, values) }

    fn do_write_data(&mut self, command: u8, values: &[u8]) -> Result<()> {
        self.write_pin(self.stb_off, LOW)?;
        self.write_command_byte(command)?;
        for (i, &value) in values.iter().enumerate() {
            self.write_data_byte(value, i % 2 != 0)?;
        }
        self.write_pin(self.stb_off, HIGH)?;
        Ok(())
    }

    fn write_pin(&mut self, off: u32, val: u8) -> Result<()> {
        let mut guard = self.bus.lock().map_err(|e| anyhow::anyhow!("GpioBus lock poisoned: {}", e))?;
        guard.write(off, val)
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
            for _ in 0..6 { self.write_bit(LOW)?; }
        }
        Ok(())
    }

    fn write_bit(&mut self, bit: u8) -> Result<()> {
        self.write_pin(self.clk_off, LOW)?;
        self.write_pin(self.dio_off, bit)?;
        self.write_pin(self.clk_off, HIGH)?;
        Ok(())
    }
}

impl Drop for LedScreen {
    fn drop(&mut self) {
        // bus Arc 最后一个持有者释放时，会调用 shutdown
    }
}

// ==========================================
// 辅助函数：自动探测主控芯片 GPIO base（仅 sysfs 后端用）
// ==========================================
#[cfg(unix)]
pub fn detect_gpio_base() -> u64 {
    use std::fs;
    let mut best: Option<(u64, u64, bool)> = None;
    if let Ok(entries) = fs::read_dir("/sys/class/gpio") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("gpiochip") { continue; }
            let path = entry.path();
            let read_num = |f: &str| -> Option<u64> {
                fs::read_to_string(path.join(f)).ok().and_then(|s| s.trim().parse().ok())
            };
            let base = match read_num("base") { Some(b) => b, None => continue };
            let ngpio = read_num("ngpio").unwrap_or(0);
            let label = fs::read_to_string(path.join("label")).unwrap_or_default().trim().to_lowercase();
            let is_main = label.contains("pinctrl") || label.contains("tlmm");
            let better = match &best {
                None => true,
                Some((_, best_n, best_main)) =>
                    (is_main && !best_main) || (is_main == *best_main && ngpio > *best_n),
            };
            if better { best = Some((base, ngpio, is_main)); }
        }
    }
    match best {
        Some((base, ngpio, _)) => {
            println!("🔍 [GPIO] 自动探测主控芯片 base={} (ngpio={})", base, ngpio);
            base
        }
        None => { println!("⚠️ [GPIO] 未能探测 gpiochip，回退默认 base=512"); 512 }
    }
}

#[cfg(not(unix))]
pub fn detect_gpio_base() -> u64 { 512 }

// ==========================================
// 辅助函数：查找主控芯片的字符设备路径 /dev/gpiochipN
// ==========================================
#[cfg(unix)]
pub fn find_main_chip() -> Option<std::path::PathBuf> {
    use std::fs;
    use std::path::PathBuf;
    let mut best: Option<(PathBuf, u32, bool)> = None;
    if let Ok(entries) = fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("gpiochip") { continue; }
            let path = entry.path();
            let info = match gpiocdev::Chip::from_path(&path).and_then(|c| c.info()) {
                Ok(i) => i, Err(_) => continue,
            };
            let label = info.label.to_lowercase();
            let is_main = label.contains("pinctrl") || label.contains("tlmm");
            let better = match &best {
                None => true,
                Some((_, best_n, best_main)) =>
                    (is_main && !best_main) || (is_main == *best_main && info.num_lines > *best_n),
            };
            if better { best = Some((path, info.num_lines, is_main)); }
        }
    }
    best.map(|(p, n, _)| {
        println!("🔍 [GPIO] 找到主控字符设备: {} (lines={})", p.display(), n);
        p
    })
}

#[cfg(not(unix))]
pub fn find_main_chip() -> Option<std::path::PathBuf> { None }
