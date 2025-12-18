use anyhow::{anyhow, Context, Result};
use chrono::Local;
use rand::prelude::*;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore, watch};
use tokio::time::sleep;

// --- CONSTANTS ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.json";
const MAX_RETRIES_BEFORE_SWITCH: usize = 3;
const CHARSET_NUMBERS: &[u8] = b"0123456789";

// --- ENUMS & STRUCTS ---

#[derive(Clone, Copy, PartialEq, Debug)]
enum RunMode {
    Direct,
    AutoProxy,
    LocalProxy,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ProxyFilter {
    All,
    Http,
    Https,
    Socks4,
    Socks5,
}

#[derive(Clone, PartialEq)]
enum ProxyStatus {
    Healthy,
    SoftFail,   // Temporary issue (e.g., 500, 404, bad response)
    HardFail,   // Connection issue, timeout, 403, 429
}

#[derive(Serialize)]
struct InviteData {
    application_name: String,
    friend_number: String,
}

#[derive(Deserialize, Debug)]
struct AppConfig {
    token: Option<String>,
    prefixes: Vec<String>,
    debug: Option<bool>,
}

#[derive(Deserialize, Debug)]
struct ProxySourceConfig {
    #[serde(default)]
    http: Vec<String>,
    #[serde(default)]
    https: Vec<String>,
    #[serde(default)]
    socks4: Vec<String>,
    #[serde(default)]
    socks5: Vec<String>,
}

// --- UTILS ---

fn generate_random_suffix() -> String {
    let mut rng = rand::rng();
    (0..7)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET_NUMBERS.len());
            CHARSET_NUMBERS[idx] as char
        })
        .collect()
}

fn prompt_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer).unwrap();
    buffer.trim().to_string()
}

fn format_proxy_url(raw: &str, default_proto: &str) -> String {
    let mut clean = raw.trim().to_string();
    
    // Fix common typos
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5://"); }
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5://"); }
    
    // Force DNS resolution on proxy side for socks5 if requested
    if default_proto == "socks5" || default_proto == "socks5h" {
        if clean.starts_with("socks5://") {
            return clean.replace("socks5://", "socks5h://");
        }
        if !clean.contains("://") {
            return format!("socks5h://{}", clean);
        }
    }

    if default_proto == "http" && !clean.contains("://") {
        // Attempt to guess HTTPS ports
        if let Some(port_str) = clean.split(':').last() {
            if let Ok(port) = port_str.parse::<u16>() {
                if [443, 8443, 2053, 2083, 2087, 2096].contains(&port) {
                    return format!("https://{}", clean);
                }
            }
        }
        return format!("http://{}", clean);
    }

    if !clean.contains("://") { 
        return format!("{}://{}", default_proto, clean); 
    }
    
    clean
}

// --- LOGGING SYSTEM ---

struct AsyncLogger {
    tx: mpsc::Sender<String>,
}

impl AsyncLogger {
    fn new() -> Self {
        let (tx, mut rx) = mpsc::channel(100);
        tokio::spawn(async move {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(LOG_FILE)
                .expect("Failed to open log file");

            while let Some(msg) = rx.recv().await {
                let timestamp = Local::now().format("%H:%M:%S");
                let log_line = format!("[{}] {}\n", timestamp, msg);
                if let Err(e) = file.write_all(log_line.as_bytes()) {
                    eprintln!("Log write error: {}", e);
                }
            }
        });
        Self { tx }
    }

    async fn log(&self, msg: String) {
        let _ = self.tx.send(msg).await;
    }
}

// --- PROXY MANAGER ---

struct ProxyManager {
    pool: Arc<Mutex<Vec<String>>>,
    mode: RunMode,
    // AutoProxy config
    token: String,
    filter: ProxyFilter,
    // LocalProxy config
    local_path: String,
    local_proto: String,
    // State
    is_refilling: Arc<AtomicBool>,
}

impl ProxyManager {
    fn new(mode: RunMode, token: String, filter: ProxyFilter, local_path: String, local_proto: String) -> Self {
        Self {
            pool: Arc::new(Mutex::new(Vec::new())),
            mode,
            token,
            filter,
            local_path,
            local_proto,
            is_refilling: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn get_proxy(&self) -> Option<String> {
        loop {
            let mut lock = self.pool.lock().await;
            if let Some(proxy) = lock.pop() {
                return Some(proxy);
            }
            drop(lock); // Release lock before refilling

            if self.mode == RunMode::Direct {
                return None; // Should not happen in direct mode loop logic
            }

            // Attempt to trigger refill
            self.trigger_refill_if_needed().await;
            
            // Wait a bit for refill to happen
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn trigger_refill_if_needed(&self) {
        // CompareAndSet: If false, set true and return Ok(_). If true, return Err(_).
        // This ensures only one task triggers the refill.
        if self.is_refilling.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            println!("🔄 Proxy pool empty! Refilling proxies from source...");
            
            let new_proxies = match self.mode {
                RunMode::AutoProxy => self.fetch_online_proxies().await,
                RunMode::LocalProxy => self.read_local_proxies().await,
                _ => Ok(vec![]),
            };

            match new_proxies {
                Ok(mut list) => {
                    if list.is_empty() {
                        println!("⚠️ Warning: Source returned 0 proxies. Retrying in 5s...");
                        sleep(Duration::from_secs(5)).await;
                    } else {
                        println!("✅ Refilled pool with {} proxies.", list.len());
                        let mut lock = self.pool.lock().await;
                        lock.append(&mut list);
                    }
                },
                Err(e) => {
                    println!("❌ Failed to refill proxies: {}. Retrying in 5s...", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }

            // Reset flag
            self.is_refilling.store(false, Ordering::SeqCst);
        }
    }

    async fn fetch_online_proxies(&self) -> Result<Vec<String>> {
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        
        let json_text = client.get(SOURCE_OF_SOURCES_URL).send().await?.text().await?;
        let sources: ProxySourceConfig = serde_json::from_str(&json_text)?;

        let mut raw_proxies = HashSet::new();
        let mut tasks = Vec::new();

        let spawn_dl = |url: String, proto: &'static str, c: Client| {
            tokio::spawn(async move {
                let mut found = Vec::new();
                if let Ok(resp) = c.get(&url).timeout(Duration::from_secs(15)).send().await {
                    if let Ok(text) = resp.text().await {
                        for line in text.lines() {
                            let p = line.trim();
                            if !p.is_empty() && p.contains(':') {
                                found.push(format_proxy_url(p, proto));
                            }
                        }
                    }
                }
                found
            })
        };

        match self.filter {
            ProxyFilter::All => {
                for url in sources.http { tasks.push(spawn_dl(url, "http", client.clone())); }
                for url in sources.https { tasks.push(spawn_dl(url, "https", client.clone())); }
                for url in sources.socks4 { tasks.push(spawn_dl(url, "socks4", client.clone())); }
                for url in sources.socks5 { tasks.push(spawn_dl(url, "socks5h", client.clone())); }
            },
            ProxyFilter::Http => for url in sources.http { tasks.push(spawn_dl(url, "http", client.clone())); },
            ProxyFilter::Https => for url in sources.https { tasks.push(spawn_dl(url, "https", client.clone())); },
            ProxyFilter::Socks4 => for url in sources.socks4 { tasks.push(spawn_dl(url, "socks4", client.clone())); },
            ProxyFilter::Socks5 => for url in sources.socks5 { tasks.push(spawn_dl(url, "socks5h", client.clone())); },
        }

        for task in tasks {
            if let Ok(proxies) = task.await {
                for p in proxies { raw_proxies.insert(p); }
            }
        }

        let mut final_list: Vec<String> = raw_proxies.into_iter().collect();
        final_list.shuffle(&mut rand::rng());
        Ok(final_list)
    }

    async fn read_local_proxies(&self) -> Result<Vec<String>> {
        // Run blocking IO in spawn_blocking
        let path = self.local_path.clone();
        let proto = self.local_proto.clone();
        
        tokio::task::spawn_blocking(move || {
            let file = File::open(&path).context(format!("Could not open file: {}", path))?;
            let reader = BufReader::new(file);
            let mut unique = HashSet::new();
            
            for line in reader.lines() {
                if let Ok(l) = line {
                    let p = l.trim();
                    if !p.is_empty() {
                        unique.insert(format_proxy_url(p, &proto));
                    }
                }
            }
            let mut list: Vec<String> = unique.into_iter().collect();
            list.shuffle(&mut rand::rng());
            Ok(list)
        }).await?
    }
}

// --- WORKER LOGIC ---

async fn build_client(token: &str, proxy: Option<Proxy>) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa-IR,fa;q=0.9,en-US;q=0.8,en;q=0.7".parse().unwrap());
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse().unwrap());

    let mut builder = Client::builder()
        .default_headers(headers)
        .tcp_nodelay(true)
        .cookie_store(true)
        .pool_idle_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(12));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

async fn run_worker(
    id: usize,
    manager: Arc<ProxyManager>,
    token: String,
    prefixes: Arc<Vec<String>>,
    success_counter: Arc<AtomicUsize>,
    target: usize,
    mut shutdown: watch::Receiver<bool>,
    logger: Arc<AsyncLogger>,
    debug: bool,
    send_invite: bool,
    req_per_proxy: usize
) {
    let mut client: Option<Client> = None;
    let mut current_proxy = String::new();
    let mut fails = 0;
    let mut requests_made = 0;

    loop {
        if *shutdown.borrow() { break; }

        // Get or Refresh Client
        if client.is_none() || fails >= MAX_RETRIES_BEFORE_SWITCH || requests_made >= req_per_proxy {
            if debug && !current_proxy.is_empty() {
                logger.log(format!("Worker {} switching proxy. (Fails: {}, Reqs: {})", id, fails, requests_made)).await;
            }
            
            // Mode handling
            match manager.mode {
                RunMode::Direct => {
                     // In direct mode, we just build client once usually, but lets robustly rebuild
                     if let Ok(c) = build_client(&token, None).await {
                         client = Some(c);
                         current_proxy = "DIRECT".to_string();
                         fails = 0; requests_made = 0;
                     } else {
                         sleep(Duration::from_secs(5)).await;
                     }
                },
                _ => {
                    // Fetch new proxy from manager (handles refill automatically)
                    if let Some(p_addr) = manager.get_proxy().await {
                        if let Ok(proxy_obj) = Proxy::all(&p_addr) {
                            if let Ok(c) = build_client(&token, Some(proxy_obj)).await {
                                client = Some(c);
                                current_proxy = p_addr;
                                fails = 0; requests_made = 0;
                            } else {
                                // Proxy invalid?
                                fails += 1;
                            }
                        }
                    } 
                    // if get_proxy loops inside, we get a proxy eventually
                }
            }
        }

        // Perform Task
        if let Some(c) = &client {
             // Check target limit globally
             if success_counter.load(Ordering::Relaxed) >= target { break; }

             let prefix = prefixes.choose(&mut rand::rng()).unwrap();
             let phone = format!("98{}{}", prefix, generate_random_suffix());

             let status = perform_invite(id, &current_proxy, c, phone, &success_counter, target, &logger, send_invite, debug).await;
             
             requests_made += 1;
             match status {
                 ProxyStatus::Healthy => { fails = 0; },
                 ProxyStatus::SoftFail => { fails += 1; }, // Count soft fails too, maybe switch if too many
                 ProxyStatus::HardFail => { fails = MAX_RETRIES_BEFORE_SWITCH + 1; }, // Force switch
             }
        }
    }
}

async fn perform_invite(
    worker_id: usize,
    proxy: &str,
    client: &Client,
    phone: String,
    counter: &Arc<AtomicUsize>,
    target: usize,
    logger: &Arc<AsyncLogger>,
    send_sms: bool,
    debug: bool
) -> ProxyStatus {
    let data = InviteData { application_name: "NGMI".to_string(), friend_number: phone.clone() };

    // Step 1: Check eligibility
    match client.post(API_CHECK_APP).json(&data).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status == 403 || status == 429 || status == 407 { return ProxyStatus::HardFail; }
            if status.is_server_error() { return ProxyStatus::SoftFail; }

            if status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                if text.contains(r#""message":"done""#) {
                    
                    // Step 2: Send Invite (if enabled)
                    if send_sms {
                        match client.post(API_SEND_INVITE).json(&data).send().await {
                            Ok(resp2) => {
                                if resp2.status().is_success() {
                                    let text2 = resp2.text().await.unwrap_or_default();
                                    if text2.contains(r#""message":"done""#) {
                                        let current = counter.fetch_add(1, Ordering::SeqCst) + 1;
                                        println!("✅ Worker #{} | {} | SENT: {} ({}/{})", worker_id, proxy, phone, current, target);
                                        return ProxyStatus::Healthy;
                                    }
                                }
                                return ProxyStatus::SoftFail;
                            },
                            Err(_) => return ProxyStatus::HardFail,
                        }
                    } else {
                        // Check only mode
                        let current = counter.fetch_add(1, Ordering::SeqCst) + 1;
                        println!("✅ Worker #{} | {} | CHECKED: {} ({}/{})", worker_id, proxy, phone, current, target);
                        return ProxyStatus::Healthy;
                    }
                }
                return ProxyStatus::Healthy; // Request worked, just not eligible or already invited
            } else {
                if debug { logger.log(format!("Worker {} HTTP {} on {}", worker_id, status, phone)).await; }
                return ProxyStatus::SoftFail;
            }
        },
        Err(e) => {
            if debug { logger.log(format!("Worker {} Network Error: {}", worker_id, e)).await; }
            return ProxyStatus::HardFail;
        }
    }
}

// --- MAIN ---

fn read_config() -> Result<AppConfig> {
    if !std::path::Path::new(CONFIG_FILE).exists() {
        return Err(anyhow!("❌ '{}' not found.", CONFIG_FILE));
    }
    let file = File::open(CONFIG_FILE).context("Error opening config file")?;
    let reader = BufReader::new(file);
    let config: AppConfig = serde_json::from_reader(reader).context("❌ Failed to parse config.json.")?;
    if config.prefixes.is_empty() { return Err(anyhow!("❌ Prefixes list is empty!")); }
    Ok(config)
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Setup
    let config = read_config()?;
    let debug_mode = config.debug.unwrap_or(false);
    let logger = Arc::new(AsyncLogger::new());
    
    if debug_mode {
        println!("🐞 Debug Mode ON.");
        logger.log("--- SESSION STARTED ---".to_string()).await;
    }

    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Loaded {} prefixes.", config.prefixes.len());
    let target_count: usize = prompt_input("🎯 Target SUCCESS Count: ").parse().unwrap_or(1000);

    // 2. Select Mode
    println!("\n✨ Select Mode:");
    println!("1) Direct Mode (No Proxy) 🌐");
    println!("2) Auto Proxy Mode (Online Sources) 🚀");
    println!("3) Local Proxy Mode (File) 📁");
    let mode_choice = prompt_input("Choice [1-3]: ");

    let mut run_mode = RunMode::Direct;
    let mut proxy_filter = ProxyFilter::All;
    let mut local_path = "socks5.txt".to_string();
    let mut local_proto = "socks5h".to_string();

    match mode_choice.as_str() {
        "2" => {
            run_mode = RunMode::AutoProxy;
            println!("\n🔍 Select Proxy Protocol to Download:");
            println!("1) All (Default)");
            println!("2) HTTP");
            println!("3) HTTPS");
            println!("4) SOCKS4");
            println!("5) SOCKS5");
            proxy_filter = match prompt_input("Choice [1-5]: ").as_str() {
                "2" => ProxyFilter::Http,
                "3" => ProxyFilter::Https,
                "4" => ProxyFilter::Socks4,
                "5" => ProxyFilter::Socks5,
                _ => ProxyFilter::All,
            };
        },
        "3" => {
            run_mode = RunMode::LocalProxy;
            let path_in = prompt_input("📁 Enter Proxy File Path: ");
            if !path_in.trim().is_empty() { local_path = path_in; }
            
            println!("\n🔍 Select Default Protocol:");
            println!("1) Auto/Socks5h (Default)");
            println!("2) HTTP");
            println!("3) HTTPS");
            local_proto = match prompt_input("Choice [1-3]: ").as_str() {
                "2" => "http".to_string(),
                "3" => "https".to_string(),
                _ => "socks5h".to_string(),
            };
        },
        _ => {}
    }

    // 3. Select Action
    println!("\n🔧 Select Method:");
    println!("1) Send Invite SMS");
    println!("2) Don't Send (Check Only)");
    let send_invite = prompt_input("Choice: ") != "2";

    let requests_per_proxy: usize = prompt_input("⚡ Requests PER Proxy (before switch): ").parse().unwrap_or(5);
    let worker_count: usize = prompt_input("👷 Total Worker Threads: ").parse().unwrap_or(50);

    // 4. Initialize Manager & Workers
    let manager = Arc::new(ProxyManager::new(
        run_mode, 
        token.clone(), 
        proxy_filter, 
        local_path, 
        local_proto
    ));

    // Initial fill for manager to avoid all workers spinning at once on start
    manager.trigger_refill_if_needed().await;
    // Small wait to let initial fetch happen
    if run_mode != RunMode::Direct {
        println!("⏳ Waiting for initial proxy pool...");
        sleep(Duration::from_secs(3)).await;
    }

    let success_counter = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let prefixes = Arc::new(config.prefixes);

    println!("🚀 Launching {} workers...", worker_count);

    let mut handles = Vec::new();
    for id in 0..worker_count {
        let mgr = manager.clone();
        let tok = token.clone();
        let pre = prefixes.clone();
        let cnt = success_counter.clone();
        let rx = shutdown_rx.clone();
        let log = logger.clone();
        
        handles.push(tokio::spawn(async move {
            run_worker(id, mgr, tok, pre, cnt, target_count, rx, log, debug_mode, send_invite, requests_per_proxy).await;
        }));
    }

    // 5. Monitor Loop
    loop {
        sleep(Duration::from_secs(1)).await;
        let current_success = success_counter.load(Ordering::Relaxed);
        if current_success >= target_count {
            let _ = shutdown_tx.send(true);
            break;
        }
    }

    println!("\n🏁 Target reached! Waiting for workers to finish...");
    for h in handles { let _ = h.await; }
    
    println!("📊 Finished. Total Success: {}", success_counter.load(Ordering::SeqCst));
    Ok(())
}
