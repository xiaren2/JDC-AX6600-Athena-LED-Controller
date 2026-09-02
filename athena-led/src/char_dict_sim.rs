// ==========================================================
// char_dict_sim.rs - 非 Unix 平台 stub
// Windows/Mac 下不会真正渲染字符, 仅提供空静态 HashMap 让编译通过
// 若后续想用模拟器预览, 可从 char_dict.rs 复制 CHAR_DICT 过来
// ==========================================================
use std::collections::HashMap;

// 空的 static DICT 占位（crate::char_dict::CHAR_DICT 被 led_screen.rs 引用）
// 为了编译通过，初始化一个完全空的 HashMap 即可
static EMPTY_DICT: once_cell::sync::Lazy<HashMap<char, [u8; 5]>> =
    once_cell::sync::Lazy::new(HashMap::new);

pub struct SimDict;
impl SimDict {
    pub fn get(&self, _k: &char) -> Option<&[u8; 5]> { None }
}

impl std::ops::Deref for SimDict {
    type Target = HashMap<char, [u8; 5]>;
    fn deref(&self) -> &Self::Target { &EMPTY_DICT }
}

pub static CHAR_DICT: SimDict = SimDict;
