use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use boannews_incident_brief::{crawler, discord, output, ranking, store::Store};
use chrono::{DateTime, FixedOffset, Timelike, Utc};
use clap::{Args, Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(version, about = "보안뉴스 침해사고 RSS 수집 및 일일 브리핑")]
struct Cli {
    /// SQLite와 브리핑 파일을 보관할 경로
    #[arg(long, global = true, default_value = "data")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// RSS를 한 번 수집합니다. 디스코드 전송은 하지 않습니다.
    Collect {
        /// 네트워크 대신 로컬 RSS XML을 읽습니다 (오프라인 검증용).
        #[arg(long)]
        fixture: Option<PathBuf>,
    },
    /// 저장된 기사에서 주요 침해사고를 선정하고 JSON/Markdown으로 저장합니다.
    Digest(DigestOptions),
    /// 전송 결과가 불명확해 보류 중인 브리핑을 표시합니다.
    DeliveryStatus,
    /// 디스코드 채널 확인 후 보류 중인 전송 결과를 수동 확정합니다.
    ResolveDelivery {
        #[arg(
            long,
            required_unless_present = "not_delivered",
            conflicts_with = "not_delivered"
        )]
        delivered: bool,
        #[arg(long, required_unless_present = "delivered")]
        not_delivered: bool,
    },
    /// 시작 즉시 수집하고, 매시간 수집 / 매일 18시(KST) 브리핑을 실행합니다.
    Run {
        /// 수집 및 브리핑을 한 번 실행하고 종료합니다.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(5..=1440))]
        interval_minutes: u64,
        #[arg(long, default_value_t = 18, value_parser = clap::value_parser!(u8).range(0..=23))]
        digest_hour: u8,
        #[command(flatten)]
        digest: DigestOptions,
    },
}

#[derive(Args)]
struct DigestOptions {
    /// 최근 N시간 이내 발행 기사만 선정합니다.
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(i64).range(1..=168))]
    hours: i64,
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=5))]
    limit: u8,
    /// DISCORD_WEBHOOK_URL 또는 DISCORD_WEBHOOK_URL_FILE의 웹훅으로 실제 전송합니다.
    #[arg(long)]
    send: bool,
}

#[derive(Serialize, Deserialize)]
struct PendingDelivery {
    article_ids: Vec<String>,
    attempted_at: DateTime<Utc>,
    #[serde(default)]
    scheduled_date: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Check requested delivery configuration before spending time on network work.
    match &cli.command {
        Command::Digest(options)
        | Command::Run {
            digest: options, ..
        } if options.send => {
            webhook_from_environment()?;
        }
        _ => {}
    }
    fs::create_dir_all(&cli.data_dir).context("Cannot create data directory")?;
    let _lock = acquire_lock(&cli.data_dir)?;
    let mut store = Store::open(&cli.data_dir.join("news.sqlite3"))?;
    match cli.command {
        Command::Collect { fixture } => collect(&mut store, fixture.as_deref()),
        Command::Digest(options) => digest(&mut store, &cli.data_dir, &options, None, None),
        Command::DeliveryStatus => {
            match store.get_metadata("pending_delivery")? {
                Some(pending) => println!("전송 결과 확인 필요: {pending}"),
                None => println!("보류 중인 전송이 없습니다."),
            }
            Ok(())
        }
        Command::ResolveDelivery { delivered, .. } => {
            if let Some(payload) = store.get_metadata("pending_delivery")? {
                let pending: PendingDelivery = serde_json::from_str(&payload)?;
                if delivered {
                    store.complete_delivery(
                        &pending.article_ids,
                        pending.attempted_at,
                        pending.scheduled_date.as_deref(),
                    )?;
                } else {
                    store.clear_pending_delivery()?;
                }
                println!("보류 상태를 해제했습니다. 전달 완료 여부: {delivered}");
            } else {
                println!("보류 중인 전송이 없습니다.");
            }
            Ok(())
        }
        Command::Run {
            once,
            interval_minutes,
            digest_hour,
            digest: options,
        } => {
            if once {
                collect(&mut store, None)?;
                return digest(&mut store, &cli.data_dir, &options, None, None);
            }
            run_loop(
                &mut store,
                &cli.data_dir,
                interval_minutes,
                digest_hour,
                &options,
            )
        }
    }
}

fn acquire_lock(directory: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("collector.lock"))?;
    file.try_lock_exclusive()
        .context("Another process is using this data directory; stop it first")?;
    Ok(file)
}

fn collect(store: &mut Store, fixture: Option<&Path>) -> Result<()> {
    let articles = match fixture {
        Some(path) => {
            if fs::metadata(path)?.len() > 2 * 1024 * 1024 {
                bail!("RSS fixture exceeds the 2 MiB limit");
            }
            let xml = fs::read_to_string(path).context("Cannot read RSS fixture")?;
            crawler::parse_feed(&xml, Utc::now())?
        }
        None => crawler::fetch_articles()?,
    };
    let count = articles.len();
    let new_count = store.upsert(&articles)?;
    println!(
        "수집 완료: {count}개 확인 / {new_count}개 신규 / {}개 갱신",
        count - new_count
    );
    Ok(())
}

fn webhook_from_environment() -> Result<String> {
    load_webhook(
        std::env::var_os("DISCORD_WEBHOOK_URL"),
        std::env::var_os("DISCORD_WEBHOOK_URL_FILE"),
    )
}

fn load_webhook(value: Option<OsString>, file: Option<OsString>) -> Result<String> {
    let value = match (value, file) {
        (Some(_), Some(_)) => {
            bail!("Set only one of DISCORD_WEBHOOK_URL or DISCORD_WEBHOOK_URL_FILE");
        }
        (Some(value), None) => value
            .into_string()
            .map_err(|_| anyhow::anyhow!("DISCORD_WEBHOOK_URL must be valid Unicode"))?,
        (None, Some(path)) => {
            if path.is_empty() {
                bail!("DISCORD_WEBHOOK_URL_FILE must not be empty");
            }
            // Bound secret reads, and never include the path or content in errors.
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| file.take(4097).read_to_end(&mut bytes))
                .map_err(|_| anyhow::anyhow!("Cannot read DISCORD_WEBHOOK_URL_FILE"))?;
            if bytes.len() > 4096 {
                bail!("DISCORD_WEBHOOK_URL_FILE exceeds 4096 bytes");
            }
            String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("DISCORD_WEBHOOK_URL_FILE must be UTF-8"))?
                .trim_end_matches(['\r', '\n'])
                .to_owned()
        }
        (None, None) => {
            bail!("--send requires DISCORD_WEBHOOK_URL or DISCORD_WEBHOOK_URL_FILE");
        }
    };
    if value.trim().is_empty() {
        bail!("Discord webhook must not be empty");
    }
    discord::validate_webhook_url(&value)?;
    Ok(value)
}

fn ensure_running(running: Option<&AtomicBool>) -> Result<()> {
    if running.is_some_and(|flag| !flag.load(Ordering::SeqCst)) {
        bail!("Shutdown requested; briefing cancelled");
    }
    Ok(())
}

fn digest(
    store: &mut Store,
    data_dir: &Path,
    options: &DigestOptions,
    scheduled_date: Option<&str>,
    running: Option<&AtomicBool>,
) -> Result<()> {
    ensure_running(running)?;
    if options.send && store.get_metadata("pending_delivery")?.is_some() {
        bail!(
            "Previous delivery is pending/uncertain. Check Discord and use delivery-status, then resolve-delivery --delivered or --not-delivered before sending again."
        );
    }
    let now = Utc::now();
    let excluded = if options.send {
        store.sent_ids()?
    } else {
        HashSet::new()
    };
    let selected = ranking::rank_articles(
        &store.articles()?,
        now,
        options.hours,
        usize::from(options.limit),
        &excluded,
    );
    let (json_path, markdown_path) =
        output::write_digest(&data_dir.join("digests"), &selected, now)?;
    println!(
        "최근 {}시간 침해사고 {}개 선정",
        options.hours,
        selected.len()
    );
    println!("JSON: {}", json_path.display());
    println!("Markdown: {}", markdown_path.display());
    if options.send && !selected.is_empty() {
        let ids = selected
            .iter()
            .map(|item| item.article.id.clone())
            .collect::<Vec<_>>();
        let pending = PendingDelivery {
            article_ids: ids.clone(),
            attempted_at: now,
            scheduled_date: scheduled_date.map(str::to_owned),
        };
        let webhook = webhook_from_environment()?;
        ensure_running(running)?;
        store.set_metadata("pending_delivery", &serde_json::to_string(&pending)?)?;
        if let Err(error) = ensure_running(running) {
            store.clear_pending_delivery()?;
            return Err(error);
        }
        discord::send_digest(&webhook, &selected, now).context(
            "Delivery was not confirmed; automatic resend is paused until resolve-delivery",
        )?;
        store.complete_delivery(&ids, now, scheduled_date)?;
        println!("디스코드 전송 완료: {}개", selected.len());
    } else if options.send {
        println!("새로 전달할 침해사고가 없어 전송하지 않았습니다.");
    } else {
        println!("미리보기 저장 완료. 실제 전송은 --send 옵션을 사용하세요.");
    }
    if (!options.send || selected.is_empty())
        && let Some(date) = scheduled_date
    {
        ensure_running(running)?;
        let marker = if options.send {
            "last_sent_daily_date"
        } else {
            "last_preview_daily_date"
        };
        store.set_metadata(marker, date)?;
    }
    Ok(())
}

fn daily_key(now: DateTime<Utc>, hour: u8, last_date: Option<&str>) -> Option<String> {
    let local = now.with_timezone(&FixedOffset::east_opt(9 * 3600).unwrap());
    let today = local.format("%Y-%m-%d").to_string();
    (local.hour() >= u32::from(hour) && last_date != Some(today.as_str())).then_some(today)
}

fn collection_due(
    now: Instant,
    next_collect: Instant,
    daily_due: bool,
    collection_ready: bool,
) -> bool {
    now >= next_collect || (daily_due && collection_ready)
}

fn run_loop(
    store: &mut Store,
    data_dir: &Path,
    interval_minutes: u64,
    hour: u8,
    options: &DigestOptions,
) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let stop_flag = Arc::clone(&running);
    ctrlc::set_handler(move || {
        stop_flag.store(false, Ordering::SeqCst);
    })?;
    let interval = Duration::from_secs(interval_minutes * 60);
    let mut next_collect = Instant::now();
    let mut next_digest_attempt = Instant::now();
    let mut collection_ready = false;
    let marker = if options.send {
        "last_sent_daily_date"
    } else {
        "last_preview_daily_date"
    };
    println!(
        "실행 중: {interval_minutes}분마다 수집, 매일 {hour:02}:00 KST 브리핑. Ctrl+C 또는 SIGTERM으로 종료."
    );
    while running.load(Ordering::SeqCst) {
        let previous = store.get_metadata(marker)?;
        let daily_due = Instant::now() >= next_digest_attempt
            && daily_key(Utc::now(), hour, previous.as_deref()).is_some();
        // Always refresh at the daily deadline, even between regular polling
        // ticks. A previous collection failure still preserves server cooldowns.
        if collection_due(Instant::now(), next_collect, daily_due, collection_ready) {
            match collect(store, None) {
                Ok(()) => {
                    collection_ready = true;
                    next_collect = Instant::now() + interval;
                }
                Err(error) => {
                    collection_ready = false;
                    let delay = error
                        .downcast_ref::<crawler::CollectionDeferred>()
                        .map(|deferred| deferred.retry_after)
                        .unwrap_or(Duration::from_secs(300))
                        .max(Duration::from_secs(300));
                    eprintln!("수집 실패, {}초 후 재시도: {error:#}", delay.as_secs());
                    next_collect = Instant::now().checked_add(delay).context(
                        "Server requested an unrepresentable cooldown; stopping collection",
                    )?;
                }
            }
        }
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let previous = store.get_metadata(marker)?;
        if collection_ready
            && Instant::now() >= next_digest_attempt
            && let Some(today) = daily_key(Utc::now(), hour, previous.as_deref())
        {
            match digest(store, data_dir, options, Some(&today), Some(&running)) {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("브리핑 실패, 5분 후 재시도: {error:#}");
                    next_digest_attempt = Instant::now() + Duration::from_secs(300);
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    println!("종료했습니다.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn schedule_uses_korea_time_and_persisted_date() {
        let before = Utc.with_ymd_and_hms(2026, 11, 1, 8, 59, 0).unwrap();
        let due = Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap();
        assert_eq!(daily_key(before, 18, None), None);
        assert_eq!(daily_key(due, 18, None), Some("2026-11-01".into()));
        assert_eq!(daily_key(due, 18, Some("2026-11-01")), None);
        assert_eq!(
            daily_key(due, 18, Some("2026-10-31")),
            Some("2026-11-01".into())
        );
    }

    #[test]
    fn same_database_cannot_be_used_by_two_processes() {
        let directory = tempfile::tempdir().unwrap();
        let guard = acquire_lock(directory.path()).unwrap();
        assert!(acquire_lock(directory.path()).is_err());
        drop(guard);
        assert!(acquire_lock(directory.path()).is_ok());
    }

    #[test]
    fn daily_deadline_refreshes_between_polls_and_preserves_failure_cooldown() {
        let now = Instant::now();
        let next_poll = now + Duration::from_secs(1800);
        // At 18:00, a 17:30 successful poll must be refreshed before briefing.
        assert!(collection_due(now, next_poll, true, true));
        assert!(!collection_due(now, next_poll, false, true));
        // A failure/Retry-After cannot be bypassed by an overdue briefing.
        assert!(!collection_due(now, next_poll, true, false));
        assert!(collection_due(next_poll, next_poll, true, false));
    }

    #[test]
    fn cancellation_prevents_report_delivery_and_daily_completion() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let running = AtomicBool::new(false);
        let options = DigestOptions {
            hours: 24,
            limit: 5,
            send: true,
        };
        let error = digest(
            &mut store,
            directory.path(),
            &options,
            Some("2026-11-01"),
            Some(&running),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Shutdown requested"));
        assert!(store.sent_ids().unwrap().is_empty());
        assert!(store.get_metadata("pending_delivery").unwrap().is_none());
        assert!(
            store
                .get_metadata("last_sent_daily_date")
                .unwrap()
                .is_none()
        );
        assert!(!directory.path().join("digests").exists());
    }

    #[test]
    fn cli_rejects_unbounded_or_invalid_work() {
        assert!(Cli::try_parse_from(["app", "digest", "--hours", "0"]).is_err());
        assert!(Cli::try_parse_from(["app", "digest", "--limit", "100"]).is_err());
        assert!(Cli::try_parse_from(["app", "run", "--interval-minutes", "0"]).is_err());
        assert!(Cli::try_parse_from(["app", "run", "--digest-hour", "24"]).is_err());
        assert!(Cli::try_parse_from(["app", "resolve-delivery"]).is_err());
        assert!(
            Cli::try_parse_from(["app", "resolve-delivery", "--delivered", "--not-delivered"])
                .is_err()
        );
        let parsed = Cli::try_parse_from(["app", "run", "--once"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Run {
                digest: DigestOptions { send: false, .. },
                ..
            }
        ));
    }
}
