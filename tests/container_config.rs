//! Secret configuration checks: empty databases guarantee no Discord POST.
use std::{fs, path::Path, process::Command};

fn command(data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boannews-incident-brief"));
    command
        .arg("--data-dir")
        .arg(data)
        .args(["digest", "--send"])
        .env_remove("DISCORD_WEBHOOK_URL")
        .env_remove("DISCORD_WEBHOOK_URL_FILE");
    command
}

#[test]
fn secret_file_accepts_lf_and_crlf_without_sending_empty_digests() {
    let directory = tempfile::tempdir().unwrap();
    for (index, ending) in ["", "\n", "\r\n"].into_iter().enumerate() {
        let file = directory.path().join(format!("secret-{index}"));
        fs::write(
            &file,
            format!("https://discord.com/api/webhooks/123/FAKE_TEST_ONLY{ending}"),
        )
        .unwrap();
        let output = command(&directory.path().join(format!("data-{index}")))
            .env("DISCORD_WEBHOOK_URL_FILE", &file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("전송하지 않았습니다"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("FAKE_TEST_ONLY"));
    }
}

#[test]
fn direct_environment_variable_remains_supported() {
    let directory = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .env(
            "DISCORD_WEBHOOK_URL",
            "https://discord.com/api/webhooks/123/FAKE_TEST_ONLY",
        )
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn ambiguous_secret_configuration_is_rejected_before_database_creation() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let output = command(&data)
        .env("DISCORD_WEBHOOK_URL", "FAKE_TEST_ONLY")
        .env("DISCORD_WEBHOOK_URL_FILE", "FAKE_SECRET_PATH")
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("Set only one"));
    assert!(!error.contains("FAKE_TEST_ONLY"));
    assert!(!error.contains("FAKE_SECRET_PATH"));
    assert!(!data.exists());
}

#[test]
fn malformed_empty_and_oversized_secrets_fail_without_leaking_content() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("FAKE_SECRET_PATH");
    for bytes in [
        Vec::new(),
        b"\r\n".to_vec(),
        b"https://attacker.test/FAKE_TEST_ONLY".to_vec(),
        b"https://discord.com/api/webhooks/123/FAKE_TEST_ONLY\nextra".to_vec(),
        vec![b'x'; 4097],
        vec![0xff],
    ] {
        fs::write(&file, bytes).unwrap();
        let output = command(&directory.path().join("data"))
            .env("DISCORD_WEBHOOK_URL_FILE", &file)
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(!error.contains("FAKE_TEST_ONLY"));
        assert!(!error.contains("FAKE_SECRET_PATH"));
        assert!(!directory.path().join("data").exists());
    }
}

#[test]
fn missing_secret_file_fails_without_leaking_its_path() {
    let directory = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .env(
            "DISCORD_WEBHOOK_URL_FILE",
            directory.path().join("FAKE_SECRET_PATH"),
        )
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("Cannot read"));
    assert!(!error.contains("FAKE_SECRET_PATH"));
}

#[test]
fn missing_secret_configuration_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = command(directory.path()).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--send requires"));
}
