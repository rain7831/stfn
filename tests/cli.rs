//! CLI 端到端测试。
//!
//! 所有测试通过 XDG_CONFIG_HOME 使用临时配置目录，避免污染真实用户配置；
//! 网络请求用 httpmock 拦截，验证 CLI 参数、provider 协议和输出格式。
use assert_cmd::Command;
use httpmock::prelude::*;
use predicates::prelude::*;

// 构造 Bing 网页会话 HTML + ttranslatev3 响应；extra 用于测试第二个数组元素。
fn mock_bing(server: &MockServer, translated: &str, extra: &str) {
    server.mock(|when, then| {
        when.method(GET).path("/translator");
        then.status(200)
            .header("content-type", "text/html")
            .body(r#"<html><script>IG:"IG123"; data-iid="translator.5025"; params_AbusePreventionHelper = [1794365788717,"token123",3600000];</script></html>"#);
    });
    server.mock(|when, then| {
        when.method(POST).path("/ttranslatev3");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"[{{"translations":[{{"text":"{translated}"}}],"detectedLanguage":{{"language":"en"}}}}{extra}]"#
            ));
    });
}

// 统一测试命令：注入 mock Bing base URL 和临时配置目录。
fn cmd(server: &MockServer, dir: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("stfn").unwrap();
    command
        .env("STFN_BING_BASE_URL", server.base_url())
        .env("XDG_CONFIG_HOME", dir);
    command
}

#[test]
// en -> zh 的基本路径。
fn translates_english_to_chinese() {
    let server = MockServer::start();
    mock_bing(&server, "你好", "");
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("你好"));
}

#[test]
// zh -> en 时 Bing 会多返回一个 transliteration 元素，解析应跳过。
fn translates_chinese_to_english_with_extra_array_item() {
    let server = MockServer::start();
    mock_bing(&server, "Hello", r#",{"inputTransliteration":"Nǐ hǎo"}"#);
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .arg("你好")
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello"));
}

#[test]
// 校验 --json 的稳定字段。
fn json_output_is_stable() {
    let server = MockServer::start();
    mock_bing(&server, "你好", "");
    let temp = tempfile::tempdir().unwrap();

    let output = cmd(&server, temp.path())
        .args(["--json", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["source"], "en");
    assert_eq!(value["target"], "zh");
    assert_eq!(value["results"][0]["provider"], "bing");
    assert_eq!(value["results"][0]["text"], "你好");
    assert!(value["errors"].as_array().unwrap().is_empty());
}

#[test]
// --notify 与 --json 互斥。
fn notify_and_json_conflict() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .args(["--notify", "--json", "hello"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("不能同时使用"));
}

#[test]
// 空输入应返回用法错误。
fn empty_input_is_usage_error() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path()).arg("").assert().code(2);
}

#[test]
// 日文等非中英输入应明确拒绝。
fn unsupported_language_is_usage_error() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .arg("こんにちは")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("仅支持中英"));
}

#[test]
// 显式 `-` 才读取 stdin，避免管道误触发。
fn stdin_input_works() {
    let server = MockServer::start();
    mock_bing(&server, "你好", "");
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .arg("-")
        .write_stdin("hello\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("你好"));
}

#[test]
// --source all 在只启用 Bing 时仍可工作。
fn source_all_uses_bing_when_only_bing_enabled() {
    let server = MockServer::start();
    mock_bing(&server, "你好", "");
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .args(["--source", "all", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("你好"));
}

#[test]
// v0.1 中朗读明确返回 v1.1 提示。
fn speak_is_v11() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();

    cmd(&server, temp.path())
        .args(["--speak", "hello"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("v1.1"));
}

#[test]
// 阿里云请求参数/签名返回 + JSON 解析。
fn alibaba_translates_with_mock() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST)
            .path("/")
            .body_contains("Action=TranslateGeneral")
            .body_contains("AccessKeyId=test-id")
            .body_contains("SourceText=hello")
            .body_contains("Signature=");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"Code":200,"Message":"success","RequestId":"test","Data":{"Translated":"你好","DetectedLanguage":"en","WordCount":"5"}}"#);
    });
    let temp = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("stfn")
        .unwrap()
        .env("XDG_CONFIG_HOME", temp.path())
        .env("STFN_ALI_BASE_URL", server.base_url())
        .env("STFN_ALI_ACCESS_KEY_ID", "test-id")
        .env("STFN_ALI_ACCESS_KEY_SECRET", "test-secret")
        .args(["--source", "alibaba", "--json", "hello"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["results"][0]["text"], "你好");
}

#[test]
// 火山必须使用 TextList，并正确读取 DetectedSourceLanguage。
fn volcengine_uses_textlist_and_parses_detected_source() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST)
            .path("/")
            .body_contains("TextList")
            .body_contains("TargetLanguage");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"TranslationList":[{"Translation":"你好","DetectedSourceLanguage":"en"}],"ResponseMetadata":{"RequestId":"test"}}"#);
    });
    let temp = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("stfn")
        .unwrap()
        .env("XDG_CONFIG_HOME", temp.path())
        .env("STFN_VOLC_BASE_URL", server.base_url())
        .env("STFN_VOLC_ACCESS_KEY", "test-ak")
        .env("STFN_VOLC_SECRET_KEY", "test-sk")
        .args(["--source", "volcengine", "--json", "hello"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["results"][0]["text"], "你好");
    assert_eq!(value["results"][0]["detected_source"], "en");
}
