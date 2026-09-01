// ==========================================
// 🖥️ led_screen.rs — AX6600 屏幕驱动 (修复版, gpiocdev 0.7 正确 API)
//
// 本次修复项 (对应 CI 2 个 E0433 + E0599):
//   ✅ gpiocdev 0.7 入口是 gpiocdev::Request::builder()
//        以前写的 gpiocdev::Builder::new() → E0433 (Builder 不在 crate 根)
//   ✅ 多条线批量申请用 .with_lines(&[..]) / .as_output(Value::Inactive)
//   ✅ 设置单条线用 req.set_value(offset, Value::Active/Inactive)
//        以前写的 req.set_line(offset, v) → E0599 (没这个方法)
//   ✅ 引脚偏移化: PIN_STB_LEFT=69 PIN_STB_RIGHT=70 PIN_CLK=73 PIN_DIO=74
//        sysfs 后端 gpio_base + offset; cdev 后端直接用 offset
//   ✅ 自动探测 gpio_base (兼容 base=512/432/0 的各类固件)
//   ✅ write_data 尾部 pop() 去掉多余空格 (刚好 27 列不触发滚动)
//   ✅ flow() 用 tokio::time::sleep (滚动不阻塞按键)
// ==========================================
#![allow(unused)]

use anyhow::{anyhow, Context, Result};
use crate::char_dict::CHAR_DICT;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOW:  u8 = 0x00;
const HIGH: u8 = 0x01;

// ★ AX6600 物理引脚偏移 (主控 TLMM, 芯片级 offset, 与 gpio_base 无关)
pub const PIN_STB_LEFT:  u32 = 69;
pub const PIN_STB_RIGHT: u32 = 70;
pub const PIN_CLK:       u32 = 73;
pub const PIN_DIO:       u32 = 74;

const COMMAND1: u8 = 0b00000011;
const COMMAND2: u8 = 0b01000000;
const COMMAND3: u8 = 0b11000000;

// ==========================================
// 🔎 自动探测 gpio_base (sysfs 用)
// ==========================================
pub fn detect_gpio_base() -> u64 {
    let mut best: Option<(u64, u64, bool)> = None;
    if let Ok(entries) = fs::read_dir("/sys/class/gpio") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("gpiochip") { continue; }
            let path = entry.path();
            let read = |f: &str| fs::read_to_string(path.join(f)).ok().and_then(|s| s.trim().parse::<u64>().ok());
            let base  = match read("base")  { Some(b) => b, None => continue };
            let ngpio = read("ngpio").unwrap_or(0);
            let label = fs::read_to_string(path.join("label")).unwrap_or_default().trim().to_lowercase();
            let is_main = label.contains("pinctrl") || label.contains("tlmm");
            let better = match &best {
                None => true,
                Some((_, bn, bm)) => (is_main && !*bm) || (is_main == *bm && ngpio > *bn),
            };
            if better { best = Some((base, ngpio, is_main)); }
        }
    }
    match best {
        Some((base, n, _)) => {
            println!("🔍 [GPIO] 自动探测主控 base={} ngpio={}", base, n);
            base
        }
        None => {
            println!("⚠️  [GPIO] 探测不到, 回退 base=512");
            512
        }
    }
}

// ==========================================
// 🔎 找主控字符设备 /dev/gpiochipN
// ==========================================
pub fn find_main_chip() -> Option<PathBuf> {
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
                Some((_, bn, bm)) => (is_main && !*bm) || (is_main == *bm && info.num_lines > *bn),
            };
            if better { best = Some((path, info.num_lines, is_main)); }
        }
    }
    best.map(|(p, n, _)| {
        println!("🔍 [GPIO] 主控字符设备 {} (lines={})", p.display(), n);
        p
    })
}

// ==========================================
// 🧱 引脚枚举
// ==========================================
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Line { StbLeft, StbRight, Clk, Dio }

// ==========================================
// 🧱 GPIO 双后端 (cdev 优先, sysfs 兜底)
// ==========================================
enum GpioBus {
    Cdev { req: gpiocdev::Request },
    Sysfs {
        stb_l: sysfs_gpio::Pin,
        stb_r: sysfs_gpio::Pin,
        clk:   sysfs_gpio::Pin,
        dio:   sysfs_gpio::Pin,
    },
}

impl GpioBus {
    fn set(&mut self, line: Line, level: u8) -> Result<()> {
        let v = if level == LOW {
            gpiocdev::line::Value::Inactive
        } else {
            gpiocdev::line::Value::Active
        };
        match self {
            // ✅ 修复: gpiocdev 0.7 写单条线 → req.set_value(offset, Value)
            //         之前写 set_line(offset, v) → E0599 方法不存在
            GpioBus::Cdev { req } => {
                let offset = match line {
                    Line::StbLeft  => PIN_STB_LEFT,
                    Line::StbRight => PIN_STB_RIGHT,
                    Line::Clk      => PIN_CLK,
                    Line::Dio      => PIN_DIO,
                };
                req.set_value(offset, v)
                    .with_context(|| format!("设置 GPIO offset={} level={:?} 失败", offset, level))?;
            }
            GpioBus::Sysfs { stb_l, stb_r, clk, dio } => {
                let pin = match line {
                    Line::StbLeft  => *stb_l,
                    Line::StbRight => *stb_r,
                    Line::Clk      => *clk,
                    Line::Dio      => *dio,
                };
                pin.set_value(level)
                    .with_context(|| format!("sysfs 写引脚失败 line={:?}", line))?;
            }
        }
        Ok(())
    }
}

impl Drop for GpioBus {
    fn drop(&mut self) {
        if let GpioBus::Sysfs { stb_l, stb_r, clk, dio } = self {
            let _ = stb_l.unexport();
            let _ = stb_r.unexport();
            let _ = clk.unexport();
            let _ = dio.unexport();
        }
    }
}

// ==========================================
// 🖥️ LedScreen 对外结构
// ==========================================
pub struct LedScreen {
    bus: GpioBus,
}

impl LedScreen {
    /// backend = "auto" | "cdev" | "sysfs"
    /// gpio_base = "auto" | "<number>"
    pub fn new(backend: &str, gpio_base: &str) -> Result<Self> {
        use gpiocdev::line::Value;
        let bus = match backend.trim() {
            "cdev" => Self::open_cdev()?,
            "sysfs" => Self::open_sysfs(gpio_base)?,
            _ => match Self::open_cdev() {
                Ok(b) => b,
                Err(e) => {
                    println!("⚠️  [GPIO] cdev 后端不可用 ({}), 回退 sysfs", e);
                    Self::open_sysfs(gpio_base)?
                }
            },
        };
        let mut s = Self { bus };
        s.set_show_model()?;
        s.set_data_model()?;
        Ok(s)
    }

    fn open_cdev() -> Result<GpioBus> {
        use gpiocdev::line::Value;
        let chip = find_main_chip().ok_or_else(|| anyhow!("没有 /dev/gpiochip* 字符设备"))?;

        // ✅ 修复: gpiocdev 0.7 正确入口是 gpiocdev::Request::builder()
        //         之前写 gpiocdev::Builder::new() → E0433 (Builder 不在根命名空间)
        let req = gpiocdev::Request::builder()
            .on_chip(&chip)
            .with_consumer("athena-led")
            .with_lines(&[PIN_STB_LEFT, PIN_STB_RIGHT, PIN_CLK, PIN_DIO])
            .as_output(Value::Inactive)
            .request()
            .with_context(|| format!("请求字符设备 {} 的屏幕引脚失败", chip.display()))?;

        println!("🔌 [GPIO] cdev 后端: {}", chip.display());
        Ok(GpioBus::Cdev { req })
    }

    fn open_sysfs(gpio_base: &str) -> Result<GpioBus> {
        let base: u64 = match gpio_base.trim() {
            "" | "auto" => detect_gpio_base(),
            s => match s.parse::<u64>() {
                Ok(n) => n,
                Err(_) => {
                    println!("⚠️  [GPIO] gpio-base '{}' 解析失败, 改为自动探测", s);
                    detect_gpio_base()
                }
            },
        };
        let make = |off: u32, name: &str| -> Result<sysfs_gpio::Pin> {
            let num = base + off as u64;
            let p = sysfs_gpio::Pin::new(num);
            p.export().with_context(|| format!("导出 GPIO{} ({}) 失败", num, name))?;
            p.set_direction(sysfs_gpio::Direction::Out)
                .with_context(|| format!("GPIO{} ({}) 设置 out 方向失败", num, name))?;
            Ok(p)
        };
        let sl = make(PIN_STB_LEFT,  "STB_L")?;
        let sr = make(PIN_STB_RIGHT, "STB_R")?;
        let ck = make(PIN_CLK,       "CLK")?;
        let di = make(PIN_DIO,       "DIO")?;
        println!(
            "🔌 [GPIO] sysfs 后端 base={} (引脚 {}/{}/{}/{})",
            base,
            base + PIN_STB_LEFT as u64, base + PIN_STB_RIGHT as u64,
            base + PIN_CLK as u64,      base + PIN_DIO as u64
        );
        Ok(GpioBus::Sysfs { stb_l: sl, stb_r: sr, clk: ck, dio: di })
    }

    pub fn set_show_model(&mut self) -> Result<()> {
        self.unit_write(Line::StbLeft,  COMMAND1, &[])?;
        self.unit_write(Line::StbRight, COMMAND1, &[])?;
        Ok(())
    }

    pub fn set_data_model(&mut self) -> Result<()> {
        self.unit_write(Line::StbLeft,  COMMAND2, &[])?;
        self.unit_write(Line::StbRight, COMMAND2, &[])?;
        Ok(())
    }

    pub fn power(&mut self, run: bool, light_level: u8) -> Result<()> {
        let cmd = if run {
            (light_level << 5 >> 5 | 0b11111000) & 0b10001111
        } else { 0b10000000 };
        self.unit_write(Line::StbLeft,  cmd, &[])?;
        self.unit_write(Line::StbRight, cmd, &[])?;
        Ok(())
    }

    /// 写入字符串 (异步: 长文本 flow() 内部 tokio::sleep 不阻塞按键)
    pub async fn write_data(&mut self, text: &[u8], status: u8) -> Result<()> {
        let mut data: Vec<u8> = Vec::new();
        let s = std::str::from_utf8(text).unwrap_or("");
        for ch in s.chars() {
            let key = ch.to_ascii_uppercase();
            if let Some(bytes) = CHAR_DICT.get(&key) {
                data.extend_from_slice(bytes);
                data.push(0x00); // 字间距
            }
        }
        // ★ 修复: 砍掉最后一个多余空格, 刚好 27 列不触发滚动
        if data.last() == Some(&0x00) { data.pop(); }

        if data.len() > 27 { self.flow(&data, status).await?; }
        else               { self.static_display(&data, status)?; }
        Ok(())
    }

    async fn flow(&mut self, data: &[u8], status: u8) -> Result<()> {
        let mut start = 0usize;
        for i in 1..=data.len() {
            let mut win = [0u8; 27];
            if i > 27 { start += 1; }
            let end = i.min(27);
            win[..end].copy_from_slice(&data[start..start + end]);
            self.do_write(&win, status)?;
            // ★ 修复: tokio 异步睡眠代替 std::thread::sleep, 不阻塞 tokio 线程池
            tokio::time::sleep(Duration::from_millis(128)).await;
        }
        Ok(())
    }

    pub async fn play_animation(&mut self, file: &str, dur: u64, status: u8) -> Result<()> {
        let path = format!("/etc/athena_led/anim/{}", file);
        let meta = fs::metadata(&path).ok();
        if let Some(m) = &meta {
            if m.len() > 5 * 1024 * 1024 {
                eprintln!("❌ 动画 > 5MB: {}", path);
                return self.static_display(b"TOO LARGE", status);
            }
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("❌ 动画文件读取失败 {}: {}", path, e);
                return self.static_display(b"FILE ERR", status);
            }
        };
        let n_frames = bytes.len() / 27;
        if n_frames == 0 { return Ok(()); }

        let deadline = Instant::now() + Duration::from_secs(dur);
        let mut it = bytes.chunks_exact(27).cycle();
        while Instant::now() < deadline {
            if let Some(f) = it.next() { self.do_write(f, status)?; }
            tokio::time::sleep(Duration::from_millis(66)).await;
        }
        Ok(())
    }

    fn static_display(&mut self, data: &[u8], status: u8) -> Result<()> {
        let mut buf = [0u8; 27];
        if data.len() < 27 {
            let o = (27 - data.len()) / 2;
            buf[o..o + data.len()].copy_from_slice(data);
        } else {
            buf.copy_from_slice(&data[..27]);
        }
        self.do_write(&buf, status)?;
        Ok(())
    }

    fn do_write(&mut self, values: &[u8], status: u8) -> Result<()> {
        let left: Vec<u8> = values[..14].to_vec();
        self.unit_write(Line::StbLeft,  COMMAND3, &left)?;
        let mut right: Vec<u8> = values[14..27].to_vec();
        right.push(status);
        self.unit_write(Line::StbRight, COMMAND3, &right)?;
        Ok(())
    }

    fn unit_write(&mut self, stb: Line, cmd: u8, vals: &[u8]) -> Result<()> {
        self.bus.set(stb, LOW)?;
        self.write_byte_cmd(cmd)?;
        for (i, &v) in vals.iter().enumerate() {
            self.write_byte_data(v, i % 2 != 0)?;
        }
        self.bus.set(stb, HIGH)?;
        Ok(())
    }

    fn write_byte_cmd(&mut self, v: u8) -> Result<()> {
        for i in 0..8 { self.write_bit((v >> i) & 1)?; }
        Ok(())
    }

    fn write_byte_data(&mut self, v: u8, fill: bool) -> Result<()> {
        for i in 0..5 { self.write_bit((v >> i) & 1)?; }
        if fill { for _ in 0..6 { self.write_bit(LOW)?; } }
        Ok(())
    }

    fn write_bit(&mut self, b: u8) -> Result<()> {
        self.bus.set(Line::Clk, LOW)?;
        self.bus.set(Line::Dio, b)?;
        self.bus.set(Line::Clk, HIGH)?;
        Ok(())
    }
}
