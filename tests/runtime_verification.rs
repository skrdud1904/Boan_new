//! Explicit live read-only smoke test; never sends a Discord message.
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use chrono::{FixedOffset, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        // Only the child process started by this test is stopped.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(data: &Path, log: &Path, proxy: Option<&str>) -> Daemon {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boannews-incident-brief"));
    command
        .arg("--data-dir")
        .arg(data)
        .args(["run", "--digest-hour", "0", "--hours", "48"])
        .env_remove("DISCORD_WEBHOOK_URL")
        .env_remove("DISCORD_WEBHOOK_URL_FILE")
        .stdin(Stdio::null())
        .stdout(fs::File::create(log).unwrap())
        .stderr(fs::File::create(log.with_extension("stderr")).unwrap());
    if let Some(proxy) = proxy {
        command
            .env("HTTPS_PROXY", proxy)
            .env("HTTP_PROXY", proxy)
            .env("ALL_PROXY", proxy)
            .env_remove("NO_PROXY");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    Daemon(command.spawn().unwrap())
}

fn report_count(data: &Path) -> usize {
    fs::read_dir(data.join("digests"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count()
}

fn completed_today(data: &Path) -> bool {
    let Ok(connection) =
        Connection::open_with_flags(data.join("news.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    let date: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'last_preview_daily_date'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or_default();
    let today = Utc::now()
        .with_timezone(&FixedOffset::east_opt(9 * 3600).unwrap())
        .format("%Y-%m-%d")
        .to_string();
    date.as_deref() == Some(today.as_str())
}

fn wait_for_collection(daemon: &mut Daemon, data: &Path, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited unexpectedly"
        );
        let collected = fs::read_to_string(log)
            .unwrap_or_default()
            .contains("수집 완료");
        if collected && completed_today(data) && report_count(data) == 1 {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "daemon did not collect and persist a daily briefing within 40 seconds: {}",
        fs::read_to_string(log.with_extension("stderr")).unwrap_or_default()
    );
}

#[test]
#[ignore = "live Boannews RSS access; run explicitly with --ignored --nocapture"]
fn live_daemon_persists_daily_schedule_and_enforces_exclusive_access() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let first_log = directory.path().join("first.log");
    let mut first = start(&data, &first_log, None);
    wait_for_collection(&mut first, &data, &first_log);
    assert_eq!(report_count(&data), 1);

    let competing = Command::new(env!("CARGO_BIN_EXE_boannews-incident-brief"))
        .arg("--data-dir")
        .arg(&data)
        .arg("delivery-status")
        .env_remove("DISCORD_WEBHOOK_URL")
        .output()
        .unwrap();
    assert!(!competing.status.success());
    assert!(String::from_utf8_lossy(&competing.stderr).contains("Another process"));
    drop(first);

    let second_log = directory.path().join("restart.log");
    let mut restarted = start(&data, &second_log, None);
    wait_for_collection(&mut restarted, &data, &second_log);
    // Allow multiple scheduler ticks after restart; the daily marker must block it.
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(report_count(&data), 1);
    drop(restarted);
    let connection =
        Connection::open_with_flags(data.join("news.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let sent: i64 = connection
        .query_row("SELECT COUNT(*) FROM deliveries", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sent, 0);
    println!(
        "LIVE PASS: initial report=1, restarted report=1, competing process rejected, delivered rows=0"
    );
}

#[test]
fn failed_collection_cannot_complete_the_daily_briefing() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let log = directory.path().join("failed.log");
    let port_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", port_reservation.local_addr().unwrap());
    drop(port_reservation);
    // The child connects only to a closed loopback proxy, never to Boannews.
    let mut daemon = start(&data, &log, Some(&proxy));
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        assert!(daemon.0.try_wait().unwrap().is_none());
        if fs::read_to_string(log.with_extension("stderr"))
            .unwrap_or_default()
            .contains("수집 실패")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected controlled network failure was not reported"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(report_count(&data), 0);
    assert!(!completed_today(&data));
    let connection =
        Connection::open_with_flags(data.join("news.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let markers: i64 = connection
        .query_row("SELECT COUNT(*) FROM metadata", [], |row| row.get(0))
        .unwrap();
    assert_eq!(markers, 0);
    drop(connection);
    drop(daemon);
}

#[cfg(unix)]
#[test]
fn sigterm_stops_daemon_cleanly_and_releases_the_data_lock() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let log = directory.path().join("shutdown.log");
    let port_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", port_reservation.local_addr().unwrap());
    drop(port_reservation);
    let mut daemon = start(&data, &log, Some(&proxy));
    let deadline = Instant::now() + Duration::from_secs(12);
    // Wait until the handler is installed and bounded network retries finish.
    while !fs::read_to_string(log.with_extension("stderr"))
        .unwrap_or_default()
        .contains("수집 실패")
    {
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not reach idle state");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &daemon.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success(), "daemon did not exit gracefully: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "SIGTERM did not stop daemon");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(fs::read_to_string(&log).unwrap().contains("종료했습니다"));
    assert!(!completed_today(&data));
    let reopened = Command::new(env!("CARGO_BIN_EXE_boannews-incident-brief"))
        .arg("--data-dir")
        .arg(&data)
        .arg("delivery-status")
        .output()
        .unwrap();
    assert!(reopened.status.success());
}
