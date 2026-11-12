//! 英文词典查询、IPA/accent 解析、音频缓存与单飞测试。
//!
//! 通过 `PronunciationService::with_base_url` 指向 httpmock，不访问真实网络。
use std::sync::Arc;
use std::time::Duration;

use httpmock::prelude::*;
use stfn::pronunciation::{Accent, AudioSource, PronunciationService};

fn service(server: &MockServer) -> PronunciationService {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    PronunciationService::with_base_url(client, server.base_url())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

#[test]
fn parses_us_uk_and_falls_back_to_edge_tts() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"[{"word":"hello","phonetics":[
                    {"audio":"https://example.com/hello-au.mp3"},
                    {"text":"/həˈləʊ/","audio":"https://example.com/hello-uk.mp3"},
                    {"text":"/həˈloʊ/","audio":""}
                ]}]"#,
            );
    });

    let result = runtime()
        .block_on(service(&server).lookup("hello"))
        .expect("dictionary hit");

    let us = result.us.expect("US IPA");
    assert_eq!(us.ipa, "/həˈloʊ/");
    assert_eq!(
        us.audio,
        Some(AudioSource::EdgeTts {
            voice: "en-US-AriaNeural"
        })
    );

    let uk = result.uk.expect("UK IPA");
    assert_eq!(uk.ipa, "/həˈləʊ/");
    assert_eq!(
        uk.audio,
        Some(AudioSource::DictionaryUrl(
            "https://example.com/hello-uk.mp3".to_string()
        ))
    );
    assert!(result.generic.is_none());
}

#[test]
fn single_accent_only_returns_that_accent() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/word");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"[{"word":"word","phonetics":[
                    {"text":"/wɜːd/","audio":"https://example.com/word-uk.mp3"}
                ]}]"#,
            );
    });

    let result = runtime()
        .block_on(service(&server).lookup("word"))
        .expect("dictionary hit");
    assert!(result.us.is_none());
    assert_eq!(result.uk.unwrap().ipa, "/wɜːd/");
    assert!(result.generic.is_none());
}

#[test]
fn unmarked_ipa_alone_becomes_generic() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/test");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"[{"word":"test","phonetics":[{"text":"/tɛst/","audio":""}]}]"#);
    });

    let result = runtime()
        .block_on(service(&server).lookup("test"))
        .expect("dictionary hit");
    assert!(result.us.is_none());
    assert!(result.uk.is_none());
    let generic = result.generic.expect("generic IPA");
    assert_eq!(generic.ipa, "/tɛst/");
    assert!(matches!(generic.audio, Some(AudioSource::EdgeTts { .. })));
}

#[test]
fn missing_word_returns_none() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/zzzzqqqxx");
        then.status(404)
            .header("content-type", "application/json")
            .body(r#"{"title":"No Definitions Found"}"#);
    });

    let result = runtime().block_on(service(&server).lookup("zzzzqqqxx"));
    assert!(result.is_none());
    mock.assert_hits(1);
}

#[test]
fn non_word_does_not_request_dictionary() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET);
        then.status(200).body("[]");
    });

    let service = service(&server);
    let runtime = runtime();
    assert!(runtime.block_on(service.lookup("hello world")).is_none());
    assert!(runtime.block_on(service.lookup("中文")).is_none());
    mock.assert_hits(0);
}

#[test]
fn same_word_uses_inflight_dedup_and_cache() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"[{"word":"hello","phonetic":"/həˈloʊ/","phonetics":[]}]"#);
    });

    let service = service(&server);
    let runtime = runtime();
    runtime.block_on(async {
        let (first, second) = tokio::join!(service.lookup("hello"), service.lookup("Hello"));
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(service.lookup("hello").await.is_some());
    });
    mock.assert_hits(1);
}

#[test]
fn dictionary_audio_is_downloaded_and_cached() {
    let server = MockServer::start();
    let audio = b"fake-mp3-bytes".to_vec();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/hello-us.mp3");
        then.status(200)
            .header("content-type", "audio/mpeg")
            .body(audio.clone());
    });

    let service = service(&server);
    let runtime = runtime();
    let source = AudioSource::DictionaryUrl(format!("{}/hello-us.mp3", server.base_url()));
    runtime.block_on(async {
        let first = service.audio("hello", Accent::Us, &source).await.unwrap();
        let second = service.audio("hello", Accent::Us, &source).await.unwrap();
        assert_eq!(first.as_slice(), audio.as_slice());
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    });
    mock.assert_hits(1);
}

#[test]
fn concurrent_audio_requests_share_one_download() {
    let server = MockServer::start();
    let audio = b"fake-mp3-bytes".to_vec();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/hello-us.mp3");
        then.status(200)
            .header("content-type", "audio/mpeg")
            .delay(Duration::from_millis(250))
            .body(audio.clone());
    });

    let service = service(&server);
    let runtime = runtime();
    let source = AudioSource::DictionaryUrl(format!("{}/hello-us.mp3", server.base_url()));
    runtime.block_on(async {
        let (first, second) = tokio::join!(
            service.audio("hello", Accent::Us, &source),
            service.audio("hello", Accent::Us, &source)
        );
        let first = first.expect("first audio");
        let second = second.expect("second audio");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.as_slice(), audio.as_slice());
    });
    mock.assert_hits(1);
}
