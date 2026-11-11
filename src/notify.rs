pub fn send(title: &str, body: &str) -> Result<(), String> {
    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .timeout(5000)
        .show()
        .map(|_| ())
        .map_err(|e| format!("无法发送桌面通知：{e}"))
}
