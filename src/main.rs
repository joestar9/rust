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
use tokio::sync::{mpsc, Mutex, RwLock, Semaphore, watch};
use tokio::time::sleep;

// --- Constants ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const TARGET_CHECK_URL: &str = "https://my.irancell.ir/invite"; 
const MAX_VALIDATION_CONCURRENCY: usize = 500;
const VALIDATION_TIMEOUT: u64 = 5;

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
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5://"); }
    
    if !clean.contains("://") { 
        return format!("socks5://{}", clean); 
    }
    
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
        .timeout(Duration::from_secs(timeout_secs));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

// --- Validation Logic ---

async fn fetch_validate_and_build(token: String) -> Result<Vec<Client>> {
    println!("⏳ Initializing download sequence...");
    
    let fetcher = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    println!("📄 Getting sources from GitHub...");
    let sources_text = fetcher.get(SOURCE_OF_SOURCES_URL).send().await?.text().await?;
    let source_urls: Vec<String> = sources_text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.starts_with("http"))
        .collect();

    println!("🔗 Found {} sources. Downloading raw list concurrently...", source_urls.len());
    
    let mut raw_proxies = HashSet::new();
    let mut download_tasks = Vec::new();

    for url in source_urls {
        let c = fetcher.clone();
        let u = url.clone();
        download_tasks.push(tokio::spawn(async move {
            if let Ok(resp) = c.get(&u).timeout(Duration::from_secs(10)).send().await {
                if let Ok(text) = resp.text().await {
                    return Some(text);
                }
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

    let total_raw = raw_proxies.len();
    println!("📦 Collected {} raw proxies. Starting HIGH-SPEED validation...", total_raw);
    
    let semaphore = Arc::new(Semaphore::new(MAX_VALIDATION_CONCURRENCY));
    let valid_clients = Arc::new(Mutex::new(Vec::new()));
    let mut check_tasks = Vec::new();

    for proxy_str in raw_proxies {
        let sem = semaphore.clone();
        let valid_list = valid_clients.clone();
        let t_token = token.clone();
        
        check_tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            
            let sanitized = sanitize_proxy_url(&proxy_str);
            if let Ok(proxy_obj) = Proxy::all(&sanitized) {
                if let Ok(test_client) = build_client(&t_token, Some(proxy_obj.clone()), VALIDATION_TIMEOUT) {
                    if test_client.get(TARGET_CHECK_URL).send().await.is_ok() {
                        if let Ok(prod_client) = build_client(&t_token, Some(proxy_obj), 15) {
                            valid_list.lock().await.push(prod_client);
                            print!("."); 
                            let _ = io::stdout().flush();
                        }
                    }
                }
            }
        }));
    }

    for t in check_tasks { let _ = t.await; }

    // ✅ FIX: Explicitly specify type Vec<Client>
    let final_list: Vec<Client> = valid_clients.lock().await.drain(..).collect();
    println!("\n✅ Process Complete. Pool size: {} clients.", final_list.len());
    
    Ok(final_list)
}

fn read_local_proxies(token: &str) -> Result<Vec<Client>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let mut clients = Vec::new();
    
    println!("📁 Processing local proxies...");
    for line in reader.lines() {
        if let Ok(l) = line {
            let trimmed = l.trim().to_string();
            if !trimmed.is_empty() { 
                let sanitized = sanitize_proxy_url(&trimmed);
                if let Ok(proxy) = Proxy::all(&sanitized) {
                    if let Ok(client) = build_client(token, Some(proxy), 15) {
                        clients.push(client);
                    }
                }
            }
        }
    }
    Ok(clients)
}

// --- Core API Logic ---

async fn process_number(
    client: &Client,
    phone: String,
    success_counter: &Arc<Mutex<usize>>,
    debug_mode: bool,
    target: usize,
    shutdown_tx: &watch::Sender<bool>,
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
                                            let current = *lock;
                                            println!("✅ Invite sent: {} ({}/{})", phone, current, target);
                                            
                                            if current >= target {
                                                println!("🎉 Target reached! Stopping all workers...");
                                                let _ = shutdown_tx.send(true);
                                            }
                                        } else if debug_mode { 
                                            log_to_file(format!("❌ [Step 2 Fail] {}: {}", phone, text2)).await; 
                                        }
                                    }
                                }
                            },
                            Err(e) => { 
                                if debug_mode { log_to_file(format!("❌ [Step 2 Net] {}: {}", phone, e)).await; } 
                            }
                        }
                    }
                }
            } else {
                println!("❌ Request Failed {}: {}", resp.status(), phone);
                if debug_mode { log_to_file(format!("❌ [HTTP {}] {}", resp.status(), phone)).await; }
            }
        },
        Err(e) => { 
            if debug_mode { log_to_file(format!("❌ [Step 1 Net] {}: {}", phone, e)).await; } 
        }
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
    let count_input = prompt_input("🎯 Target SUCCESS Count: ");
    let target_count: usize = count_input.parse().unwrap_or(1000);

    println!("\n✨ Select Mode:");
    println!("1) Direct Mode (Maximum Speed) 🌐");
    println!("2) Auto Proxy Mode (Client Pool) 🚀");
    println!("3) Local Proxy Mode 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    let mode = match mode_input.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    let workers_input = prompt_input("👷 Concurrent Limit (Simultaneous Requests): ");
    let worker_count: usize = workers_input.parse().unwrap_or(100);

    let client_pool: Arc<RwLock<Vec<Client>>> = Arc::new(RwLock::new(Vec::new()));
    let direct_client_ref: Arc<RwLock<Option<Client>>> = Arc::new(RwLock::new(None));

    if mode == RunMode::Direct {
        println!("🚀 Building Optimized Direct Client...");
        let c = build_client(&token, None, 15)?;
        *direct_client_ref.write().await = Some(c);
    } else if mode == RunMode::LocalProxy {
        let clients = read_local_proxies(&token)?;
        println!("📁 Built {} local clients.", clients.len());
        *client_pool.write().await = clients;
    } else if mode == RunMode::AutoProxy {
        let clients = fetch_validate_and_build(token.clone()).await?;
        if clients.is_empty() { return Err(anyhow!("0 Healthy proxies found!")); }
        *client_pool.write().await = clients;
    }

    println!("🚀 Start processing until {} successes...", target_count);
    let success_count = Arc::new(Mutex::new(0));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    
    if mode == RunMode::Direct {
        let semaphore = Arc::new(Semaphore::new(worker_count));
        let prefixes = Arc::new(config.prefixes);
        let client = direct_client_ref.read().await.as_ref().unwrap().clone();
        
        let mut tasks = Vec::new();
        
        loop {
            if *shutdown_rx.borrow() { break; }
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            
            let client_clone = client.clone();
            let succ_clone = success_count.clone();
            let prefix = prefixes.choose(&mut rand::thread_rng()).unwrap().clone();
            let shutdown_clone = shutdown_tx.clone();
            
            let t = tokio::spawn(async move {
                let _p = permit;
                let phone = format!("98{}{}", prefix, generate_random_suffix());
                process_number(&client_clone, phone, &succ_clone, debug_mode, target_count, &shutdown_clone).await;
            });
            tasks.push(t);
             if tasks.len() > 20000 { tasks.retain(|h| !h.is_finished()); }
        }
    } 
    else {
        let (tx, rx) = mpsc::channel::<String>(worker_count * 4);
        let shared_rx = Arc::new(Mutex::new(rx));
        let prefixes = Arc::new(config.prefixes);
        
        let prefixes_clone = prefixes.clone();
        // ✅ FIX: Removed 'mut'
        let gen_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                if *gen_rx.borrow() { break; }
                let prefix = prefixes_clone.choose(&mut rand::thread_rng()).unwrap();
                let num = format!("98{}{}", prefix, generate_random_suffix());
                if tx.send(num).await.is_err() { break; }
            }
        });

        let mut handles = Vec::new();
        for _ in 0..worker_count {
            let rx_clone = shared_rx.clone();
            let pool_clone = client_pool.clone();
            let succ_clone = success_count.clone();
            let shutdown_clone = shutdown_tx.clone();
            // ✅ FIX: Removed 'mut'
            let w_rx = shutdown_rx.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    if *w_rx.borrow() { break; }
                    let phone_opt = { let mut g = rx_clone.lock().await; g.recv().await };
                    let phone = match phone_opt { Some(p) => p, None => break };

                    let client_opt = {
                        let pool = pool_clone.read().await;
                        if !pool.is_empty() {
                            let mut rng = rand::thread_rng();
                            pool.choose(&mut rng).cloned()
                        } else { None }
                    };

                    if let Some(c) = client_opt {
                        process_number(&c, phone, &succ_clone, debug_mode, target_count, &shutdown_clone).await;
                    } else {
                        sleep(Duration::from_millis(1000)).await;
                    }
                }
            }));
        }
        for h in handles { let _ = h.await; }
    }

    if mode != RunMode::Direct {
         let _ = shutdown_rx.changed().await;
    }
    
    sleep(Duration::from_millis(500)).await;
    println!("\n📊 Final Results: {} Successes", *success_count.lock().await);
    Ok(())
}
