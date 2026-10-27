// RAMiao 内存喵 — by Miaomiao (github.com/Miaomiaomeowmeow) · CC BY-NC-SA 4.0
// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    ramiao_lib::run()
}
