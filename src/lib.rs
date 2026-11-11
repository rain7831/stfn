//! STFN（Simple Translator For Niri）库入口。
//!
//! 模块分层：
//! - `cli`：命令行解析、翻译执行与输出；
//! - `config` / `secrets`：配置与密钥；
//! - `lang` / `translate` / `providers`：语言检测、统一翻译接口和三家 provider；
//! - `ui`：GTK4 主窗口、应用内设置页；
//! - `notify`：桌面通知。
pub mod cli;
pub mod config;
pub mod lang;
pub mod notify;
pub mod providers;
pub mod secrets;
pub mod translate;
pub mod ui;
