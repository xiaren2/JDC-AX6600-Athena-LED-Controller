// ==========================================
// 🎛️ control.rs — 运行时共享控制状态
// 为按键双击检测 (go_home) 提供共享锁
// ==========================================
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct ControlState {
    // 双击按键: 回到频道 1
    pub go_home: bool,
}

pub type SharedControl = Arc<Mutex<ControlState>>;

pub fn new_shared() -> SharedControl {
    Arc::new(Mutex::new(ControlState::default()))
}
