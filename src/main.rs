use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::prelude::*; // Rand 0.9 syntax
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

// --- Constants (Exactly matching Python) ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";

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

/// Exact logic of Python's: ''.join(random.sample(chars, 7))
fn generate_random_suffix() -> String {
    let chars: Vec<char> = "1234567890".chars().collect();
    let mut rng = rand::rng(); 
    // choose_multiple guarantees unique elements, same as random.sample
    chars.choose_multiple(&mut rng, 7).into_iter().collect()
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

fn sanitize_proxy_url(raw: &str) -> String {
    let mut clean = raw.trim().to_string();
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5://"); }
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5://"); }
    if !clean.contains("://") { return format!("socks5://{}", clean); }
    clean
}

// --- Client Factory ---

fn build_client(token: &str, proxy: Option<Proxy>) -> Result<Client> {
    // Setting default headers exactly as in Python "header_has_app"
    // Note: Referer is dynamic and set per request.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    // reqwest handles Accept-Encoding automatically (gzip, br, etc.)
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse().unwrap());

    let mut builder = Client::builder()
        .default_headers(headers)
        .tcp_nodelay(true) // Optimization for speed
        .cookie_store(true) // Python aiohttp session stores cookies, so we must too
        .timeout(Duration::from_secs(20));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

// --- Proxy Fetching (NO CHECK as requested) ---

async fn fetch_proxies_no_check(token: String) -> Result<Vec<(String, Client)>> {
    println!("⏳ Downloading proxy list...");
    
    // Use system proxy/VPN to fetch list
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
    println!("📦 Found {} proxies. Building workers immediately (No Health Check)...", total);
    
    let mut clients = Vec::new();
    for proxy_raw in raw_proxies {
        let sanitized = sanitize_proxy_url(&proxy_raw);
        if let Ok(proxy_obj) = Proxy::all(&sanitized) {
            // Build client for this specific proxy
            if let Ok(client) = build_client(&token, Some(proxy_obj)) {
                clients.push((proxy_raw, client));
            }
        }
    }
    
    Ok(clients)
}

fn read_local_no_check(token: &str) -> Result<Vec<(String, Client)>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let mut clients = Vec::new();
    println!("📁 Processing local proxies (No Health Check)...");
    
    for line in reader.lines() {
        if let Ok(l) = line {
            let p_raw = l.trim().to_string();
            if !p_raw.is_empty() {
                let sanitized = sanitize_proxy_url(&p_raw);
                if let Ok(p_obj) = Proxy::all(&sanitized) {
                    if let Ok(c) = build_client(token, Some(p_obj)) {
                        clients.push((p_raw, c));
                    }
                }
            }
        }
    }
    Ok(clients)
}

// --- Worker Logic (Exact Python Replication) ---

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
    // This Semaphore mimics Python's `asyncio.Semaphore(10)` but per worker
    let sem = Arc::new(Semaphore::new(concurrency_limit));
    let mut tasks = Vec::new();

    loop {
        if *shutdown_rx.borrow() { break; }

        tasks.retain(|h: &tokio::task::JoinHandle<()>| !h.is_finished());

        // Acquire permit to respect concurrency limit
        if let Ok(permit) = sem.clone().acquire_owned().await {
            let c = client.clone();
            let p_ref = prefixes.clone();
            let s_ref = success_counter.clone();
            let p_name = proxy_name.clone();
            let mut s_rx = shutdown_rx.clone();
            
            tasks.push(tokio::spawn(async move {
                let _p = permit; // Hold permit
                if *s_rx.borrow() { return; }

                // Logic: 98 + prefix + 7 random digits
                let prefix = p_ref.choose(&mut rand::rng()).unwrap();
                let phone = format!("98{}{}", prefix, generate_random_suffix());

                perform_invite(worker_id, &p_name, &c, phone, &s_ref, debug_mode, target).await;
            }));
        } else {
            break; 
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
    // Check if we hit target
    if *success_counter.lock().await >= target { return; }

    let data = InviteData { application_name: "NGMI".to_string(), friend_number: phone.clone() };

    // --- STEP 1: Check App ---
    // Python: headers=header_has_app (Referer: .../invite)
    let res1 = client.post(API_CHECK_APP)
        .header("Referer", "https://my.irancell.ir/invite") 
        .json(&data).send().await;

    match res1 {
        Ok(resp) => {
            // Python: if res1.status == 200 ...
            if resp.status().as_u16() == 200 { 
                let text = resp.text().await.unwrap_or_default();
                if let Ok(body) = serde_json::from_str::<Value>(&text) {
                     // Python: ... and response1.get("message") == "done"
                     if body["message"] == "done" {
                        
                        // --- STEP 2: Send Invite ---
                        // Python: headers=header_invite_to_MyIransell (Referer: .../invite/confirm)
                        let res2 = client.post(API_SEND_INVITE)
                            .header("Referer", "https://my.irancell.ir/invite/confirm")
                            .json(&data).send().await;

                        match res2 {
                            Ok(resp2) => {
                                // Python: if res2.status == 200 ...
                                if resp2.status().as_u16() == 200 {
                                    let text2 = resp2.text().await.unwrap_or_default();
                                    if let Ok(body2) = serde_json::from_str::<Value>(&text2) {
                                        // Python: ... and response2.get("message") == "done"
                                        if body2["message"] == "done" {
                                            let mut lock = success_counter.lock().await;
                                            *lock += 1;
                                            println!("✅ Worker #{} | Proxy {} | Sent: {} ({}/{})", id, proxy_name, phone, *lock, target);
                                        } else if debug_mode { 
                                            log_debug(format!("Worker {} Step 2 Logic Fail: {}", id, text2)).await; 
                                        }
                                    }
                                } else if debug_mode {
                                    log_debug(format!("Worker {} Step 2 HTTP {}: {}", id, resp2.status(), phone)).await;
                                }
                            },
                            Err(e) => { 
                                if debug_mode { log_debug(format!("Worker {} Step 2 Net: {}", id, e)).await; } 
                            }
                        }
                    }
                }
            } else {
                if debug_mode { log_debug(format!("Worker {} HTTP Error {}: {}", id, resp.status(), phone)).await; }
            }
        },
        Err(e) => { 
            // Network error (Proxy dead, timeout, etc)
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
        println!("🐞 Debug Mode ON. Errors -> '{}'", LOG_FILE);
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
    println!("2) Auto Proxy Mode (GitHub - NO CHECK) 🚀");
    println!("3) Local Proxy Mode (socks5.txt - NO CHECK) 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    let mode = match mode_input.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    let concurrent_input = prompt_input("⚡ Requests PER PROXY (simultaneous): ");
    let requests_per_proxy: usize = concurrent_input.parse().unwrap_or(5);

    // --- PREPARE WORKERS ---
    let mut workers_setup: Vec<(String, Client)> = Vec::new();

    if mode == RunMode::Direct {
        println!("🚀 Building Direct Client...");
        let c = build_client(&token, None)?;
        workers_setup.push(("Direct".to_string(), c));
    } else if mode == RunMode::LocalProxy {
        workers_setup = read_local_no_check(&token)?;
    } else if mode == RunMode::AutoProxy {
        workers_setup = fetch_proxies_no_check(token.clone()).await?;
    }

    if workers_setup.is_empty() {
        return Err(anyhow!("❌ No workers initialized."));
    }

    let active_workers = workers_setup.len();
    println!("🚀 Launching {} workers immediately...", active_workers);

    // --- MAIN LOOP ---
    let success_counter = Arc::new(Mutex::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let prefixes = Arc::new(config.prefixes);
    
    // Launch threads
    for (id, (p_name, client)) in workers_setup.into_iter().enumerate() {
        let p_ref = prefixes.clone();
        let s_ref = success_counter.clone();
        let rx = shutdown_rx.clone();
        
        tokio::spawn(async move {
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
        });
    }

    let monitor_success = success_counter.clone();
    let monitor_tx = shutdown_tx.clone();
    
    loop {
        sleep(Duration::from_secs(1)).await;
        let count = *monitor_success.lock().await;
        if count >= target_count {
            let _ = monitor_tx.send(true); // Signal shutdown
            break;
        }
    }

    println!("🏁 Target reached. Cooling down...");
    sleep(Duration::from_secs(2)).await;

    println!("\n📊 Final Results: {} Successes", *success_counter.lock().await);
    Ok(())
}
