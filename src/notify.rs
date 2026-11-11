//! 桌面通知封装：只负责把标题/正文交给 org.freedesktop.Notifications。
/// 发送 5 秒超时的通知；不包含输入原文之外的敏感信息。
pub fn send(title: &str, body: &str) -> Result<(), String> {
    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .timeout(5000)
        .show()
        .map(|_| ())
        .map_err(|e| format!("无法发送桌面通知：{e}"))
}
