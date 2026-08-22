use std::{fs, process::Command};

#[test]
fn binary_config_error_never_echoes_the_source_or_auth_token() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must be available: {error}"));
    let path = directory.path().join("config.toml");
    let secret = "0123456789abcdef0123456789abcdef";
    fs::write(
        &path,
        format!("[security]\nauth_token = {secret}\ninvalid = [\n"),
    )
    .unwrap_or_else(|error| panic!("fixture config must be written: {error}"));

    let output = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args([
            "--config",
            path.to_string_lossy().as_ref(),
            "config",
            "print",
        ])
        .output()
        .unwrap_or_else(|error| panic!("agentdeck must run: {error}"));
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("invalid AgentDeck config at"));
    assert!(stderr.contains("line "));
    assert!(stderr.contains("column "));
    assert!(!stderr.contains(secret));
    assert!(!stderr.contains("auth_token"));
    assert!(!stderr.contains("invalid ="));
}

#[test]
fn binary_non_utf8_config_error_never_echoes_valid_prefixes_or_auth_token() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must be available: {error}"));
    let path = directory.path().join("config.toml");
    let secret = "fedcba9876543210fedcba9876543210";
    let mut contents = format!("[security]\nauth_token = '{secret}'\n").into_bytes();
    contents.extend_from_slice(&[0xff, 0xfe, b'\n']);
    fs::write(&path, contents)
        .unwrap_or_else(|error| panic!("fixture config must be written: {error}"));

    let output = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args([
            "--config",
            path.to_string_lossy().as_ref(),
            "config",
            "print",
        ])
        .output()
        .unwrap_or_else(|error| panic!("agentdeck must run: {error}"));
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("could not read AgentDeck config at"));
    assert!(!stderr.contains(secret));
    assert!(!stderr.contains("auth_token"));
    assert!(!stderr.contains("[security]"));
}

#[test]
fn binary_semantic_url_errors_never_echo_rejected_values_or_secrets() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must be available: {error}"));
    let secret = "semantic-cli-secret-0123456789abcdef";
    let cases = [
        (
            "headings.toml",
            format!("[headings]\nendpoint = 'http://user:{secret}@127.0.0.1:11434'\n"),
            "headings.endpoint",
        ),
        (
            "origins.toml",
            format!(
                "[security]\nallowed_origins = ['https://deck.example.test/?token={secret}']\n"
            ),
            "security.allowed_origins",
        ),
    ];

    for (name, contents, field) in cases {
        let path = directory.path().join(name);
        fs::write(&path, &contents)
            .unwrap_or_else(|error| panic!("fixture config must be written: {error}"));
        let output = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args([
                "--config",
                path.to_string_lossy().as_ref(),
                "config",
                "print",
            ])
            .output()
            .unwrap_or_else(|error| panic!("agentdeck must run: {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success());
        assert!(stderr.contains(field));
        assert!(!stderr.contains(secret));
        assert!(!stderr.contains(&contents));
        assert!(!stderr.contains("user:"));
        assert!(!stderr.contains("token="));
    }
}
