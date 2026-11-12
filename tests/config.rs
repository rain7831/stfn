//! 配置读写测试：手动配色字段与非法值回退。
use stfn::config::{self, ColorScheme};

#[test]
fn color_scheme_roundtrip_and_invalid_fallback() {
    let temp = tempfile::tempdir().unwrap();
    // 该测试二进制只包含这一个测试，修改进程级 XDG_CONFIG_HOME 是安全的。
    std::env::set_var("XDG_CONFIG_HOME", temp.path());

    let mut config = config::Config::default();
    config.general.color_scheme = ColorScheme::Dark;
    config::save(&config).unwrap();
    let loaded = config::load();
    assert_eq!(loaded.config.general.color_scheme, ColorScheme::Dark);

    std::fs::write(
        config::config_path(),
        "version = 1\n[general]\ncolor_scheme = \"nope\"\n",
    )
    .unwrap();
    let loaded = config::load();
    assert_eq!(loaded.config.general.color_scheme, ColorScheme::Default);
    assert!(loaded
        .warnings
        .iter()
        .any(|warning| warning.contains("color_scheme")));
}
