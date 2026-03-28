use std::fs;
use std::io::Read;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use csv::Writer;
use directories::ProjectDirs;
use indicatif::{ProgressBar, ProgressStyle};
use rand::RngCore;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tabled::{Table, Tabled};
use tokio::task::JoinSet;

#[derive(Parser)]
#[command(name = "speed-test")]
#[command(version = "0.1.0")]
#[command(about = "家庭网络测速 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Test(TestArgs),
    History(HistoryArgs),
    Export(ExportArgs),
    Config(ConfigArgs),
    Servers(ServersArgs),
}

#[derive(Args, Clone)]
struct TestArgs {
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    duration: Option<u64>,
    #[arg(long)]
    concurrency: Option<usize>,
    #[arg(long)]
    timeout: Option<u64>,
    #[arg(long)]
    no_save: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    quiet: bool,
    #[arg(long, value_enum, default_value_t = TestUnit::All)]
    unit: TestUnit,
}

#[derive(Args)]
struct HistoryArgs {
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(ValueEnum, Clone)]
enum ExportFormat {
    Json,
    Csv,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum TestUnit {
    All,
    Latency,
    Download,
    Upload,
}

#[derive(Args)]
struct ExportArgs {
    #[arg(long, value_enum)]
    format: ExportFormat,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
}

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Subcommand)]
enum ConfigAction {
    Show,
    Get { key: String },
    Set { key: String, value: String },
}

#[derive(Args)]
struct ServersArgs {
    #[command(subcommand)]
    action: ServersAction,
}

#[derive(Subcommand)]
enum ServersAction {
    List,
    Probe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Server {
    id: String,
    name: String,
    download_url: String,
    upload_url: String,
    latency_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestConfig {
    duration_seconds: u64,
    concurrency: usize,
    timeout_seconds: u64,
    chunk_size_bytes: usize,
    max_download_bytes: u64,
    max_upload_bytes: u64,
    save_history: bool,
    latency_probe_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    version: u32,
    test: TestConfig,
    servers: Vec<Server>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            test: TestConfig {
                duration_seconds: 10,
                concurrency: 16,
                timeout_seconds: 15,
                chunk_size_bytes: 64 * 1024,
                max_download_bytes: 2_000_000_000,
                max_upload_bytes: 1_000_000_000,
                save_history: true,
                latency_probe_count: 12,
            },
            servers: vec![
                Server {
                    id: "zju-ipv4".to_string(),
                    name: "ZJU Speedtest IPv4".to_string(),
                    download_url: "http://speedtest.zju.edu.cn/garbage.php".to_string(),
                    upload_url: "http://speedtest.zju.edu.cn/empty.php".to_string(),
                    latency_url: "http://speedtest.zju.edu.cn/empty.php".to_string(),
                },
                Server {
                    id: "zju-ipv6".to_string(),
                    name: "ZJU Speedtest IPv6".to_string(),
                    download_url: "http://speedtest.zju6.edu.cn/garbage.php".to_string(),
                    upload_url: "http://speedtest.zju6.edu.cn/empty.php".to_string(),
                    latency_url: "http://speedtest.zju6.edu.cn/empty.php".to_string(),
                },
                Server {
                    id: "cloudflare".to_string(),
                    name: "Cloudflare".to_string(),
                    download_url: "https://speed.cloudflare.com/__down?bytes=25000000".to_string(),
                    upload_url: "https://speed.cloudflare.com/__up".to_string(),
                    latency_url: "https://speed.cloudflare.com/__down?bytes=1000".to_string(),
                },
                Server {
                    id: "fsn1-hetzner".to_string(),
                    name: "Hetzner FSN1".to_string(),
                    download_url: "https://speed.hetzner.de/100MB.bin".to_string(),
                    upload_url: "https://httpbin.org/post".to_string(),
                    latency_url: "https://httpbin.org/get".to_string(),
                },
            ],
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TestResult {
    id: Option<i64>,
    started_at: DateTime<Utc>,
    duration_seconds: u64,
    server_id: String,
    server_name: String,
    server_download_url: String,
    server_upload_url: String,
    server_latency_url: String,
    latency_ms: f64,
    jitter_ms: f64,
    download_mbps: f64,
    upload_mbps: f64,
    download_bytes: u64,
    upload_bytes: u64,
    error: Option<String>,
    app_version: String,
}

#[derive(Tabled)]
struct ResultRow {
    指标: String,
    数值: String,
    单位: String,
}

#[derive(Clone)]
struct RuntimeOptions {
    duration_seconds: u64,
    concurrency: usize,
    timeout_seconds: u64,
    no_save: bool,
    json: bool,
    quiet: bool,
    unit: TestUnit,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::new()?;
    paths.ensure_dirs()?;
    let mut config = load_or_init_config(&paths)?;
    let db = open_db(&paths.db_path)?;
    migrate(&db)?;
    match cli.command.unwrap_or(Command::Test(TestArgs {
        server: None,
        duration: None,
        concurrency: None,
        timeout: None,
        no_save: false,
        json: false,
        quiet: false,
        unit: TestUnit::All,
    })) {
        Command::Test(args) => run_test(&db, &config, args).await?,
        Command::History(args) => run_history(&db, args)?,
        Command::Export(args) => run_export(&db, args)?,
        Command::Config(args) => run_config(&paths, &mut config, args)?,
        Command::Servers(args) => run_servers(&config, args).await?,
    }
    Ok(())
}

struct AppPaths {
    config_path: PathBuf,
    db_path: PathBuf,
}

impl AppPaths {
    fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "speed-cli", "speed-test")
            .ok_or_else(|| anyhow!("无法确定配置目录"))?;
        let config_dir = dirs.config_dir().to_path_buf();
        let data_dir = dirs.data_dir().to_path_buf();
        Ok(Self {
            config_path: config_dir.join("config.toml"),
            db_path: data_dir.join("speed-test.sqlite"),
        })
    }

    fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

fn load_or_init_config(paths: &AppPaths) -> Result<AppConfig> {
    if paths.config_path.exists() {
        let text = fs::read_to_string(&paths.config_path).context("读取配置文件失败")?;
        let mut cfg = toml::from_str::<AppConfig>(&text).context("解析配置文件失败")?;
        let mut changed = false;
        if merge_missing_default_servers(&mut cfg) {
            changed = true;
        }
        if upgrade_legacy_test_defaults(&mut cfg) {
            changed = true;
        }
        if changed {
            save_config(&paths.config_path, &cfg).context("写回补齐后的配置文件失败")?;
        }
        return Ok(cfg);
    }
    let cfg = AppConfig::default();
    let text = toml::to_string_pretty(&cfg)?;
    fs::write(&paths.config_path, text)?;
    Ok(cfg)
}

fn save_config(path: &Path, cfg: &AppConfig) -> Result<()> {
    let text = toml::to_string_pretty(cfg)?;
    fs::write(path, text)?;
    Ok(())
}

fn merge_missing_default_servers(cfg: &mut AppConfig) -> bool {
    let mut changed = false;
    for server in AppConfig::default().servers {
        if cfg.servers.iter().any(|s| s.id == server.id) {
            continue;
        }
        cfg.servers.push(server);
        changed = true;
    }
    changed
}

fn upgrade_legacy_test_defaults(cfg: &mut AppConfig) -> bool {
    let mut changed = false;
    if cfg.test.concurrency == 4 || cfg.test.concurrency == 8 {
        cfg.test.concurrency = 16;
        changed = true;
    }
    changed
}

fn open_db(path: &Path) -> Result<Connection> {
    Connection::open(path).context("打开数据库失败")
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS test_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at TEXT NOT NULL,
            duration_seconds INTEGER NOT NULL,
            server_id TEXT NOT NULL,
            server_name TEXT NOT NULL,
            server_download_url TEXT NOT NULL,
            server_upload_url TEXT NOT NULL,
            server_latency_url TEXT NOT NULL,
            latency_ms REAL NOT NULL,
            jitter_ms REAL NOT NULL,
            download_mbps REAL NOT NULL,
            upload_mbps REAL NOT NULL,
            download_bytes INTEGER NOT NULL,
            upload_bytes INTEGER NOT NULL,
            error TEXT NULL,
            app_version TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_test_runs_started_at ON test_runs(started_at);
    "#,
    )?;
    Ok(())
}

fn create_phase_bar(label: &str) -> ProgressBar {
    let bar = ProgressBar::new(100);
    bar.set_style(
        ProgressStyle::with_template("{msg} [{bar:24.cyan/blue}] {pos:>3}%")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("█░ "),
    );
    bar.set_message(label.to_string());
    bar
}

fn paint(text: impl Into<String>, color_code: &str) -> String {
    format!("\x1b[{color_code}m{}\x1b[0m", text.into())
}

fn color_speed(mbps: f64) -> String {
    if mbps >= 100.0 {
        paint(format!("{mbps:.2} Mbps"), "32")
    } else if mbps >= 30.0 {
        paint(format!("{mbps:.2} Mbps"), "33")
    } else {
        paint(format!("{mbps:.2} Mbps"), "31")
    }
}

fn color_latency(ms: f64) -> String {
    if ms <= 20.0 {
        paint(format!("{ms:.2} ms"), "32")
    } else if ms <= 50.0 {
        paint(format!("{ms:.2} ms"), "33")
    } else {
        paint(format!("{ms:.2} ms"), "31")
    }
}

fn color_jitter(ms: f64) -> String {
    if ms <= 5.0 {
        paint(format!("{ms:.2} ms"), "32")
    } else if ms <= 15.0 {
        paint(format!("{ms:.2} ms"), "33")
    } else {
        paint(format!("{ms:.2} ms"), "31")
    }
}

fn compute_dynamic_cap(base_cap_bytes: u64, duration_seconds: u64, floor_mbps: u64) -> u64 {
    let floor_bytes = duration_seconds
        .saturating_mul(floor_mbps)
        .saturating_mul(1_000_000)
        / 8;
    base_cap_bytes.max(floor_bytes)
}

async fn run_test(conn: &Connection, cfg: &AppConfig, args: TestArgs) -> Result<()> {
    let options = RuntimeOptions {
        duration_seconds: args.duration.unwrap_or(cfg.test.duration_seconds),
        concurrency: args.concurrency.unwrap_or(cfg.test.concurrency),
        timeout_seconds: args.timeout.unwrap_or(cfg.test.timeout_seconds),
        no_save: args.no_save || !cfg.test.save_history || args.unit != TestUnit::All,
        json: args.json,
        quiet: args.quiet,
        unit: args.unit,
    };
    let effective_download_cap =
        compute_dynamic_cap(cfg.test.max_download_bytes, options.duration_seconds, 1_000);
    let effective_upload_cap =
        compute_dynamic_cap(cfg.test.max_upload_bytes, options.duration_seconds, 600);
    if cfg.servers.is_empty() {
        return Err(anyhow!("服务器列表为空，请先配置 servers"));
    }
    let interactive = !options.quiet && !options.json;
    if interactive {
        println!("🏠 家庭网络速度测试 v{}", env!("CARGO_PKG_VERSION"));
        println!("=====================================");
    }
    let spinner = if interactive {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        pb.enable_steady_tick(Duration::from_millis(120));
        pb.set_message("正在探测可用服务器");
        pb
    } else {
        ProgressBar::hidden()
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(options.timeout_seconds))
        .build()?;
    let (selected, selected_probe_latency_ms) = select_server(&client, cfg, args.server.as_deref()).await?;
    spinner.finish_and_clear();
    if interactive {
        match selected_probe_latency_ms {
            Some(v) => println!("📍 测试服务器: {} (探测延迟: {:.2}ms)", selected.name, v),
            None => println!("📍 测试服务器: {}", selected.name),
        }
        println!();
    }
    let started_at = Utc::now();
    let (latency_ms, jitter_ms) =
        if options.unit == TestUnit::All || options.unit == TestUnit::Latency {
            if interactive {
                let pb = create_phase_bar("⏳ 正在测试延迟...");
                let measured = measure_latency(
                    &client,
                    &selected.latency_url,
                    cfg.test.latency_probe_count.min(6),
                    options.timeout_seconds,
                    Some(pb.clone()),
                )
                .await?;
                pb.set_position(100);
                pb.finish_with_message(format!(
                    "✅ 延迟: {}  抖动: {}",
                    color_latency(measured.0),
                    color_jitter(measured.1)
                ));
                println!();
                measured
            } else {
                measure_latency(
                    &client,
                    &selected.latency_url,
                    cfg.test.latency_probe_count.min(6),
                    options.timeout_seconds,
                    None,
                )
                .await?
            }
        } else {
            (0.0, 0.0)
        };
    let (download_mbps, download_bytes) =
        if options.unit == TestUnit::All || options.unit == TestUnit::Download {
            if interactive {
                let pb = create_phase_bar("⏳ 正在测试下载速度...");
                let client = client.clone();
                let url = selected.download_url.clone();
                let duration_seconds = options.duration_seconds;
                let concurrency = options.concurrency;
                let max_download_bytes = effective_download_cap;
                let task = tokio::spawn(async move {
                    measure_download(
                        &client,
                        &url,
                        duration_seconds,
                        concurrency,
                        max_download_bytes,
                    )
                    .await
                });
                let phase_start = Instant::now();
                while !task.is_finished() {
                    let pct = ((phase_start.elapsed().as_secs_f64()
                        / options.duration_seconds.max(1) as f64)
                        * 100.0)
                        .clamp(0.0, 99.0) as u64;
                    pb.set_position(pct);
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
                let measured = task.await.map_err(|e| anyhow!("下载任务失败: {e}"))??;
                pb.set_position(100);
                pb.finish_with_message(format!("✅ 下载速度: {}", color_speed(measured.0)));
                println!();
                measured
            } else {
                measure_download(
                    &client,
                    &selected.download_url,
                    options.duration_seconds,
                    options.concurrency,
                    effective_download_cap,
                )
                .await?
            }
        } else {
            (0.0, 0)
        };
    let (upload_mbps, upload_bytes) =
        if options.unit == TestUnit::All || options.unit == TestUnit::Upload {
            if interactive {
                let pb = create_phase_bar("⏳ 正在测试上传速度...");
                let client = client.clone();
                let url = selected.upload_url.clone();
                let duration_seconds = options.duration_seconds;
                let concurrency = options.concurrency;
                let max_upload_bytes = effective_upload_cap;
                let chunk_size_bytes = cfg.test.chunk_size_bytes;
                let task = tokio::spawn(async move {
                    measure_upload(
                        &client,
                        &url,
                        duration_seconds,
                        concurrency,
                        max_upload_bytes,
                        chunk_size_bytes,
                    )
                    .await
                });
                let phase_start = Instant::now();
                while !task.is_finished() {
                    let pct = ((phase_start.elapsed().as_secs_f64()
                        / options.duration_seconds.max(1) as f64)
                        * 100.0)
                        .clamp(0.0, 99.0) as u64;
                    pb.set_position(pct);
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
                let measured = task.await.map_err(|e| anyhow!("上传任务失败: {e}"))??;
                pb.set_position(100);
                pb.finish_with_message(format!("✅ 上传速度: {}", color_speed(measured.0)));
                println!();
                measured
            } else {
                measure_upload(
                    &client,
                    &selected.upload_url,
                    options.duration_seconds,
                    options.concurrency,
                    effective_upload_cap,
                    cfg.test.chunk_size_bytes,
                )
                .await?
            }
        } else {
            (0.0, 0)
        };
    if interactive {
        println!("=====================================");
    }
    let result = TestResult {
        id: None,
        started_at,
        duration_seconds: options.duration_seconds,
        server_id: selected.id.clone(),
        server_name: selected.name.clone(),
        server_download_url: selected.download_url.clone(),
        server_upload_url: selected.upload_url.clone(),
        server_latency_url: selected.latency_url.clone(),
        latency_ms,
        jitter_ms,
        download_mbps,
        upload_mbps,
        download_bytes,
        upload_bytes,
        error: None,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if options.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if options.quiet {
        match options.unit {
            TestUnit::All => println!(
                "download_mbps={:.2} upload_mbps={:.2} latency_ms={:.2}",
                result.download_mbps, result.upload_mbps, result.latency_ms
            ),
            TestUnit::Latency => println!(
                "latency_ms={:.2} jitter_ms={:.2}",
                result.latency_ms, result.jitter_ms
            ),
            TestUnit::Download => println!(
                "download_mbps={:.2} download_bytes={}",
                result.download_mbps, result.download_bytes
            ),
            TestUnit::Upload => println!(
                "upload_mbps={:.2} upload_bytes={}",
                result.upload_mbps, result.upload_bytes
            ),
        }
    } else {
        print_test_result(&result, options.unit);
    }
    if !options.no_save {
        insert_result(conn, &result)?;
        if !options.quiet {
            println!("测试结果已保存");
        }
    }
    Ok(())
}

fn print_test_result(result: &TestResult, unit: TestUnit) {
    println!("🏠 家庭网络速度测试 v{}", env!("CARGO_PKG_VERSION"));
    println!("📍 测试服务器: {}", result.server_name);
    let mut rows = Vec::new();
    if unit == TestUnit::All || unit == TestUnit::Latency {
        rows.push(ResultRow {
            指标: "延迟".to_string(),
            数值: format!("{:.2}", result.latency_ms),
            单位: "ms".to_string(),
        });
        rows.push(ResultRow {
            指标: "抖动".to_string(),
            数值: format!("{:.2}", result.jitter_ms),
            单位: "ms".to_string(),
        });
    }
    if unit == TestUnit::All || unit == TestUnit::Download {
        rows.push(ResultRow {
            指标: "下载速度".to_string(),
            数值: format!("{:.2}", result.download_mbps),
            单位: "Mbps".to_string(),
        });
    }
    if unit == TestUnit::All || unit == TestUnit::Upload {
        rows.push(ResultRow {
            指标: "上传速度".to_string(),
            数值: format!("{:.2}", result.upload_mbps),
            单位: "Mbps".to_string(),
        });
    }
    let table = Table::new(rows);
    println!("{table}");
}

async fn run_servers(cfg: &AppConfig, args: ServersArgs) -> Result<()> {
    match args.action {
        ServersAction::List => {
            if cfg.servers.is_empty() {
                println!("没有可用服务器");
                return Ok(());
            }
            #[derive(Tabled)]
            struct ServerRow {
                id: String,
                name: String,
                latency_url: String,
            }
            let rows = cfg
                .servers
                .iter()
                .map(|s| ServerRow {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    latency_url: s.latency_url.clone(),
                })
                .collect::<Vec<_>>();
            println!("{}", Table::new(rows));
        }
        ServersAction::Probe => {
            let client = Client::builder()
                .timeout(Duration::from_secs(cfg.test.timeout_seconds))
                .build()?;
            let mut report = Vec::new();
            for server in &cfg.servers {
                let probed = measure_latency(
                    &client,
                    &server.latency_url,
                    cfg.test.latency_probe_count.min(6),
                    cfg.test.timeout_seconds,
                    None,
                )
                .await;
                match probed {
                    Ok((latency, jitter)) => report.push((
                        server.name.clone(),
                        format!("{latency:.2}"),
                        format!("{jitter:.2}"),
                        "ok".to_string(),
                    )),
                    Err(e) => report.push((
                        server.name.clone(),
                        "-".to_string(),
                        "-".to_string(),
                        e.to_string(),
                    )),
                }
            }
            #[derive(Tabled)]
            struct ProbeRow {
                server: String,
                latency_ms: String,
                jitter_ms: String,
                status: String,
            }
            let rows = report
                .into_iter()
                .map(|r| ProbeRow {
                    server: r.0,
                    latency_ms: r.1,
                    jitter_ms: r.2,
                    status: r.3,
                })
                .collect::<Vec<_>>();
            println!("{}", Table::new(rows));
        }
    }
    Ok(())
}

fn run_config(paths: &AppPaths, cfg: &mut AppConfig, args: ConfigArgs) -> Result<()> {
    match args.action {
        ConfigAction::Show => {
            println!("{}", toml::to_string_pretty(cfg)?);
        }
        ConfigAction::Get { key } => {
            let value = get_config_value(cfg, &key).ok_or_else(|| anyhow!("未知配置项: {key}"))?;
            println!("{value}");
        }
        ConfigAction::Set { key, value } => {
            set_config_value(cfg, &key, &value)?;
            save_config(&paths.config_path, cfg)?;
            println!("已更新: {key}={value}");
        }
    }
    Ok(())
}

fn get_config_value(cfg: &AppConfig, key: &str) -> Option<String> {
    match key {
        "test.duration_seconds" => Some(cfg.test.duration_seconds.to_string()),
        "test.concurrency" => Some(cfg.test.concurrency.to_string()),
        "test.timeout_seconds" => Some(cfg.test.timeout_seconds.to_string()),
        "test.chunk_size_bytes" => Some(cfg.test.chunk_size_bytes.to_string()),
        "test.max_download_bytes" => Some(cfg.test.max_download_bytes.to_string()),
        "test.max_upload_bytes" => Some(cfg.test.max_upload_bytes.to_string()),
        "test.save_history" => Some(cfg.test.save_history.to_string()),
        "test.latency_probe_count" => Some(cfg.test.latency_probe_count.to_string()),
        _ => None,
    }
}

fn set_config_value(cfg: &mut AppConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "test.duration_seconds" => cfg.test.duration_seconds = value.parse()?,
        "test.concurrency" => cfg.test.concurrency = value.parse()?,
        "test.timeout_seconds" => cfg.test.timeout_seconds = value.parse()?,
        "test.chunk_size_bytes" => cfg.test.chunk_size_bytes = value.parse()?,
        "test.max_download_bytes" => cfg.test.max_download_bytes = value.parse()?,
        "test.max_upload_bytes" => cfg.test.max_upload_bytes = value.parse()?,
        "test.save_history" => cfg.test.save_history = value.parse()?,
        "test.latency_probe_count" => cfg.test.latency_probe_count = value.parse()?,
        _ => return Err(anyhow!("未知配置项: {key}")),
    }
    Ok(())
}

fn run_history(conn: &Connection, args: HistoryArgs) -> Result<()> {
    let since = parse_time(args.since)?;
    let until = parse_time(args.until)?;
    let rows = query_results(conn, args.limit, since, until)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("暂无历史记录");
        return Ok(());
    }
    #[derive(Tabled)]
    struct HistoryRow {
        id: i64,
        started_at: String,
        server: String,
        latency_ms: String,
        download_mbps: String,
        upload_mbps: String,
    }
    let data = rows
        .into_iter()
        .filter_map(|r| {
            r.id.map(|id| HistoryRow {
                id,
                started_at: r.started_at.to_rfc3339(),
                server: r.server_name,
                latency_ms: format!("{:.2}", r.latency_ms),
                download_mbps: format!("{:.2}", r.download_mbps),
                upload_mbps: format!("{:.2}", r.upload_mbps),
            })
        })
        .collect::<Vec<_>>();
    println!("{}", Table::new(data));
    Ok(())
}

fn run_export(conn: &Connection, args: ExportArgs) -> Result<()> {
    let since = parse_time(args.since)?;
    let until = parse_time(args.until)?;
    let rows = query_results(conn, usize::MAX, since, until)?;
    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    match args.format {
        ExportFormat::Json => {
            let text = serde_json::to_string_pretty(&rows)?;
            fs::write(&args.out, text)?;
        }
        ExportFormat::Csv => {
            let mut writer = Writer::from_path(&args.out)?;
            writer.write_record([
                "id",
                "started_at",
                "duration_seconds",
                "server_id",
                "server_name",
                "latency_ms",
                "jitter_ms",
                "download_mbps",
                "upload_mbps",
                "download_bytes",
                "upload_bytes",
                "error",
                "app_version",
            ])?;
            for row in rows {
                writer.write_record([
                    row.id.unwrap_or_default().to_string(),
                    row.started_at.to_rfc3339(),
                    row.duration_seconds.to_string(),
                    row.server_id,
                    row.server_name,
                    format!("{:.2}", row.latency_ms),
                    format!("{:.2}", row.jitter_ms),
                    format!("{:.2}", row.download_mbps),
                    format!("{:.2}", row.upload_mbps),
                    row.download_bytes.to_string(),
                    row.upload_bytes.to_string(),
                    row.error.unwrap_or_default(),
                    row.app_version,
                ])?;
            }
            writer.flush()?;
        }
    }
    println!("导出完成: {}", args.out.display());
    Ok(())
}

fn parse_time(input: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match input {
        Some(v) => Ok(Some(DateTime::parse_from_rfc3339(&v)?.with_timezone(&Utc))),
        None => Ok(None),
    }
}

fn insert_result(conn: &Connection, result: &TestResult) -> Result<()> {
    conn.execute(
        "INSERT INTO test_runs (
            started_at, duration_seconds, server_id, server_name, server_download_url, server_upload_url, server_latency_url,
            latency_ms, jitter_ms, download_mbps, upload_mbps, download_bytes, upload_bytes, error, app_version
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            result.started_at.to_rfc3339(),
            result.duration_seconds as i64,
            result.server_id,
            result.server_name,
            result.server_download_url,
            result.server_upload_url,
            result.server_latency_url,
            result.latency_ms,
            result.jitter_ms,
            result.download_mbps,
            result.upload_mbps,
            result.download_bytes as i64,
            result.upload_bytes as i64,
            result.error,
            result.app_version
        ],
    )?;
    Ok(())
}

fn query_results(
    conn: &Connection,
    limit: usize,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Vec<TestResult>> {
    let mut sql = "SELECT id, started_at, duration_seconds, server_id, server_name, server_download_url, server_upload_url, server_latency_url,
        latency_ms, jitter_ms, download_mbps, upload_mbps, download_bytes, upload_bytes, error, app_version
        FROM test_runs WHERE 1=1"
        .to_string();
    let mut bind: Vec<String> = Vec::new();
    if let Some(v) = since {
        sql.push_str(" AND started_at >= ?");
        bind.push(v.to_rfc3339());
    }
    if let Some(v) = until {
        sql.push_str(" AND started_at <= ?");
        bind.push(v.to_rfc3339());
    }
    sql.push_str(" ORDER BY started_at DESC");
    if limit != usize::MAX {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(bind.iter()))?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let started_at_text: String = row.get(1)?;
        let started_at = DateTime::parse_from_rfc3339(&started_at_text)?.with_timezone(&Utc);
        result.push(TestResult {
            id: Some(row.get(0)?),
            started_at,
            duration_seconds: row.get::<_, i64>(2)? as u64,
            server_id: row.get(3)?,
            server_name: row.get(4)?,
            server_download_url: row.get(5)?,
            server_upload_url: row.get(6)?,
            server_latency_url: row.get(7)?,
            latency_ms: row.get(8)?,
            jitter_ms: row.get(9)?,
            download_mbps: row.get(10)?,
            upload_mbps: row.get(11)?,
            download_bytes: row.get::<_, i64>(12)? as u64,
            upload_bytes: row.get::<_, i64>(13)? as u64,
            error: row.get(14)?,
            app_version: row.get(15)?,
        });
    }
    Ok(result)
}

async fn select_server(
    client: &Client,
    cfg: &AppConfig,
    target: Option<&str>,
) -> Result<(Server, Option<f64>)> {
    if let Some(target) = target {
        if let Some(s) = cfg
            .servers
            .iter()
            .find(|s| s.id == target || s.name == target)
        {
            return Ok((s.clone(), None));
        }
        if target.starts_with("http://") || target.starts_with("https://") {
            return Ok((
                Server {
                    id: "custom".to_string(),
                    name: "Custom Server".to_string(),
                    download_url: target.to_string(),
                    upload_url: target.to_string(),
                    latency_url: target.to_string(),
                },
                None,
            ));
        }
        return Err(anyhow!("未找到服务器: {target}"));
    }
    let probe_timeout = cfg.test.timeout_seconds.min(3).max(1);
    let mut set = JoinSet::new();
    for server in &cfg.servers {
        let client = client.clone();
        let server = server.clone();
        set.spawn(async move {
            let latency = probe_latency_once(&client, &server.latency_url, probe_timeout).await?;
            Ok::<(f64, Server), anyhow::Error>((latency, server))
        });
    }
    let mut best: Option<(f64, Server)> = None;
    while let Some(joined) = set.join_next().await {
        if let Ok(Ok((latency, server))) = joined {
            match &best {
                Some((best_latency, _)) if latency >= *best_latency => {}
                _ => best = Some((latency, server)),
            }
        }
    }
    best.map(|x| (x.1, Some(x.0)))
        .ok_or_else(|| anyhow!("所有服务器探测失败，请检查网络或更新配置"))
}

async fn probe_latency_once(client: &Client, url: &str, timeout_secs: u64) -> Result<f64> {
    let now = Instant::now();
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("探测响应状态异常"));
    }
    Ok(now.elapsed().as_secs_f64() * 1000.0)
}

async fn measure_latency(
    client: &Client,
    url: &str,
    count: usize,
    timeout_secs: u64,
    progress: Option<ProgressBar>,
) -> Result<(f64, f64)> {
    let times = count.max(3);
    let mut samples = Vec::new();
    for i in 0..times {
        let now = Instant::now();
        let request = client.get(url).timeout(Duration::from_secs(timeout_secs));
        let resp = request.send().await;
        if let Ok(resp) = resp {
            if !resp.status().is_success() {
                continue;
            }
            samples.push(now.elapsed().as_secs_f64() * 1000.0);
        }
        let remaining = times - (i + 1);
        if let Some(progress) = &progress {
            let pct = (((i + 1) as f64 / times as f64) * 100.0).round() as u64;
            progress.set_position(pct.min(99));
        }
        if samples.len() + remaining < 3 {
            break;
        }
    }
    if samples.len() < 3 {
        return Err(anyhow!("延迟采样不足"));
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = samples[samples.len() / 2];
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance =
        samples.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / samples.len() as f64;
    let jitter = variance.sqrt();
    Ok((median, jitter))
}

async fn measure_download(
    client: &Client,
    url: &str,
    duration_seconds: u64,
    concurrency: usize,
    max_bytes: u64,
) -> Result<(f64, u64)> {
    let total = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let stop_at = start + Duration::from_secs(duration_seconds);
    let mut set = JoinSet::new();
    let workers = concurrency.max(1);
    for _ in 0..workers {
        let client = client.clone();
        let url = url.to_string();
        let total = total.clone();
        set.spawn(async move {
            while Instant::now() < stop_at && total.load(Ordering::Relaxed) < max_bytes {
                let response = client.get(&url).send().await;
                if let Ok(mut resp) = response {
                    loop {
                        if Instant::now() >= stop_at || total.load(Ordering::Relaxed) >= max_bytes {
                            break;
                        }
                        match resp.chunk().await {
                            Ok(Some(chunk)) => {
                                total.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                } else {
                    break;
                }
            }
        });
    }
    while set.join_next().await.is_some() {}
    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let bytes = total.load(Ordering::Relaxed);
    let mbps = (bytes as f64 * 8.0) / elapsed / 1_000_000.0;
    Ok((mbps, bytes))
}

async fn measure_upload(
    client: &Client,
    url: &str,
    duration_seconds: u64,
    concurrency: usize,
    max_bytes: u64,
    chunk_size: usize,
) -> Result<(f64, u64)> {
    let total = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let stop_at = start + Duration::from_secs(duration_seconds);
    let mut seed_data = vec![0_u8; chunk_size.max(1024)];
    rand::thread_rng().fill_bytes(&mut seed_data);
    let mut set = JoinSet::new();
    let workers = concurrency.max(1);
    for _ in 0..workers {
        let client = client.clone();
        let url = url.to_string();
        let total = total.clone();
        let body = seed_data.clone();
        set.spawn(async move {
            while Instant::now() < stop_at && total.load(Ordering::Relaxed) < max_bytes {
                let sent = body.len() as u64;
                let resp = client.post(&url).body(body.clone()).send().await;
                if let Ok(r) = resp {
                    if r.status().is_success() {
                        total.fetch_add(sent, Ordering::Relaxed);
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        });
    }
    while set.join_next().await.is_some() {}
    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let bytes = total.load(Ordering::Relaxed);
    let mbps = (bytes as f64 * 8.0) / elapsed / 1_000_000.0;
    Ok((mbps, bytes))
}

#[allow(dead_code)]
fn resolve_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    let addr = format!("{host}:{port}");
    let mut addrs = addr.to_socket_addrs().ok()?;
    addrs.next().map(|a| a.to_string())
}

#[allow(dead_code)]
fn read_all<R: Read>(reader: &mut R) -> Result<usize> {
    let mut total = 0usize;
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}
