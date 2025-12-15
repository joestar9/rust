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
use tokio::sync::{Mutex, Semaphore, watch};
use tokio::time::sleep;

// --- Constants ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const PROXY_ERROR_FILE: &str = "proxy_errors.log"; // 📄 لاگ مخصوص خطای پراکسی
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const TARGET_CHECK_URL: &str = "https://my.irancell.ir/invite"; 
const VALIDATION_TIMEOUT: u64 = 10; // افزایش تایم‌اوت تست به ۱۰ ثانیه برای اطمینان

// --- Structs ---

#[derive(Clone, Copy, PartialEq)]
enum RunMode {
    Direct,
    AutoProxy,
    LocalProxy,
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
        return Err(anyhow!("❌ '{}' not found. Please create it.", CONFIG_FILE));
    }
    let file = File::open(CONFIG_FILE).context("Error opening config file")?;
    let reader = BufReader::new(file);
    let config: AppConfig = serde_json::from_reader(reader).context("❌ Failed to parse config.json.")?;
    if config.prefixes.is_empty() { return Err(anyhow!("❌ The 'prefixes' list cannot be empty!")); }
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

async fn log_proxy_error(proxy: &str, error: &str) {
    let timestamp = Local::now().format("%H:%M:%S");
    let log_line = format!("[{}] Proxy: {} | Error: {}\n", timestamp, proxy, error);
    let result = OpenOptions::new().create(true).append(true).open(PROXY_ERROR_FILE);
    if let Ok(mut file) = result { let _ = file.write_all(log_line.as_bytes()); }
}

fn sanitize_proxy_url(raw: &str) -> String {
    let mut clean = raw.trim().to_string();
    // Fix typos
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5://"); }
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5://"); }
    // Default to socks5 if no scheme provided (common in lists)
    if !clean.contains("://") { return format!("socks5://{}", clean); }
    clean
}

// --- Client Factory ---

fn build_client(token: &str, proxy: Option<Proxy>, timeout_secs: u64) -> Result<Client> {
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
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(50) // Allow high concurrency per host
        .timeout(Duration::from_secs(timeout_secs));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

// --- Detailed Proxy Validation ---

async fn fetch_and_validate_strict(token: String) -> Result<Vec<(String, Client)>> {
    println!("⏳ Downloading proxy list...");
    
    let fetcher = Client::builder().timeout(Duration::from_secs(30)).build()?;
    let sources_text = fetcher.get(SOURCE_OF_SOURCES_URL).send().await?.text().await?;
    let source_urls: Vec<String> = sources_text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.starts_with("http"))
        .collect();

    let mut raw_proxies = HashSet::new();
    let mut download_tasks = Vec::new();

    for url in source_urls {
        let c = fetcher.clone();
        let u = url.clone();
        download_tasks.push(tokio::spawn(async move {
            if let Ok(resp) = c.get(&u).timeout(Duration::from_secs(15)).send().await {
                if let Ok(text) = resp.text().await { return Some(text); }
            }
            None
        }));
    }

    for task in download_tasks {
        if let Ok(Some(text)) = task.await {
            for line in text.lines() {
                let p = line.trim();
                if !p.is_empty() && (p.contains(':') || p.contains("://")) {
                    raw_proxies.insert(p.to_string());
                }
            }
        }
    }

    let total = raw_proxies.len();
    println!("📦 Testing {} proxies individually. Errors logged to '{}'...", total, PROXY_ERROR_FILE);
    
    // Clear old error log
    File::create(PROXY_ERROR_FILE)?; 

    // We use a semaphore to limit simultaneous CHECKS, but we store specific clients
    let semaphore = Arc::new(Semaphore::new(300));
    let valid_pairs = Arc::new(Mutex::new(Vec::new()));
    let mut check_tasks = Vec::new();

    for proxy_raw in raw_proxies {
        let sem = semaphore.clone();
        let output = valid_pairs.clone();
        let t_token = token.clone();
        let p_raw = proxy_raw.clone();

        check_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let sanitized = sanitize_proxy_url(&p_raw);
            
            match Proxy::all(&sanitized) {
                Ok(proxy_obj) => {
                    // Try to build client
                    match build_client(&t_token, Some(proxy_obj), VALIDATION_TIMEOUT) {
                        Ok(client) => {
                            // Try to Connect
                            match client.head(TARGET_CHECK_URL).send().await {
                                Ok(_) => {
                                    // SUCCESS!
                                    output.lock().await.push((p_raw, client));
                                    print!("+");
                                    let _ = io::stdout().flush();
                                },
                                Err(e) => {
                                    // LOG CONNECTION ERROR
                                    log_proxy_error(&p_raw, &e.to_string()).await;
                                    print!("-");
                                    let _ = io::stdout().flush();
                                }
                            }
                        },
                        Err(e) => log_proxy_error(&p_raw, &format!("Build error: {}", e)).await,
                    }
                },
                Err(e) => log_proxy_error(&p_raw, &format!("Bad Format: {}", e)).await,
            }
        }));
    }

    for t in check_tasks { let _ = t.await; }
    
    let result = valid_pairs.lock().await.drain(..).collect();
    println!("\n");
    Ok(result)
}

fn read_local_strict(token: &str) -> Result<Vec<(String, Client)>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let mut valid = Vec::new();
    println!("📁 Processing local proxies...");
    
    for line in reader.lines() {
        if let Ok(l) = line {
            let p_raw = l.trim().to_string();
            if !p_raw.is_empty() {
                let sanitized = sanitize_proxy_url(&p_raw);
                if let Ok(p_obj) = Proxy::all(&sanitized) {
                    if let Ok(c) = build_client(token, Some(p_obj), 15) {
                        valid.push((p_raw, c));
                    }
                }
            }
        }
    }
    Ok(valid)
}

// --- Worker Logic ---

/// This runs INSIDE a worker. It uses ONE client (one proxy) and launches multiple concurrent requests.
async fn run_worker_with_proxy(
    worker_id: usize,
    proxy_name: String,
    client: Client,
    concurrency_limit: usize,
    prefixes: Arc<Vec<String>>,
    success_counter: Arc<Mutex<usize>>,
    target: usize,
    shutdown_rx: watch::Receiver<bool>,
    debug_mode: bool
) {
    // Semaphore restricts concurrency PER WORKER (PER PROXY)
    let sem = Arc::new(Semaphore::new(concurrency_limit));
    let mut tasks = Vec::new();

    loop {
        if *shutdown_rx.borrow() { break; }

        // Clean up finished tasks
        tasks.retain(|h: &tokio::task::JoinHandle<()>| !h.is_finished());

        // Try to acquire permission to send a request
        if let Ok(permit) = sem.clone().acquire_owned().await {
            let c = client.clone();
            let p_ref = prefixes.clone();
            let s_ref = success_counter.clone();
            let p_name = proxy_name.clone();
            let mut s_rx = shutdown_rx.clone();
            
            tasks.push(tokio::spawn(async move {
                let _p = permit; // Hold permit until done
                
                // Double check shutdown
                if *s_rx.borrow() { return; }

                // Generate Number
                let prefix = p_ref.choose(&mut rand::rng()).unwrap();
                let phone = format!("98{}{}", prefix, generate_random_suffix());

                // Process
                perform_invite(worker_id, &p_name, &c, phone, &s_ref, debug_mode, target).await;
            }));
        } else {
            break; // Semaphore closed
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
) {
    if *success_counter.lock().await >= target { return; }

    let data = InviteData { application_name: "NGMI".to_string(), friend_number: phone.clone() };

    let res1 = client.post(API_CHECK_APP)
        .header("Referer", "https://my.irancell.ir/invite")
        .json(&data).send().await;

    match res1 {
        Ok(resp) => {
            if resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                if let Ok(body) = serde_json::from_str::<Value>(&text) {
                     if body["message"] == "done" {
                        let res2 = client.post(API_SEND_INVITE)
                            .header("Referer", "https://my.irancell.ir/invite/confirm")
                            .json(&data).send().await;

                        match res2 {
                            Ok(resp2) => {
                                if resp2.status().is_success() {
                                    let text2 = resp2.text().await.unwrap_or_default();
                                    if let Ok(body2) = serde_json::from_str::<Value>(&text2) {
                                        if body2["message"] == "done" {
                                            let mut lock = success_counter.lock().await;
                                            *lock += 1;
                                            println!("✅ Worker #{} | Proxy {} | Sent: {} ({}/{})", id, proxy_name, phone, *lock, target);
                                        } else if debug_mode { 
                                            log_debug(format!("Worker {} Step 2 Fail: {}", id, text2)).await; 
                                        }
                                    }
                                }
                            },
                            Err(e) => { 
                                if debug_mode { log_debug(format!("Worker {} Step 2 Net: {}", id, e)).await; } 
                            }
                        }
                    }
                }
            } else {
                // If specific HTTP errors occur, proxy might be banned
                if debug_mode { log_debug(format!("Worker {} HTTP Error {}: {}", id, resp.status(), phone)).await; }
            }
        },
        Err(e) => { 
            // This logs why a previously good proxy failed
            if debug_mode { log_debug(format!("Worker {} Net Error [{}]: {}", id, proxy_name, e)).await; } 
        }
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    // Config
    let config = read_config()?;
    let debug_mode = config.debug.unwrap_or(false);
    if debug_mode {
        println!("🐞 Debug Mode ON. Errors -> '{}', Proxy Failures -> '{}'", LOG_FILE, PROXY_ERROR_FILE);
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
    println!("2) Auto Proxy Mode (GitHub) 🚀");
    println!("3) Local Proxy Mode (socks5.txt) 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    let mode = match mode_input.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    // Important: Concurrency PER WORKER
    let concurrent_input = prompt_input("⚡ Requests PER PROXY (simultaneous): ");
    let requests_per_proxy: usize = concurrent_input.parse().unwrap_or(5);

    // --- PREPARE CLIENTS (Each client is tied to ONE proxy) ---
    // Type: Vec<(String, Client)> -> (ProxyName, ClientObject)
    let mut workers_setup: Vec<(String, Client)> = Vec::new();

    if mode == RunMode::Direct {
        println!("🚀 Building Direct Client...");
        let c = build_client(&token, None, 15)?;
        // For Direct mode, we treat "Direct" as a single "Proxy"
        workers_setup.push(("Direct".to_string(), c));
    } else if mode == RunMode::LocalProxy {
        workers_setup = read_local_strict(&token)?;
    } else if mode == RunMode::AutoProxy {
        workers_setup = fetch_and_validate_strict(token.clone()).await?;
    }

    if workers_setup.is_empty() {
        return Err(anyhow!("❌ No valid workers could be initialized. Check proxy_errors.log"));
    }

    let active_workers = workers_setup.len();
    println!("🚀 Launching {} workers.", active_workers);
    println!("🔥 Total theoretical concurrency: {} req/sec", active_workers * requests_per_proxy);

    // --- MAIN EXECUTION ---
    let success_counter = Arc::new(Mutex::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let prefixes = Arc::new(config.prefixes);
    
    let mut worker_handles = Vec::new();

    // Spawn 1 Worker Task per Valid Proxy
    for (id, (p_name, client)) in workers_setup.into_iter().enumerate() {
        let p_ref = prefixes.clone();
        let s_ref = success_counter.clone();
        let rx = shutdown_rx.clone();
        
        worker_handles.push(tokio::spawn(async move {
            run_worker_with_proxy(
                id,
                p_name,
                client,
                requests_per_proxy,
                p_ref,
                s_ref,
                target_count,
                rx,
                debug_mode
            ).await;
        }));
    }

    // Monitor success count in main thread to trigger shutdown
    let monitor_success = success_counter.clone();
    let monitor_tx = shutdown_tx.clone();
    
    // Blocking loop to check status
    loop {
        sleep(Duration::from_secs(1)).await;
        let count = *monitor_success.lock().await;
        
        if count >= target_count {
            let _ = monitor_tx.send(true);
            break;
        }
        // Optional: Print status every few seconds
        // println!("Status: {}/{}", count, target_count);
    }

    // Wait for workers to wind down
    println!("🏁 Target reached. Waiting for pending requests...");
    sleep(Duration::from_secs(2)).await;

    println!("\n📊 Final Results: {} Successes", *success_counter.lock().await);
    Ok(())
}
