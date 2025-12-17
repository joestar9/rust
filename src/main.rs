use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::prelude::*;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore, watch};
use tokio::time::sleep;

// --- Constants ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const MAX_RETRIES_BEFORE_SWITCH: u8 = 3; 

// --- Enums & Structs ---
#[derive(Clone, Copy, PartialEq)]
enum RunMode {
    Direct,
    AutoProxy,
    LocalProxy,
}

enum ProxyStatus {
    Healthy,
    SoftFail,
    HardFail,
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

// --- Helper Functions ---

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

fn generate_random_suffix() -> String {
    let chars: Vec<char> = "1234567890".chars().collect();
    let mut rng = rand::rng(); 
    chars.choose_multiple(&mut rng, 7).collect()
}

fn prompt_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer).unwrap();
    buffer.trim().to_string()
}

async fn log_debug(msg: String) {
    let timestamp = Local::now().format("%H:%M:%S");
    let log_line = format!("[{}] {}\n", timestamp, msg);
    let result = OpenOptions::new().create(true).append(true).open(LOG_FILE);
    if let Ok(mut file) = result { let _ = file.write_all(log_line.as_bytes()); }
}

/// ✅ CRITICAL UPDATE: Force Remote DNS (socks5h)
fn sanitize_proxy_url(raw: &str) -> String {
    let mut clean = raw.trim().to_string();
    
    // Fix common typos
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5h://"); }
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5h://"); }
    
    // Convert standard socks5 to socks5h for better connectivity
    if clean.starts_with("socks5://") {
        clean = clean.replace("socks5://", "socks5h://");
    }

    // Default to socks5h if no scheme provided
    if !clean.contains("://") { 
        return format!("socks5h://{}", clean); 
    }
    
    clean
}

// --- Client Factory ---

fn build_client(token: &str, proxy: Option<Proxy>) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse().unwrap());

    let mut builder = Client::builder()
        .default_headers(headers)
        .tcp_nodelay(true)
        .cookie_store(true)
        .pool_idle_timeout(Duration::from_secs(90))
        .timeout(Duration::from_secs(15)); // 15s timeout is fair for proxies

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

// --- Fetch Logic ---

async fn fetch_proxies_list(_token: String) -> Result<Vec<String>> {
    println!("⏳ Downloading proxy list...");
    // Use a temporary client for fetching list (System Proxy)
    let fetcher = Client::builder().timeout(Duration::from_secs(30)).build()?;
    
    // 1. Get List of Sources
    let sources_text = match fetcher.get(SOURCE_OF_SOURCES_URL).send().await {
        Ok(res) => res.text().await?,
        Err(e) => return Err(anyhow!("Failed to fetch source list: {}", e)),
    };
    
    let source_urls: Vec<String> = sources_text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.starts_with("http"))
        .collect();

    println!("🔗 Found {} sources. Downloading raw proxies...", source_urls.len());

    let mut raw_proxies = HashSet::new();
    let mut tasks = Vec::new();

    // 2. Fetch each source concurrently
    for url in source_urls {
        let c = fetcher.clone();
        let u = url.clone();
        tasks.push(tokio::spawn(async move {
            if let Ok(resp) = c.get(&u).timeout(Duration::from_secs(15)).send().await {
                if let Ok(text) = resp.text().await { return Some(text); }
            }
            None
        }));
    }

    for task in tasks {
        if let Ok(Some(text)) = task.await {
            for line in text.lines() {
                let p = line.trim();
                // Basic validation: must have IP structure
                if !p.is_empty() && (p.contains(':')) {
                    raw_proxies.insert(p.to_string());
                }
            }
        }
    }
    
    let mut final_list: Vec<String> = raw_proxies.into_iter().collect();
    final_list.shuffle(&mut rand::rng());
    Ok(final_list)
}

fn read_local_list() -> Result<Vec<String>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let mut proxies = Vec::new();
    for line in reader.lines() {
        if let Ok(l) = line {
            let p = l.trim().to_string();
            if !p.is_empty() { proxies.push(p); }
        }
    }
    proxies.shuffle(&mut rand::rng());
    Ok(proxies)
}

// --- Robust Worker Logic ---

async fn run_worker_robust(
    worker_id: usize,
    proxy_pool: Arc<Mutex<Vec<String>>>,
    token: String,
    concurrency_limit: usize,
    prefixes: Arc<Vec<String>>,
    success_counter: Arc<Mutex<usize>>,
    target: usize,
    shutdown_rx: watch::Receiver<bool>,
    debug_mode: bool
) {
    let (status_tx, mut status_rx) = mpsc::channel::<ProxyStatus>(concurrency_limit + 10);
    let sem = Arc::new(Semaphore::new(concurrency_limit));

    let mut current_client: Option<Client> = None;
    let mut current_proxy_addr: String = String::new();
    let mut consecutive_errors: u8 = 0;

    loop {
        if *shutdown_rx.borrow() { break; }

        // Feedback Loop
        while let Ok(status) = status_rx.try_recv() {
            match status {
                ProxyStatus::Healthy | ProxyStatus::SoftFail => {
                    if consecutive_errors > 0 { consecutive_errors = 0; }
                },
                ProxyStatus::HardFail => {
                    consecutive_errors += 1;
                }
            }
        }

        // Switch Logic
        if consecutive_errors >= MAX_RETRIES_BEFORE_SWITCH {
            if debug_mode { 
                log_debug(format!("♻️ Worker {} switching. Failed Proxy: {}", worker_id, current_proxy_addr)).await; 
            }
            current_client = None; 
            consecutive_errors = 0; 
        }

        // Setup New Client
        if current_client.is_none() {
            let mut pool = proxy_pool.lock().await;
            if let Some(proxy_raw) = pool.pop() {
                // IMPORTANT: sanitize_proxy_url now forces socks5h://
                let sanitized = sanitize_proxy_url(&proxy_raw);
                if let Ok(proxy_obj) = Proxy::all(&sanitized) {
                    if let Ok(c) = build_client(&token, Some(proxy_obj)) {
                        current_client = Some(c);
                        current_proxy_addr = proxy_raw;
                        consecutive_errors = 0;
                    }
                }
            } else {
                drop(pool);
                // Pool is empty, wait a bit
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        }

        // Execute
        if let Some(client) = current_client.clone() {
            if let Ok(permit) = sem.clone().acquire_owned().await {
                let p_ref = prefixes.clone();
                let s_ref = success_counter.clone();
                let p_addr = current_proxy_addr.clone();
                let tx = status_tx.clone();
                let s_rx = shutdown_rx.clone();
                
                tokio::spawn(async move {
                    let _p = permit; 
                    if *s_rx.borrow() { return; } 

                    let prefix = p_ref.choose(&mut rand::rng()).unwrap();
                    let phone = format!("98{}{}", prefix, generate_random_suffix());

                    let status = perform_invite(worker_id, &p_addr, &client, phone, &s_ref, debug_mode, target).await;
                    let _ = tx.send(status).await;
                });
            }
        }
    }
}

async fn perform_invite(
    id: usize,
    proxy_name: &str,
    client: &Client,
    phone: String,
    success_counter: &Arc<Mutex<usize>>,
    debug_mode: bool,
    target: usize
) -> ProxyStatus {
    if *success_counter.lock().await >= target { return ProxyStatus::Healthy; }

    let data = InviteData { application_name: "NGMI".to_string(), friend_number: phone.clone() };

    // Request 1
    let res1 = client.post(API_CHECK_APP)
        .header("Referer", "https://my.irancell.ir/invite")
        .json(&data).send().await;

    match res1 {
        Ok(resp) => {
            let status_code = resp.status();
            
            // Hard failures (Proxy rejected or Banned)
            if status_code == 403 || status_code == 429 || status_code == 407 {
                return ProxyStatus::HardFail; 
            }

            if status_code.as_u16() == 200 {
                let text = resp.text().await.unwrap_or_default();
                if let Ok(body) = serde_json::from_str::<Value>(&text) {
                     if body["message"] == "done" {
                        // Request 2
                        let res2 = client.post(API_SEND_INVITE)
                            .header("Referer", "https://my.irancell.ir/invite/confirm")
                            .json(&data).send().await;

                        match res2 {
                            Ok(resp2) => {
                                if resp2.status().as_u16() == 200 {
                                    let text2 = resp2.text().await.unwrap_or_default();
                                    if let Ok(body2) = serde_json::from_str::<Value>(&text2) {
                                        if body2["message"] == "done" {
                                            let mut lock = success_counter.lock().await;
                                            *lock += 1;
                                            println!("✅ Worker #{} | Proxy {} | Sent: {} ({}/{})", id, proxy_name, phone, *lock, target);
                                            return ProxyStatus::Healthy;
                                        }
                                    }
                                }
                                return ProxyStatus::SoftFail;
                            },
                            Err(_) => {
                                return ProxyStatus::HardFail;
                            }
                        }
                    }
                }
                return ProxyStatus::Healthy;
            } else {
                if debug_mode { log_debug(format!("Worker {} HTTP {} on {}", id, status_code, phone)).await; }
                return ProxyStatus::SoftFail; 
            }
        },
        Err(e) => {
            // This is the most common error with bad proxies
            if debug_mode { log_debug(format!("Worker {} Net Error: {}", id, e)).await; }
            return ProxyStatus::HardFail; 
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = read_config()?;
    let debug_mode = config.debug.unwrap_or(false);
    if debug_mode {
        println!("🐞 Debug Mode ON.");
        log_debug("--- SESSION STARTED ---".to_string()).await;
    }

    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Loaded {} prefixes.", config.prefixes.len());
    let count_input = prompt_input("🎯 Target SUCCESS Count: ");
    let target_count: usize = count_input.parse().unwrap_or(1000);

    println!("\n✨ Select Mode:");
    println!("1) Direct Mode (No Proxy) 🌐");
    println!("2) Auto Proxy Mode (Smart Rotation - Force Remote DNS) 🚀");
    println!("3) Local Proxy Mode (Smart Rotation - Force Remote DNS) 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    let mode = match mode_input.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    let concurrent_input = prompt_input("⚡ Requests PER PROXY (simultaneous): ");
    let requests_per_proxy: usize = concurrent_input.parse().unwrap_or(5);

    let workers_input = prompt_input("👷 Total Worker Threads: ");
    let worker_count: usize = workers_input.parse().unwrap_or(50);

    // --- PREPARE POOL ---
    let mut raw_pool: Vec<String> = Vec::new();

    if mode == RunMode::LocalProxy {
        raw_pool = read_local_list()?;
        println!("📦 Loaded {} local proxies.", raw_pool.len());
    } else if mode == RunMode::AutoProxy {
        raw_pool = fetch_proxies_list(token.clone()).await?;
        println!("📦 Downloaded {} proxies.", raw_pool.len());
    } else {
        raw_pool.push("Direct".to_string());
    }

    if raw_pool.is_empty() { return Err(anyhow!("❌ Pool is empty.")); }

    let shared_pool = Arc::new(Mutex::new(raw_pool));
    let success_counter = Arc::new(Mutex::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let prefixes = Arc::new(config.prefixes);

    println!("🚀 Launching {} worker threads...", worker_count);

    for id in 0..worker_count {
        let pool = shared_pool.clone();
        let token_clone = token.clone();
        let p_ref = prefixes.clone();
        let s_ref = success_counter.clone();
        let rx = shutdown_rx.clone();
        
        if mode == RunMode::Direct {
            let client = build_client(&token_clone, None)?;
            tokio::spawn(async move {
                let sem = Arc::new(Semaphore::new(requests_per_proxy));
                loop {
                    if *rx.borrow() { break; }
                    if let Ok(permit) = sem.clone().acquire_owned().await {
                        let c = client.clone();
                        let pr = p_ref.clone();
                        let sr = s_ref.clone();
                        tokio::spawn(async move {
                            let _p = permit;
                            let prefix = pr.choose(&mut rand::rng()).unwrap();
                            let phone = format!("98{}{}", prefix, generate_random_suffix());
                            perform_invite(id, "DIRECT", &c, phone, &sr, debug_mode, target_count).await;
                        });
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                run_worker_robust(id, pool, token_clone, requests_per_proxy, p_ref, s_ref, target_count, rx, debug_mode).await;
            });
        }
    }

    let monitor_success = success_counter.clone();
    let monitor_tx = shutdown_tx.clone();
    
    loop {
        sleep(Duration::from_secs(1)).await;
        if *monitor_success.lock().await >= target_count {
            let _ = monitor_tx.send(true);
            break;
        }
    }

    println!("🏁 Target reached. Stopping...");
    sleep(Duration::from_secs(2)).await;
    println!("\n📊 Final Results: {} Successes", *success_counter.lock().await);
    Ok(())
}

