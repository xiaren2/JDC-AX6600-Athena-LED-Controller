use anyhow::Result;
use sysfs_gpio::{Direction, Pin};
use crate::char_dict::CHAR_DICT;
use tokio::sync::watch;
use std::sync::{Arc, Mutex};

const LOW: u8 = 0x00;
const HIGH: u8 = 0x01;

// Display mode commands
const COMMAND1: u8 = 0b00000011; // Display mode
const COMMAND2: u8 = 0b01000000; // Data mode
const COMMAND3: u8 = 0b11000000; // Display address

pub struct LedScreen {
    left_screen: LedScreenUnit,
    right_screen: LedScreenUnit,
    // 禁用 LED 掩码: bit0=时钟 bit1=奖牌 bit2=上箭头 bit3=下箭头
    // 1 = 禁用 (用户勾选了关闭), 0 = 正常
    disabled_led_mask: u8,
    // 可选: 按键 watch channel. 存在时, flow() 滚动显示每帧都会检查按键中断
    interrupt_rx: Option<Arc<Mutex<watch::Receiver<i32>>>>,
    // 检查中断时跟踪的 last_seen 缓存
    interrupt_last_seen: i32,
}

pub struct LedScreenUnit {
    stb: Pin,
    clk: Pin,
    dio: Pin,
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

    /// 绑定按键 watch channel, 后续 flow/滚动 会在每帧检查按键中断
    pub fn bind_interrupt_rx(&mut self, rx: Arc<Mutex<watch::Receiver<i32>>>) {
        self.interrupt_last_seen = *rx.lock().unwrap().borrow();
        self.interrupt_rx = Some(rx);
    }

    /// 检查是否有待处理按键/息屏指令.
    /// 返回 Some(cmd) 表示有新命令: -1=息屏, >0=切台, None=无
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
        
        // [核心修复] 
        // 1. 尝试把字节流转成 UTF-8 字符串
        // 2. 按【字符】(chars) 遍历，而不是按字节
        // 这样 '℃'、'☀' 这种多字节符号才能被正确识别！
        let content = std::str::from_utf8(text).unwrap_or("");
        
        for ch in content.chars() {
            // 统一转大写匹配 (兼容 a-z)
            let key = ch.to_ascii_uppercase(); 
            
            if let Some(bytes) = CHAR_DICT.get(&key) {
                display_data.extend_from_slice(bytes);
                // 统一加 1 列间距
                display_data.push(0x00); 
            }
        }

        if display_data.len() > 27 {
            self.flow(&display_data, status)?;
        } else {
            self.static_display(&display_data, status)?;
        }
        Ok(())
    }

    fn flow(&mut self, data: &[u8], status: u8) -> Result<()> {
        // 进入滚动前同步 last_seen, 避免之前已消费的按键误触发中断
        // 只有 flow 期间新发生的按键才应该中断滚动
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

            // 滚动期间检查按键中断 (短按切台/长按息屏/双击), 有按键立即退出滚动
            if self.poll_interrupt().is_some() {
                return Ok(());
            }
            // 把 128ms 拆成小段轮询, 提高按键响应灵敏度 (20ms 内即可打断)
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
        // 4 盏独立状态 LED: 用 disabled_led_mask 清除用户选择关闭的位
        // disabled_led_mask 上 1 表示关闭，所以取反后按位与
        let filtered_status = status & !self.disabled_led_mask;
        right_data.push(filtered_status);
        self.right_screen.printf(&right_data)?;
        Ok(())
    }
}

impl LedScreenUnit {
    fn new(stb: u64, clk: u64, dio: u64) -> Result<Self> {
        let stb_pin = Pin::new(stb);
        let clk_pin = Pin::new(clk);
        let dio_pin = Pin::new(dio);

        stb_pin.export()?;
        clk_pin.export()?;
        dio_pin.export()?;

        stb_pin.set_direction(Direction::Out)?;
        clk_pin.set_direction(Direction::Out)?;
        dio_pin.set_direction(Direction::Out)?;

        Ok(Self {
            stb: stb_pin,
            clk: clk_pin,
            dio: dio_pin,
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

impl Drop for LedScreenUnit {
    fn drop(&mut self) {
        let _ = self.stb.unexport();
        let _ = self.clk.unexport();
        let _ = self.dio.unexport();
    }
}

// ==========================================
// 🌟 辅助函数：自动探测主控芯片 GPIO base
// 扫描 /sys/class/gpio/gpiochip*/ 找出 TLMM/pinctrl 芯片
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
// 🌟 辅助函数：查找主控芯片的字符设备路径 /dev/gpiochipN
// 优先选 label 含 pinctrl/tlmm 的芯片
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
