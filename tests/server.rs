//! Server lifecycle as driven by the tray menu: start, stop, edit config.json, reload, restart.

use std::path::PathBuf;

use printer_bridge::bridge::AppState;
use printer_bridge::config;
use printer_bridge::server::ServerController;

fn temp_config(json: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("printer-bridge-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(&path, json).unwrap();
    path
}

fn get(url: &str, auth: Option<&str>) -> Result<(u16, String, Option<String>), reqwest::Error> {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut request = reqwest::Client::new().get(url);
        if let Some(auth) = auth {
            request = request.header("Authorization", auth);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let www = response
            .headers()
            .get("www-authenticate")
            .map(|v| v.to_str().unwrap().to_string());
        Ok((status, response.text().await?, www))
    })
}

#[test]
fn restart_applies_edited_config() {
    let path = temp_config(r#"{"server":{"bind":"127.0.0.1","port":0},"serial":{"enabled":true}}"#);
    let state = AppState::new(config::load(&path).unwrap(), path.clone());
    let server = ServerController::new(state.clone()).unwrap();

    let addr = server.start().unwrap();
    let (status, body, _) = get(&format!("http://{addr}/system/version.json"), None).unwrap();
    assert_eq!(status, 200);
    assert!(body.contains(&format!(r#""appId":"{}""#, printer_bridge::APP_ID)));

    server.stop();
    assert!(
        get(&format!("http://{addr}/system/version.json"), None).is_err(),
        "server still listening after stop"
    );

    // User edits config.json (enables the token), then chooses Restart.
    std::fs::write(
        &path,
        r#"{"server":{"bind":"127.0.0.1","port":0,"authentication":{"enabled":true,"token":"s3cret"}}}"#,
    )
    .unwrap();
    state.reload().unwrap();
    let addr = server.start().unwrap();
    let url = format!("http://{addr}/system/printers.json");

    let (status, body, www) = get(&url, None).unwrap();
    assert_eq!((status, body.as_str()), (401, "Token mismatch"));
    assert_eq!(www.as_deref(), Some("Basic realm=\"Token required\""));
    assert_eq!(get(&url, Some("Bearer wrong")).unwrap().0, 401);
    assert_eq!(get(&url, Some("Bearer s3cret")).unwrap().0, 200);
    // Basic auth with empty user and the token as password (":s3cret").
    assert_eq!(get(&url, Some("Basic OnMzY3JldA==")).unwrap().0, 200);

    server.stop();
}

#[test]
fn invalid_config_is_reported_and_kept() {
    let path = temp_config(r#"{"server":{"bind":"127.0.0.1","port":0}}"#);
    let state = AppState::new(config::load(&path).unwrap(), path.clone());

    std::fs::write(&path, r#"{"server":{"port":"not a number"}"#).unwrap();
    let error = format!("{:#}", state.reload().unwrap_err());
    assert!(
        error.contains("invalid") && error.contains("config.json"),
        "{error}"
    );
    // The previous config stays active and the broken file is not overwritten.
    assert_eq!(state.config().server.port, 0);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("not a number")
    );
}

#[test]
fn save_settings_replaces_managed_keys_only() {
    let path = temp_config(
        "\u{feff}{\"gui\":{\"notification\":{\"enabled\":true}},\
         \"server\":{\"address\":\"192.168.1.20\",\"bind\":\"192.168.1.20\",\"port\":12212,\
         \"authentication\":{\"enabled\":false,\"token\":null},\"tls\":{\"enabled\":false}},\
         \"downloader\":{\"timeout\":12.0,\"path\":\"dl\"},\
         \"printer\":{\"enabled\":true,\"mappings\":[{\"type\":\"OLD\",\"name\":\"X\"}]},\
         \"serial\":{\"enabled\":true,\"mappings\":[]}}",
    );
    let mut config = config::load(&path).unwrap();
    assert!(config.log.enabled, "log file is on unless switched off");
    config.gui.notification.enabled = false;
    config.log.enabled = false;
    config.server.bind = "0.0.0.0".into();
    config.server.port = 12300;
    config.server.authentication.enabled = true;
    config.server.authentication.token = Some("t0k".into());
    config.printer.fallback_to_default = true;
    config.printer.mappings = vec![
        config::PrinterMapping {
            kind: "LABEL".into(),
            name: "BROTHER-QL".into(),
            auto_rotate: true,
            ..Default::default()
        },
        config::PrinterMapping {
            kind: "INVOICE".into(),
            name: "HP-LASERJET".into(),
            force_dpi: 300,
            ..Default::default()
        },
    ];
    config::save_settings(&path, &config).unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // Untouched keys survive, key order is kept.
    assert_eq!(raw["server"]["address"], "192.168.1.20");
    assert_eq!(raw["server"]["tls"]["enabled"], false);
    assert_eq!(raw["downloader"]["path"], "dl");
    assert_eq!(raw["serial"]["enabled"], true);
    let keys: Vec<_> = raw.as_object().unwrap().keys().cloned().collect();
    // Existing keys keep their order; the port's own `log` key is appended.
    assert_eq!(
        keys,
        ["gui", "server", "downloader", "printer", "serial", "log"]
    );
    assert_eq!(raw["log"]["enabled"], false);

    let reloaded = config::load(&path).unwrap();
    assert!(!reloaded.gui.notification.enabled);
    assert!(!reloaded.log.enabled);
    assert_eq!(
        (reloaded.server.bind.as_str(), reloaded.server.port),
        ("0.0.0.0", 12300)
    );
    assert_eq!(reloaded.server.authentication.token.as_deref(), Some("t0k"));
    assert!(reloaded.printer.fallback_to_default && reloaded.printer.enabled);
    assert_eq!(reloaded.printer.mappings.len(), 2);
    assert!(reloaded.printer.mappings[0].auto_rotate);
    assert_eq!(reloaded.printer.mappings[1].force_dpi, 300);
    assert_eq!(reloaded.downloader.timeout, 12.0);
}

#[test]
fn validate_reports_unusable_settings() {
    let mut config = config::Config::default();
    assert!(config::validate(&config).is_empty());

    config.printer.mappings = vec![
        config::PrinterMapping {
            kind: "LABEL".into(),
            ..Default::default()
        },
        config::PrinterMapping {
            kind: "LABEL".into(),
            ..Default::default()
        },
        config::PrinterMapping {
            kind: " ".into(),
            ..Default::default()
        },
    ];
    config.server.port = 0;
    config.server.authentication.enabled = true;
    let problems = config::validate(&config);
    assert_eq!(problems.len(), 4, "{problems:?}");
    assert!(problems[0].contains("\"LABEL\" is listed twice"));
}

#[test]
fn config_with_byte_order_mark_loads() {
    let path = temp_config("\u{feff}{\"server\":{\"port\":4242}}");
    assert_eq!(config::load(&path).unwrap().server.port, 4242);
    config::add_print_type(&path, "X").unwrap();
    assert_eq!(config::load(&path).unwrap().printer.mappings[0].kind, "X");
}

#[test]
fn bind_error_names_the_address() {
    // 192.0.2.1 (TEST-NET-1) is never assigned to a local interface.
    let path = temp_config(r#"{"server":{"bind":"192.0.2.1","port":12212}}"#);
    let state = AppState::new(config::load(&path).unwrap(), path);
    let server = ServerController::new(state).unwrap();
    let error = format!("{:#}", server.start().unwrap_err());
    assert!(
        error.starts_with("Cannot listen on 192.0.2.1:12212"),
        "{error}"
    );
}

#[test]
fn auto_add_keeps_unknown_keys_and_order() {
    let path = temp_config(
        r#"{"gui":{"notification":{"enabled":true}},"server":{"tls":{"enabled":false}},"printer":{"mappings":[{"type":"LABEL","name":"X"}]},"serial":{"enabled":true}}"#,
    );
    config::add_print_type(&path, "INVOICE").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["printer"]["mappings"][1]["type"], "INVOICE");
    assert_eq!(value["printer"]["mappings"][1]["name"], "");
    assert_eq!(value["server"]["tls"]["enabled"], false);
    let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["gui", "server", "printer", "serial"]);
}
