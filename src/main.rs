use anyhow::Result;
use chrono::Local;
use crossterm::{
    style::{Color, Stylize},
    terminal, ExecutableCommand,
};
use dashmap::DashMap;
use parking_lot::Mutex;
use rand::prelude::*;
use reqwest::{header, Client, Proxy};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::sleep;

const API_URL: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const STATIC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const CONFIG_FILE: &str = "config.json";

const PREFIXES: [&str; 14] = [
    "0900", "0901", "0902", "0903", "0904", "0905", "0930", "0933", "0935", "0936", "0937", "0938",
    "0939", "0941",
];

const PROXY_SOURCES: [&str; 6] = [
    "https://raw.githubusercontent.com/ClearProxy/checked-proxy-list/refs/heads/main/socks5/raw/all.txt",
    "https://raw.githubusercontent.com/proxifly/free-proxy-list/refs/heads/main/proxies/protocols/socks5/data.txt",
    "https://raw.githubusercontent.com/vmheaven/VMHeaven-Free-Proxy-Updated/refs/heads/main/socks5.txt",
    "https://raw.githubusercontent.com/s0mecode/socks5-proxies/refs/heads/main/socks5_5000ms.txt",
    "https://raw.githubusercontent.com/vakhov/fresh-proxy-list/refs/heads/master/socks5.txt",
    "https://raw.githubusercontent.com/trio666/proxy-checker/refs/heads/main/socks5.txt"
];

// --- Structs ---

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
    log_tx: mpsc::Sender<LogEvent>, // Channel for live logs
}

// Enum for Live Logs
enum LogEvent {
    Success { num: String, proxy: String },
    LogicFail { num: String, status: String },
    NetFail { num: String, error: String, is_proxy: bool },
    Info(String),
}

// --- Helper Functions ---

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
    
    // Fallback to manual input
    let token = input("Enter Authorization Token (Bearer ...): ");
    let cookie = input("Enter Cookie: ");
    (token, cookie)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Clear screen
    let mut stdout = io::stdout();
    let _ = stdout.execute(terminal::Clear(terminal::ClearType::All));
    let _ = stdout.execute(crossterm::cursor::MoveTo(0, 0));
    
    println!("{}", "╔═══════════════════════════════════════════════════════════════╗".cyan().bold());
    println!("{}", "║    IRANCELL BOT - LIVE LOGGING & CONFIG EDITION               ║".cyan().bold());
    println!("{}", "╚═══════════════════════════════════════════════════════════════╝".cyan().bold());

    // --- CONFIG LOAD ---
    let (token, cookie) = load_config();
    if token.trim().is_empty() || cookie.trim().is_empty() {
        println!("{}", "Error: Token and Cookie are required.".red());
        return Ok(());
    }

    let limit: usize = input("Enter Limit (e.g., 20000): ").parse().unwrap_or(100000);
    let worker_count: usize = input("Managers (default 50): ").parse().unwrap_or(50);
    let concurrency: usize = input("Concurrency (default 5): ").parse().unwrap_or(5);
    let use_proxies = input("Use Auto-Proxy List? (y/n): ").to_lowercase() == "y";

    // --- PROXY SETUP ---
    let mut initial_queue = VecDeque::new();
    if use_proxies {
        let list = fetch_proxies().await;
        if list.is_empty() {
            println!("{}", "Warning: No proxies found. Switching to DIRECT mode.".red());
        } else {
            initial_queue = list.into();
        }
    }

    // --- HEADERS ---
    let mut headers = header::HeaderMap::new();
    headers.insert("Authorization", header::HeaderValue::from_str(&token).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("Cookie", header::HeaderValue::from_str(&cookie).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("User-Agent", header::HeaderValue::from_static(STATIC_USER_AGENT));
    headers.insert("Origin", header::HeaderValue::from_static("https://my.irancell.ir"));
    headers.insert("Referer", header::HeaderValue::from_static("https://my.irancell.ir/"));
    headers.insert("Content-Type", header::HeaderValue::from_static("application/json"));
    headers.insert("Accept", header::HeaderValue::from_static("application/json, text/plain, */*"));
    headers.insert("Accept-Language", header::HeaderValue::from_static("fa"));

    // --- LOG CHANNEL ---
    // Increase channel buffer to prevent blocking workers if logging is slow
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
    
    // Spawn Log Printer
    tokio::spawn(logger_loop(log_rx, stats.clone()));

    // Spawn Workers
    for _ in 0..worker_count {
        let state = app_state.clone();
        tokio::spawn(async move {
            manager_loop(state, concurrency).await;
        });
    }

    // Keep main alive
    while stats.is_running.load(Ordering::Relaxed) {
        sleep(Duration::from_secs(1)).await;
    }
    
    println!("\n{}", "Finished.".green().bold());
    Ok(())
}

async fn logger_loop(mut rx: mpsc::Receiver<LogEvent>, stats: Arc<GlobalStats>) {
    let mut stdout = io::stdout();
    let start = Instant::now();
    let mut last_title_update = Instant::now();

    while let Some(event) = rx.recv().await {
        let now = Local::now().format("%H:%M:%S");
        
        match event {
            LogEvent::Success { num, proxy } => {
                let _ = writeln!(stdout, "{} {} {} | Px: {}", 
                    format!("[{}]", now).dim(), 
                    "[SUCCESS]".green().bold(), 
                    num, 
                    if proxy.is_empty() { "Direct" } else { &proxy }
                );
            },
            LogEvent::LogicFail { num, status } => {
                let _ = writeln!(stdout, "{} {} {} | Status: {}", 
                    format!("[{}]", now).dim(), 
                    "[FAIL]".red().bold(), 
                    num, 
                    status
                );
            },
            LogEvent::NetFail { num, error, is_proxy } => {
                // Determine label based on context
                let label = if is_proxy { "[PROXY-ERR]".yellow() } else { "[NET-ERR]".magenta() };
                let _ = writeln!(stdout, "{} {} {} | {}", 
                    format!("[{}]", now).dim(), 
                    label, 
                    num, 
                    error
                );
            },
            LogEvent::Info(msg) => {
                let _ = writeln!(stdout, "{} {}", format!("[{}]", now).dim(), msg.blue());
            }
        }

        // Update Title Bar every 500ms (to avoid syscall spam)
        if last_title_update.elapsed().as_millis() > 500 {
            let s = stats.success.load(Ordering::Relaxed);
            let lf = stats.logic_fail.load(Ordering::Relaxed);
            let pf = stats.proxy_fail.load(Ordering::Relaxed);
            let nf = stats.net_fail.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 { s as f64 / elapsed } else { 0.0 };

            let title = format!("Irancell Bot | OK: {} | Fail: {} | ProxyErr: {} | Rate: {:.1}/s", 
                s, lf + nf, pf, rate);
            
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

    loop {
        if !state.stats.is_running.load(Ordering::Relaxed) { break; }
        if state.stats.success.load(Ordering::Relaxed) >= state.stats.limit {
            state.stats.is_running.store(false, Ordering::Relaxed);
            break;
        }

        // --- Proxy Acquisition ---
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

        // --- Client Build ---
        if client.is_none() {
            let mut builder = Client::builder()
                .timeout(Duration::from_secs(10))
                .tcp_nodelay(true)
                .danger_accept_invalid_certs(true);

            if let Some(ref p_url) = current_proxy {
                if let Ok(proxy) = Proxy::all(p_url) {
                    builder = builder.proxy(proxy);
                }
            }

            match builder.build() {
                Ok(c) => client = Some(c),
                Err(_) => {
                    // Proxy dead on build
                    if let Some(dead) = current_proxy.take() {
                        let mut lock = state.proxy_state.lock();
                        lock.active.remove(&dead);
                        lock.dead.insert(dead);
                    }
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }

        // --- Request Execution ---
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, 
        };

        let cli = client.as_ref().unwrap().clone();
        let state_ref = state.clone();
        let num = generate_number(&state.patterns);
        let proxy_addr_str = current_proxy.clone().unwrap_or_default();
        let is_proxy = state.use_proxies && !forced_direct;

        tokio::spawn(async move {
            let payload = serde_json::json!({
                "application_name": "NGMI",
                "friend_number": format!("98{}", &num[1..])
            });

            let res = cli.post(API_URL)
                .headers(state_ref.base_headers.clone()) 
                .json(&payload)
                .send()
                .await;

            match res {
                Ok(resp) => {
                    if resp.status().is_success() {
                        state_ref.stats.success.fetch_add(1, Ordering::Relaxed);
                        update_pattern(&state_ref.patterns, &num);
                        // Log Success
                        let _ = state_ref.log_tx.send(LogEvent::Success { num, proxy: proxy_addr_str }).await;
                    } else {
                        state_ref.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                        // Log Fail
                        let _ = state_ref.log_tx.send(LogEvent::LogicFail { 
                            num, 
                            status: resp.status().as_u16().to_string() 
                        }).await;
                    }
                }
                Err(e) => {
                    if is_proxy {
                        state_ref.stats.proxy_fail.fetch_add(1, Ordering::Relaxed);
                    } else {
                        state_ref.stats.net_fail.fetch_add(1, Ordering::Relaxed);
                    }
                    // Log Error
                    let error_msg = if e.is_timeout() { "Timeout".to_string() } else { "Conn Err".to_string() };
                    let _ = state_ref.log_tx.send(LogEvent::NetFail { 
                        num, 
                        error: error_msg,
                        is_proxy 
                    }).await;
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

async fn fetch_proxies() -> Vec<String> {
    let client = Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let mut collected = HashSet::new();

    for url in PROXY_SOURCES {
        if let Ok(resp) = client.get(url).send().await {
            if let Ok(text) = resp.text().await {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() { continue; }
                    let proxy = if !line.contains("://") {
                        format!("socks5://{}", line)
                    } else if line.to_lowercase().starts_with("socks5://") {
                        line.to_string()
                    } else {
                        continue;
                    };
                    collected.insert(proxy);
                }
            }
        }
    }
    let mut list: Vec<String> = collected.into_iter().collect();
    let mut rng = rand::thread_rng();
    list.shuffle(&mut rng);
    println!("{} Total Proxies: {}\n", "[Proxy Manager]".green(), list.len());
    list
}
