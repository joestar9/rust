use anyhow::Result;
use chrono::Local;
use crossterm::{
    style::{Color, Stylize},
    terminal, ExecutableCommand,
};
use dashmap::DashMap;
use futures::future::join_all;
use parking_lot::Mutex;
use rand::prelude::*;
use reqwest::{header, Client, Proxy, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::sleep;

// --- CONSTANTS ---
const API_CHECK: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_NOTIFY: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const APP_VERSION: &str = "9.62.0";
const PYTHON_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0";
const CONFIG_FILE: &str = "config.json";

// --- PROXY SETTINGS ---
const MASTER_PROXY_LIST_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const LOCAL_PROXY_FILE: &str = "socks5.txt";

const PREFIXES: [&str; 14] = [
    "0900", "0901", "0902", "0903", "0904", "0905", "0930", "0933", "0935", "0936", "0937", "0938",
    "0939", "0941",
];

#[derive(Deserialize)]
struct ConfigFile {
    token: String,
    cookie: String,
}

#[derive(Serialize)]
struct ReferralPayload<'a> {
    application_name: &'a str,
    friend_number: String,
}

struct GlobalStats {
    success: AtomicUsize,
    logic_fail: AtomicUsize,
    proxy_fail: AtomicUsize,
    net_fail: AtomicUsize,
    limit: usize,
    start_time: Instant,
    is_running: AtomicBool,
}

struct ProxyState {
    queue: VecDeque<String>,
    active: HashSet<String>,
    dead: HashSet<String>,
}

struct AppState {
    stats: Arc<GlobalStats>,
    proxy_state: Arc<Mutex<ProxyState>>,
    patterns: Arc<DashMap<String, u32>>,
    token: String,
    cookie: String,
    use_proxies: bool,
    base_headers: header::HeaderMap,
    log_tx: mpsc::Sender<LogEvent>,
}

enum LogEvent {
    Success { num: String, proxy: String },
    LogicFail { num: String, reason: String },
    TokenDead, 
}

// --- UI Helpers ---
fn input(prompt: &str) -> String {
    print!("{}{}", prompt.bold(), crossterm::style::Attribute::Reset);
    io::stdout().flush().unwrap_or_default();
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer).unwrap_or_default();
    buffer.trim().to_string()
}

fn load_config() -> (String, String) {
    if let Ok(content) = fs::read_to_string(CONFIG_FILE) {
        if let Ok(conf) = serde_json::from_str::<ConfigFile>(&content) {
            println!("{} Config loaded from {}!", ">>>".green(), CONFIG_FILE);
            return (conf.token, conf.cookie);
        }
    }
    let token = input("Enter Authorization Token (Bearer ...): ");
    let cookie = input("Enter Cookie (Optional): ");
    (token, cookie)
}

// --- MAIN ---
#[tokio::main]
async fn main() -> Result<()> {
    let mut stdout = io::stdout();
    let _ = stdout.execute(terminal::Clear(terminal::ClearType::All));
    let _ = stdout.execute(crossterm::cursor::MoveTo(0, 0));
    
    println!("{}", "╔═══════════════════════════════════════════════════════════════╗".cyan().bold());
    println!("{}", "║    IRANCELL BOT - MODULAR PROXY SYSTEM (ONLINE/LOCAL)         ║".cyan().bold());
    println!("{}", "╚═══════════════════════════════════════════════════════════════╝".cyan().bold());

    let (token, cookie) = load_config();
    if token.trim().is_empty() {
        println!("{}", "Error: Token is required.".red());
        return Ok(());
    }

    let limit: usize = input("Enter Limit (e.g., 20000): ").parse().unwrap_or(100000);
    let worker_count: usize = input("Managers (default 50): ").parse().unwrap_or(50);
    let concurrency: usize = input("Concurrency (default 5): ").parse().unwrap_or(5);
    let use_proxies = input("Use Proxies? (y/n): ").to_lowercase() == "y";

    let mut initial_queue = VecDeque::new();
    
    if use_proxies {
        println!("\n{}", "Select Proxy Source:".yellow().bold());
        println!("1. Online (Fetch links from GitHub Master List)");
        println!("2. Local File ({})", LOCAL_PROXY_FILE);
        let choice = input("Choice (1 or 2): ");

        let proxy_list = if choice == "2" {
            // Local
            load_local_proxies()
        } else {
            // Online (Default)
            let sources = fetch_master_sources().await;
            if sources.is_empty() {
                println!("{}", "Failed to fetch master list. Trying local as fallback...".red());
                load_local_proxies()
            } else {
                fetch_proxies_from_sources(sources).await
            }
        };

        if proxy_list.is_empty() {
            println!("{}", "Warning: No proxies found/loaded. Switching to DIRECT mode.".red());
        } else {
            initial_queue = proxy_list.into();
        }
    }

    // --- HEADERS ---
    let mut headers = header::HeaderMap::new();
    headers.insert("Authorization", header::HeaderValue::from_str(&token).unwrap_or(header::HeaderValue::from_static("")));
    if !cookie.trim().is_empty() {
        headers.insert("Cookie", header::HeaderValue::from_str(&cookie).unwrap_or(header::HeaderValue::from_static("")));
    }
    headers.insert("User-Agent", header::HeaderValue::from_static(PYTHON_USER_AGENT));
    headers.insert("Origin", header::HeaderValue::from_static("https://my.irancell.ir"));
    headers.insert("Content-Type", header::HeaderValue::from_static("application/json"));
    headers.insert("Accept", header::HeaderValue::from_static("application/json, text/plain, */*"));
    headers.insert("Accept-Language", header::HeaderValue::from_static("fa"));
    headers.insert("Accept-Encoding", header::HeaderValue::from_static("gzip, deflate, br, zstd"));
    headers.insert("x-app-version", header::HeaderValue::from_static(APP_VERSION));

    let (log_tx, log_rx) = mpsc::channel(1000);

    let stats = Arc::new(GlobalStats {
        success: AtomicUsize::new(0),
        logic_fail: AtomicUsize::new(0),
        proxy_fail: AtomicUsize::new(0),
        net_fail: AtomicUsize::new(0),
        limit,
        start_time: Instant::now(),
        is_running: AtomicBool::new(true),
    });

    let app_state = Arc::new(AppState {
        stats: stats.clone(),
        proxy_state: Arc::new(Mutex::new(ProxyState {
            queue: initial_queue,
            active: HashSet::new(),
            dead: HashSet::new(),
        })),
        patterns: Arc::new(DashMap::with_capacity(5000)),
        token,
        cookie,
        use_proxies,
        base_headers: headers,
        log_tx,
    });

    println!("\n{} Starting {} Managers...", ">>>".green(), worker_count);
    
    tokio::spawn(logger_loop(log_rx, stats.clone()));

    for _ in 0..worker_count {
        let state = app_state.clone();
        tokio::spawn(async move {
            manager_loop(state, concurrency).await;
        });
    }

    while stats.is_running.load(Ordering::Relaxed) {
        sleep(Duration::from_secs(1)).await;
    }
    
    println!("\n{}", "Program Stopped.".green().bold());
    Ok(())
}

// --- PROXY LOGIC ---

// Helper to format any raw proxy line into socks5h://
fn format_proxy(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() { return None; }
    
    // If it already has socks5h://, keep it
    if line.starts_with("socks5h://") {
        return Some(line.to_string());
    }
    
    // If socks5://, upgrade to socks5h:// (Remote DNS)
    if line.starts_with("socks5://") {
        return Some(line.replace("socks5://", "socks5h://"));
    }
    
    // If no scheme (just IP:Port), add socks5h://
    if !line.contains("://") {
        return Some(format!("socks5h://{}", line));
    }
    
    // Ignore other protocols (http/https)
    None
}

async fn fetch_master_sources() -> Vec<String> {
    println!("{}", "[Proxy] Fetching Master Source List...".yellow());
    let client = Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    match client.get(MASTER_PROXY_LIST_URL).send().await {
        Ok(resp) => match resp.text().await {
            Ok(text) => {
                let urls: Vec<String> = text.lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| l.starts_with("http"))
                    .collect();
                println!("{} Found {} sources from Master List.", ">>>".green(), urls.len());
                urls
            },
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

async fn fetch_proxies_from_sources(sources: Vec<String>) -> Vec<String> {
    println!("{}", "[Proxy] Downloading proxies in parallel...".yellow());
    let client = Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    
    let tasks: Vec<_> = sources.iter().map(|url| {
        let client = client.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            match client.get(&url).send().await {
                Ok(resp) => match resp.text().await {
                    Ok(text) => Some(text),
                    Err(_) => None,
                },
                Err(_) => None,
            }
        })
    }).collect();

    let results = join_all(tasks).await;
    let mut collected = HashSet::new();

    for res in results {
        if let Ok(Some(text)) = res {
            for line in text.lines() {
                if let Some(p) = format_proxy(line) {
                    collected.insert(p);
                }
            }
        }
    }

    let mut list: Vec<String> = collected.into_iter().collect();
    let mut rng = rand::thread_rng();
    list.shuffle(&mut rng);
    println!("{} Total Proxies Ready: {}\n", "[Proxy]".green(), list.len());
    list
}

fn load_local_proxies() -> Vec<String> {
    println!("{}", format!("[Proxy] Reading local file '{}'...", LOCAL_PROXY_FILE).yellow());
    match fs::read_to_string(LOCAL_PROXY_FILE) {
        Ok(content) => {
            let mut collected = HashSet::new();
            for line in content.lines() {
                if let Some(p) = format_proxy(line) {
                    collected.insert(p);
                }
            }
            let mut list: Vec<String> = collected.into_iter().collect();
            let mut rng = rand::thread_rng();
            list.shuffle(&mut rng);
            println!("{} Loaded {} proxies from file.\n", "[Proxy]".green(), list.len());
            list
        },
        Err(_) => {
            println!("{} File '{}' not found.", "Error:".red(), LOCAL_PROXY_FILE);
            Vec::new()
        }
    }
}

// --- WORKER LOGIC ---

async fn logger_loop(mut rx: mpsc::Receiver<LogEvent>, stats: Arc<GlobalStats>) {
    let mut stdout = io::stdout();
    let start = Instant::now();
    let mut last_title_update = Instant::now();

    while let Some(event) = rx.recv().await {
        let now = Local::now().format("%H:%M:%S");
        match event {
            LogEvent::Success { num, proxy } => {
                let _ = writeln!(stdout, "{} {} {} | Px: {}", 
                    format!("[{}]", now).dim(), "[INVITE SENT]".green().bold(), num, 
                    if proxy.is_empty() { "Direct" } else { &proxy }
                );
            },
            LogEvent::LogicFail { num, reason } => {
                let _ = writeln!(stdout, "{} {} {} | Msg: {}", 
                    format!("[{}]", now).dim(), "[FAIL]".red().bold(), num, reason
                );
            },
            LogEvent::TokenDead => {
                let _ = writeln!(stdout, "\n{} {} {}\n", 
                    "!!!".red().slow_blink(), "CRITICAL: INVALID TOKEN (401)".red().bold().underlined(), "!!!".red().slow_blink()
                );
            }
        }

        if last_title_update.elapsed().as_millis() > 500 {
            let s = stats.success.load(Ordering::Relaxed);
            let lf = stats.logic_fail.load(Ordering::Relaxed);
            let pf = stats.proxy_fail.load(Ordering::Relaxed);
            let nf = stats.net_fail.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 { s as f64 / elapsed } else { 0.0 };
            let title = format!("Irancell Bot | Sent: {} | Invalid: {} | NetRetry: {} | Rate: {:.1}/s", s, lf, pf + nf, rate);
            let _ = stdout.execute(terminal::SetTitle(title));
            last_title_update = Instant::now();
        }
    }
}

async fn manager_loop(state: Arc<AppState>, concurrency: usize) {
    let mut current_proxy: Option<String> = None;
    let mut client: Option<Client> = None;
    let mut forced_direct = false;
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let (retry_tx, mut retry_rx) = mpsc::channel::<String>(concurrency * 2);

    loop {
        if !state.stats.is_running.load(Ordering::Relaxed) { break; }
        if state.stats.success.load(Ordering::Relaxed) >= state.stats.limit {
            state.stats.is_running.store(false, Ordering::Relaxed);
            break;
        }

        if state.use_proxies {
            if current_proxy.is_none() && !forced_direct {
                let mut lock = state.proxy_state.lock();
                if let Some(p) = lock.queue.pop_front() {
                    lock.active.insert(p.clone());
                    current_proxy = Some(p);
                    client = None; 
                } else if lock.active.is_empty() {
                    forced_direct = true;
                    client = None;
                }
            }
        }

        if client.is_none() {
            let mut builder = Client::builder()
                .timeout(Duration::from_secs(12))
                .connect_timeout(Duration::from_secs(5))
                .http1_only()
                .tcp_nodelay(true)
                .danger_accept_invalid_certs(true)
                .brotli(true).gzip(true);

            if let Some(ref p_url) = current_proxy {
                if let Ok(proxy) = Proxy::all(p_url) {
                    builder = builder.proxy(proxy);
                }
            }

            match builder.build() {
                Ok(c) => client = Some(c),
                Err(_) => {
                    if let Some(dead) = current_proxy.take() {
                        let mut lock = state.proxy_state.lock();
                        lock.active.remove(&dead);
                        lock.dead.insert(dead);
                    }
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
            }
        }

        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, 
        };

        let num_to_process = match retry_rx.try_recv() {
            Ok(n) => n, 
            Err(_) => generate_number(&state.patterns) 
        };

        let cli = client.as_ref().unwrap().clone();
        let state_ref = state.clone();
        let proxy_addr_str = current_proxy.clone().unwrap_or_default();
        let is_proxy = state.use_proxies && !forced_direct;
        let retry_tx_clone = retry_tx.clone();

        tokio::spawn(async move {
            let payload = serde_json::json!({
                "application_name": "NGMI",
                "friend_number": format!("98{}", &num_to_process[1..])
            });

            // STEP 1
            let mut headers_step1 = state_ref.base_headers.clone();
            headers_step1.insert("Referer", header::HeaderValue::from_static("https://my.irancell.ir/invite"));

            let res1 = cli.post(API_CHECK).headers(headers_step1).json(&payload).send().await;

            match res1 {
                Ok(resp1) => {
                    if resp1.status().is_success() {
                        let text1 = resp1.text().await.unwrap_or_default();
                        let json1: Option<Value> = serde_json::from_str(&text1).ok();
                        let is_done1 = json1.as_ref().and_then(|j| j.get("message")).and_then(|m| m.as_str()).map(|s| s == "done").unwrap_or(false);

                        if is_done1 {
                            // STEP 2
                            let mut headers_step2 = state_ref.base_headers.clone();
                            headers_step2.insert("Referer", header::HeaderValue::from_static("https://my.irancell.ir/invite/confirm"));
                            let res2 = cli.post(API_NOTIFY).headers(headers_step2).json(&payload).send().await;

                            match res2 {
                                Ok(resp2) => {
                                    let text2 = resp2.text().await.unwrap_or_default();
                                    let json2: Option<Value> = serde_json::from_str(&text2).ok();
                                    let is_done2 = json2.as_ref().and_then(|j| j.get("message")).and_then(|m| m.as_str()).map(|s| s == "done").unwrap_or(false);

                                    if is_done2 {
                                        state_ref.stats.success.fetch_add(1, Ordering::Relaxed);
                                        update_pattern(&state_ref.patterns, &num_to_process);
                                        let _ = state_ref.log_tx.send(LogEvent::Success { num: num_to_process, proxy: proxy_addr_str }).await;
                                    } else {
                                        state_ref.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                                        let _ = state_ref.log_tx.send(LogEvent::LogicFail { num: num_to_process, reason: "S2 Fail".to_string() }).await;
                                    }
                                }
                                Err(_) => {
                                    if is_proxy { state_ref.stats.proxy_fail.fetch_add(1, Ordering::Relaxed); }
                                    else { state_ref.stats.net_fail.fetch_add(1, Ordering::Relaxed); }
                                    let _ = retry_tx_clone.send(num_to_process).await;
                                }
                            }
                        } else {
                            state_ref.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                            let _ = state_ref.log_tx.send(LogEvent::LogicFail { num: num_to_process, reason: "S1 Fail".to_string() }).await;
                        }
                    } else if resp1.status() == StatusCode::UNAUTHORIZED {
                        if state_ref.stats.is_running.swap(false, Ordering::Relaxed) {
                            let _ = state_ref.log_tx.send(LogEvent::TokenDead).await;
                        }
                    } else {
                        state_ref.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                        let _ = state_ref.log_tx.send(LogEvent::LogicFail { num: num_to_process, reason: resp1.status().to_string() }).await;
                    }
                }
                Err(_) => {
                    if is_proxy { state_ref.stats.proxy_fail.fetch_add(1, Ordering::Relaxed); }
                    else { state_ref.stats.net_fail.fetch_add(1, Ordering::Relaxed); }
                    let _ = retry_tx_clone.send(num_to_process).await;
                }
            }
            drop(permit); 
        });
    }
}

fn generate_number(patterns: &DashMap<String, u32>) -> String {
    let mut rng = rand::thread_rng();
    let prefix = PREFIXES.choose(&mut rng).unwrap();
    let mut suffix = String::with_capacity(7);
    for _ in 0..7 {
        suffix.push_str(&rng.gen_range(0..=9).to_string().as_str());
    }
    format!("{}{}", prefix, suffix)
}

fn update_pattern(patterns: &DashMap<String, u32>, num: &str) {
    if num.len() < 7 { return; }
    let prefix = &num[0..4];
    let suffix = &num[4..7];
    let key = format!("{}-{}", prefix, suffix);
    *patterns.entry(key).or_insert(0) += 1;
    if rand::thread_rng().gen_bool(0.001) && patterns.len() > 3000 {
        patterns.retain(|_, v| *v > 1);
    }
}
