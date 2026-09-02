// ==========================================================
// led_screen_sim.rs - 非 Unix 平台的虚拟屏幕实现 (Windows/Mac)
// 仅为本地 cargo check / clippy / cargo build 可通过而存在
// 所有方法都是空操作，不会操作真实硬件
// ==========================================================
#![allow(dead_code)]

use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub const PIN_STB_LEFT:  u32 = 69;
pub const PIN_STB_RIGHT: u32 = 70;
pub const PIN_CLK:       u32 = 73;
pub const PIN_DIO:       u32 = 74;

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

pub struct LedScreen {
    disabled_led_mask: u8,
    interrupt_rx: Option<Arc<Mutex<watch::Receiver<i32>>>>,
    interrupt_last_seen: i32,
}

impl LedScreen {
    pub fn new(_backend: GpioBackend, _gpio_base: u64, disabled_led_mask: u8) -> Result<Self> {
        Ok(Self { disabled_led_mask, interrupt_rx: None, interrupt_last_seen: 0 })
    }

    pub fn new_with_mask(_l: u64, _r: u64, _c: u64, _d: u64, mask: u8) -> Result<Self> {
        Self::new(GpioBackend::Auto, 512, mask)
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

    pub fn set_show_model(&mut self) -> Result<()> { Ok(()) }
    pub fn set_data_model(&mut self) -> Result<()> { Ok(()) }
    pub fn power(&mut self, _run: bool, _light_level: u8) -> Result<()> { Ok(()) }
    pub async fn write_data(&mut self, _text: &[u8], _status: u8) -> Result<()> { Ok(()) }
}

pub fn detect_gpio_base() -> u64 { 512 }
pub fn find_main_chip() -> Option<std::path::PathBuf> { None }
