use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::seq::SliceRandom;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tokio::time::sleep;

// --- Constants ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const TARGET_CHECK_URL: &str = "https://my.irancell.ir/invite"; 
const MAX_CONCURRENT_CHECKS: usize = 300; // Increased for speed
const PROXY_CHECK_TIMEOUT: u64 = 6; 

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
    let mut rng = rand::thread_rng();
    chars.choose_multiple(&mut rng, 7).collect()
}

fn prompt_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer).unwrap();
    buffer.trim().to_string()
}

async fn log_to_file(msg: String) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_line = format!("[{}] {}\n", timestamp, msg);
    let result = OpenOptions::new().create(true).append(true).open(LOG_FILE);
    if let Ok(mut file) = result { let _ = file.write_all(log_line.as_bytes()); }
}

fn sanitize_proxy_url(raw: &str) -> String {
    let mut clean = raw.trim().to_string();
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5://"); }
    if !clean.contains("://") { return format!("socks5://{}", clean); }
    clean
}

// --- CORE OPTIMIZATION: Build Client ONCE ---

/// Builds a ready-to-use client for a specific proxy
fn build_proxy_client(proxy_addr: &str, token: &str) -> Option<Client> {
    let proxy_str = sanitize_proxy_url(proxy_addr);
    let proxy = match Proxy::all(&proxy_str) {
        Ok(p) => p,
        Err(_) => return None,
    };

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    headers.insert("Accept-Encoding", "gzip, deflate, br, zstd".parse().unwrap());
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    if let Ok(val) = reqwest::header::HeaderValue::from_str(token) {
        headers.insert("Authorization", val);
    }

    Client::builder()
        .default_headers(headers)
        .proxy(proxy)
        .timeout(Duration::from_secs(15)) // Request timeout
        .pool_idle_timeout(Duration::from_secs(90)) // Keep connections alive!
        .build()
        .ok()
}

/// Downloads, Validates, AND BUILDS clients concurrently
async fn fetch_validate_and_build(token: String) -> Result<Vec<Client>> {
    println!("⏳ Initializing download...");
    let client = Client::builder().timeout(Duration::from_secs(30)).build()?;

    // 1. Download Source List
    println!("📄 Getting sources from GitHub...");
    let sources_text = client.get(SOURCE_OF_SOURCES_URL).send().await?.text().await?;
    let source_urls: Vec<String> = sources_text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.starts_with("http"))
        .collect();

    // 2. Download Raw Proxies
    println!("🔗 Found {} sources. Downloading raw list...", source_urls.len());
    let mut raw_proxies = HashSet::new();
    let mut tasks = Vec::new();

    for url in source_urls {
        let c = client.clone();
        let u = url.clone();
        tasks.push(tokio::spawn(async move {
            if let Ok(resp) = c.get(&u).timeout(Duration::from_secs(15)).send().await {
                if let Ok(text) = resp.text().await {
                    return Some(text);
                }
            }
            None
        }));
    }

    for task in tasks {
        if let Ok(Some(text)) = task.await {
            for line in text.lines() {
                let p = line.trim();
                if !p.is_empty() && (p.contains(':') || p.contains("://")) {
                    raw_proxies.insert(p.to_string());
                }
            }
        }
    }

    let total_raw = raw_proxies.len();
    println!("📦 Collected {} raw proxies. Starting BULK build & check...", total_raw);
    
    // 3. Bulk Build & Check
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKS));
    let valid_clients = Arc::new(Mutex::new(Vec::new()));
    let mut check_tasks = Vec::new();

    for proxy_str in raw_proxies {
        let sem = semaphore.clone();
        let valid_list = valid_clients.clone();
        let t_token = token.clone();
        
        let t = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            
            // Try to build client
            if let Some(client) = build_proxy_client(&proxy_str, &t_token) {
                // Test connectivity with the BUILT client
                if client.head(TARGET_CHECK_URL).timeout(Duration::from_secs(PROXY_CHECK_TIMEOUT)).send().await.is_ok() {
                    valid_list.lock().await.push(client);
                    print!("."); 
                    let _ = io::stdout().flush();
                }
            }
        });
        check_tasks.push(t);
    }

    for t in check_tasks { let _ = t.await; }

    let final_list = valid_clients.lock().await.drain(..).collect(); // Move out
    println!("\n✅ Process Complete. Pool size: {} clients.", total_raw); // Count fixed in print below
    
    Ok(final_list)
}

fn read_local_proxies(token: &str) -> Result<Vec<Client>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let mut clients = Vec::new();
    
    println!("📁 Reading and building local proxies...");
    for line in reader.lines() {
        if let Ok(l) = line {
            let trimmed = l.trim().to_string();
            if !trimmed.is_empty() { 
                if let Some(c) = build_proxy_client(&trimmed, token) {
                    clients.push(c);
                }
            }
        }
    }
    Ok(clients)
}

fn build_direct_client(token: &str) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    headers.insert("Accept-Encoding", "gzip, deflate, br, zstd".parse().unwrap());
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse().unwrap());

    Client::builder().default_headers(headers).timeout(Duration::from_secs(15)).build().context("Failed build direct")
}

async fn process_number(
    client: &Client,
    phone: String,
    success_counter: &Arc<Mutex<usize>>,
    debug_mode: bool,
) {
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
                                            println!("✅ Invite sent: {}", phone);
                                            let mut lock = success_counter.lock().await;
                                            *lock += 1;
                                        } else if debug_mode { log_to_file(format!("❌ [Step 2] {}: {}", phone, text2)).await; }
                                    }
                                }
                            },
                            Err(e) => { if debug_mode { log_to_file(format!("❌ [Step 2 Net] {}: {}", phone, e)).await; } }
                        }
                    }
                }
            } else {
                println!("❌ Request Failed {}: {}", resp.status(), phone);
                if debug_mode { log_to_file(format!("❌ [HTTP {}] {}", resp.status(), phone)).await; }
            }
        },
        Err(e) => { if debug_mode { log_to_file(format!("❌ [Step 1 Net] {}: {}", phone, e)).await; } }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(windows)]
    let _ = yansi::Paint::enable_windows_ascii();

    let config = read_config()?;
    let debug_mode = config.debug.unwrap_or(false);

    if debug_mode {
        println!("🐞 Debug Mode: ON");
        log_to_file("--- SESSION STARTED ---".to_string()).await;
    }

    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Loaded {} prefixes.", config.prefixes.len());
    let count_input = prompt_input("🎯 Target Count: ");
    let target_count: usize = count_input.parse().unwrap_or(1000);

    println!("\n✨ Select Mode:");
    println!("1) Direct Mode 🌐");
    println!("2) Auto Proxy Mode (Client Pool) 🚀");
    println!("3) Local Proxy Mode 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    let mode = match mode_input.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    let workers_input = prompt_input("👷 Number of Workers (Suggested: 50+): ");
    let worker_count: usize = workers_input.parse().unwrap_or(50);

    // KEY CHANGE: Storing Clients, not Strings
    let client_pool: Arc<RwLock<Vec<Client>>> = Arc::new(RwLock::new(Vec::new()));

    if mode == RunMode::LocalProxy {
        let clients = read_local_proxies(&token)?;
        println!("📁 Built {} local clients.", clients.len());
        *client_pool.write().await = clients;
    } else if mode == RunMode::AutoProxy {
        let clients = fetch_validate_and_build(token.clone()).await?;
        if clients.is_empty() { return Err(anyhow!("0 Healthy proxies found!")); }
        println!("✅ Pool ready with {} clients.", clients.len());
        *client_pool.write().await = clients;

        // Background updater (Logic simplified for Client Pool)
        // Updating clients in background is tricky because we don't want to lock the pool too long.
        // For now, in this high-perf version, we skip complex hot-reloading to ensure max speed.
    }

    println!("🚀 Start processing...");
    let success_count = Arc::new(Mutex::new(0));
    let (tx, rx) = mpsc::channel::<String>(worker_count * 2);
    let shared_rx = Arc::new(Mutex::new(rx));
    let prefixes = Arc::new(config.prefixes);

    let prefixes_clone = prefixes.clone();
    tokio::spawn(async move {
        for _ in 0..target_count {
            let num_opt = {
                let mut rng = rand::thread_rng();
                if let Some(prefix) = prefixes_clone.choose(&mut rng) {
                    Some(format!("98{}{}", prefix, generate_random_suffix()))
                } else { None }
            };
            if let Some(num) = num_opt {
                if tx.send(num).await.is_err() { break; }
            }
        }
    });

    let direct_client_base = if mode == RunMode::Direct { Some(build_direct_client(&token)?) } else { None };
    let mut handles = Vec::new();

    for _ in 0..worker_count {
        let rx_clone = shared_rx.clone();
        let pool_clone = client_pool.clone();
        let success_clone = success_count.clone();
        let direct_client = direct_client_base.clone();
        let current_mode = mode;

        handles.push(tokio::spawn(async move {
            loop {
                let phone_opt = { let mut rx_guard = rx_clone.lock().await; rx_guard.recv().await };
                let phone = match phone_opt { Some(p) => p, None => break };

                if current_mode == RunMode::Direct {
                    if let Some(c) = &direct_client { process_number(c, phone, &success_clone, debug_mode).await; }
                } else {
                    // SUPER FAST SELECTION:
                    // Just pick a client reference. No building.
                    let client_opt = {
                        let pool = pool_clone.read().await;
                        if !pool.is_empty() {
                            let mut rng = rand::thread_rng();
                            pool.choose(&mut rng).cloned() // Clone the Client handle (cheap), not the connection
                        } else { None }
                    };

                    if let Some(client) = client_opt {
                         process_number(&client, phone, &success_clone, debug_mode).await;
                    } else {
                        sleep(Duration::from_millis(1000)).await;
                    }
                }
            }
        }));
    }

    for h in handles { let _ = h.await; }
    println!("\n📊 Final Results: {}", *success_count.lock().await);
    Ok(())
}
