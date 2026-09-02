use anyhow::{anyhow, Context, Result};
use crate::char_dict::CHAR_DICT;
use tokio::sync::watch;
use std::sync::{Arc, Mutex};

const LOW: u8 = 0x00;
const HIGH: u8 = 0x01;

// ==========================================
// 🎯 AX6600 屏幕在主控 TLMM 上的【硬件引脚偏移】(固定不变)
// sysfs 后端需要 + gpio_base 换算成全局编号；cdev 后端直接用这些偏移
// ==========================================
pub const PIN_STB_LEFT: u32 = 69;
pub const PIN_STB_RIGHT: u32 = 70;
pub const PIN_CLK: u32 = 73;
pub const PIN_DIO: u32 = 74;

// TM1628A 协议命令
const COMMAND1: u8 = 0b00000011; // Display mode
const COMMAND2: u8 = 0b01000000; // Data mode
const COMMAND3: u8 = 0b11000000; // Display address

// ==========================================
// 🔌 [双后端架构] GPIO 总线抽象
// cdev  = /dev/gpiochipN 字符设备 (现代内核标准接口，优先)
// sysfs = /sys/class/gpio (内核已废弃，仅老固件回退)
// 注意: CLK/DIO 是左右两屏共享的，所以必须由总线统一持有，
//       不能像旧版那样每个屏各导出一份 (cdev 下会 EBUSY)
// ==========================================
#[derive(Clone, Copy, PartialEq)]
enum Line {
    StbLeft,
    StbRight,
    Clk,
    Dio,
}

enum GpioBus {
    Cdev {
        req: gpiocdev::Request,
    },
    Sysfs {
        stb_l: sysfs_gpio::Pin,
        stb_r: sysfs_gpio::Pin,
        clk: sysfs_gpio::Pin,
        dio: sysfs_gpio::Pin,
    },
}

impl GpioBus {
    fn set(&mut self, line: Line, level: u8) -> Result<()> {
        match self {
            GpioBus::Cdev { req } => {
                let offset = match line {
                    Line::StbLeft => PIN_STB_LEFT,
                    Line::StbRight => PIN_STB_RIGHT,
                    Line::Clk => PIN_CLK,
                    Line::Dio => PIN_DIO,
                };
                let value = if level == LOW {
                    gpiocdev::line::Value::Inactive
                } else {
                    gpiocdev::line::Value::Active
                };
                req.set_value(offset, value)?;
            }
            GpioBus::Sysfs { stb_l, stb_r, clk, dio } => {
                let pin = match line {
                    Line::StbLeft => stb_l,
                    Line::StbRight => stb_r,
                    Line::Clk => clk,
                    Line::Dio => dio,
                };
                pin.set_value(level)?;
            }
        }
        Ok(())
    }
}

impl Drop for GpioBus {
    fn drop(&mut self) {
        // cdev 的 Request 析构时内核自动释放线；sysfs 需要手动 unexport
        if let GpioBus::Sysfs { stb_l, stb_r, clk, dio } = self {
            let _ = stb_l.unexport();
            let _ = stb_r.unexport();
            let _ = clk.unexport();
            let _ = dio.unexport();
        }
    }
}

pub struct LedScreen {
    bus: GpioBus,
    // 禁用 LED 掩码: bit0=时钟 bit1=奖牌 bit2=上箭头 bit3=下箭头
    disabled_led_mask: u8,
    // 按键 watch channel: flow() 滚动每帧检查按键中断
    interrupt_rx: Option<Arc<Mutex<watch::Receiver<i32>>>>,
    interrupt_last_seen: i32,
}

impl LedScreen {
    // ==========================================
    // 🌟 [新构造函数] 双后端 + 自动 base 探测
    // backend: "auto"(推荐) / "cdev" / "sysfs"
    // gpio_base: "auto"(推荐) / 数字，仅 sysfs 后端用
    // disabled_led_mask: 4 盏状态灯独立禁用掩码
    // ==========================================
    pub fn new(backend: &str, gpio_base: &str, disabled_led_mask: u8) -> Result<Self> {
        let bus = match backend.trim() {
            "cdev" => Self::open_cdev()?,
            "sysfs" => Self::open_sysfs(gpio_base)?,
            // auto: 优先字符设备，失败自动回退 sysfs (覆盖绝大多数固件场景)
            _ => match Self::open_cdev() {
                Ok(bus) => bus,
                Err(e) => {
                    println!("⚠️ [GPIO] 字符设备后端不可用 ({})，自动回退 sysfs 后端", e);
                    Self::open_sysfs(gpio_base)?
                }
            },
        };

        let mut screen = Self {
            bus,
            disabled_led_mask,
            interrupt_rx: None,
            interrupt_last_seen: 0,
        };
        screen.set_show_model()?;
        screen.set_data_model()?;
        Ok(screen)
    }

    // 兼容旧接口：直接传 4 个数字引脚号 + mask（忽略前 4 个硬编码参数，强制走 auto/auto）
    // 保留这个 API 是为了万一还有外部调用者
    #[allow(dead_code)]
    pub fn new_with_mask(
        _stb_left: u64, _stb_right: u64, _clk: u64, _dio: u64,
        disabled_led_mask: u8,
    ) -> Result<Self> {
        println!("ℹ️ [GPIO] 旧 API (new_with_mask) 调用已重定向到 auto/auto (双后端自动探测)");
        Self::new("auto", "auto", disabled_led_mask)
    }

    fn open_cdev() -> Result<GpioBus> {
        let chip = find_main_chip().ok_or_else(|| anyhow!("未找到 /dev/gpiochip* 字符设备"))?;
        let req = gpiocdev::Request::builder()
            .on_chip(chip.clone())
            .with_consumer("athena-led")
            .with_lines(&[PIN_STB_LEFT, PIN_STB_RIGHT, PIN_CLK, PIN_DIO])
            .as_output(gpiocdev::line::Value::Inactive)
            .request()
            .with_context(|| format!("在 {} 上请求屏幕 GPIO 线失败", chip.display()))?;
        println!("🔌 [GPIO] 屏幕使用字符设备后端: {}", chip.display());
        Ok(GpioBus::Cdev { req })
    }

    fn open_sysfs(gpio_base: &str) -> Result<GpioBus> {
        let base: u64 = match gpio_base.trim() {
            "" | "auto" => detect_gpio_base(),
            s => s.parse().unwrap_or_else(|_| {
                println!("⚠️ [GPIO] --gpio-base 参数 '{}' 无法解析，改用自动探测", s);
                detect_gpio_base()
            }),
        };

        let make_pin = |offset: u32, name: &str| -> Result<sysfs_gpio::Pin> {
            let num = base + offset as u64;
            let pin = sysfs_gpio::Pin::new(num);
            pin.export()
                .with_context(|| format!("导出 GPIO{} ({}) 失败，请检查 gpio_base 是否正确", num, name))?;
            pin.set_direction(sysfs_gpio::Direction::Out)
                .with_context(|| format!("设置 GPIO{} 方向失败", num))?;
            Ok(pin)
        };

        let stb_l = make_pin(PIN_STB_LEFT, "STB_L")?;
        let stb_r = make_pin(PIN_STB_RIGHT, "STB_R")?;
        let clk = make_pin(PIN_CLK, "CLK")?;
        let dio = make_pin(PIN_DIO, "DIO")?;

        println!(
            "🔌 [GPIO] 屏幕使用 sysfs 后端 (base={}, 引脚 {}/{}/{}/{})",
            base,
            base + PIN_STB_LEFT as u64,
            base + PIN_STB_RIGHT as u64,
            base + PIN_CLK as u64,
            base + PIN_DIO as u64
        );
        Ok(GpioBus::Sysfs { stb_l, stb_r, clk, dio })
    }

    /// 绑定按键 watch channel，后续 flow/滚动 会在每帧检查按键中断
    pub fn bind_interrupt_rx(&mut self, rx: Arc<Mutex<watch::Receiver<i32>>>) {
        self.interrupt_last_seen = *rx.lock().unwrap().borrow();
        self.interrupt_rx = Some(rx);
    }

    /// 检查是否有待处理按键/息屏指令. None=无, Some(cmd)=有
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
        self.unit_write(Line::StbLeft, COMMAND1, &[])?;
        self.unit_write(Line::StbRight, COMMAND1, &[])?;
        Ok(())
    }

    pub fn set_data_model(&mut self) -> Result<()> {
        self.unit_write(Line::StbLeft, COMMAND2, &[])?;
        self.unit_write(Line::StbRight, COMMAND2, &[])?;
        Ok(())
    }

    pub fn power(&mut self, run: bool, light_level: u8) -> Result<()> {
        let command = if run {
            (light_level << 5 >> 5 | 0b11111000) & 0b10001111
        } else {
            0b10000000
        };
        self.unit_write(Line::StbLeft, command, &[])?;
        self.unit_write(Line::StbRight, command, &[])?;
        Ok(())
    }

    pub fn write_data(&mut self, text: &[u8], status: u8) -> Result<()> {
        let mut display_data = Vec::new();

        let content = std::str::from_utf8(text).unwrap_or("");

        for ch in content.chars() {
            let key = ch.to_ascii_uppercase();
            if let Some(bytes) = CHAR_DICT.get(&key) {
                display_data.extend_from_slice(bytes);
                display_data.push(0x00); // 字符间距 1 列
            }
        }

        // 🐛 [修复尾部空格 bug] 砍掉最后一个多余的间距字节！
        // 原因: 上面循环每个字符后无条件 push(0x00)，最后一个字符后面也多了一列
        // 效果: 8 字符的 "10:10:10" 原本 31 列 (24+7+尾巴1)，pop 后变 30 列，
        //       30>27 依然滚动但不再有尾部空白；7 字符的 "01:23:45"(27 列) 原本
        //       28 列导致误判触发 flow (其实居中完全够)，pop 后正好 27 列静态显示。
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
        // 进入滚动前同步 last_seen，避免之前已消费的按键误触发中断
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

            // 滚动期间检查按键中断，有按键立即退出滚动
            if self.poll_interrupt().is_some() {
                return Ok(());
            }
            // 把 128ms 拆成小段轮询，提高按键响应灵敏度 (20ms 内即可打断)
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
        // 左屏 14 列
        let left: Vec<u8> = values[..14].to_vec();
        self.unit_write(Line::StbLeft, COMMAND3, &left)?;
        // 右屏 13 列 + 状态灯 (用 disabled_led_mask 过滤掉用户关闭的灯位)
        let mut right_data = values[14..27].to_vec();
        let filtered_status = status & !self.disabled_led_mask;
        right_data.push(filtered_status);
        self.unit_write(Line::StbRight, COMMAND3, &right_data)?;
        Ok(())
    }

    // ==========================================
    // TM1628A 底层协议 (STB 选中 -> 命令字节 -> 数据字节 -> STB 释放)
    // ==========================================
    fn unit_write(&mut self, stb: Line, command: u8, values: &[u8]) -> Result<()> {
        self.bus.set(stb, LOW)?;
        self.write_command_byte(command)?;

        for (i, &value) in values.iter().enumerate() {
            self.write_data_byte(value, i % 2 != 0)?;
        }

        self.bus.set(stb, HIGH)?;
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
        self.bus.set(Line::Clk, LOW)?;
        self.bus.set(Line::Dio, bit)?;
        self.bus.set(Line::Clk, HIGH)?;
        Ok(())
    }
}

// ==========================================
// 🔍 辅助函数：自动探测主控芯片 GPIO base (sysfs 后端用)
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
            let ngpio = read_num("ngpio").unwrap_or(0);
            let label = fs::read_to_string(path.join("label"))
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            let is_main = label.contains("pinctrl") || label.contains("tlmm");

            let better = match &best {
                None => true,
                Some((_, best_n, best_main)) => {
                    (is_main && !best_main) || (is_main == *best_main && ngpio > *best_n)
                }
            };
            if better {
                best = Some((base, ngpio, is_main));
            }
        }
    }

    match best {
        Some((base, ngpio, _)) => {
            println!("🔍 [GPIO] 自动探测到主控芯片 base={} (ngpio={})", base, ngpio);
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
// 🔍 辅助函数：查找主控芯片字符设备路径 /dev/gpiochipN (cdev 后端用)
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
