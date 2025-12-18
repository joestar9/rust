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
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.json";
const MAX_RETRIES_BEFORE_SWITCH: u8 = 3; 

// --- Enums & Structs ---
#[derive(Clone, Copy, PartialEq)]
enum RunMode {
    Direct,
    AutoProxy,
    LocalProxy,
}

#[derive(Clone, Copy, PartialEq)]
enum ProxyFilter {
    All,
    Http,
    Socks4,
    Socks5,
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

#[derive(Deserialize, Debug)]
struct ProxySourceConfig {
    #[serde(default)]
    http: Vec<String>,
    #[serde(default)]
    socks4: Vec<String>,
    #[serde(default)]
    socks5: Vec<String>,
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

fn prompt_yes_no(prompt: &str, default_yes: bool) -> bool {
    let input = prompt_input(prompt);
    let s = input.trim().to_lowercase();
    if s.is_empty() { return default_yes; }
    match s.as_str() {
        "y" | "yes" | "1" | "true" | "t" => true,
        "n" | "no" | "0" | "false" | "f" => false,
        _ => default_yes,
    }
}


async fn log_debug(msg: String) {
    let timestamp = Local::now().format("%H:%M:%S");
    let log_line = format!("[{}] {}\n", timestamp, msg);
    let result = OpenOptions::new().create(true).append(true).open(LOG_FILE);
    if let Ok(mut file) = result { let _ = file.write_all(log_line.as_bytes()); }
}

/// ✅ UPDATED: Smart Protocol Detection & Port Heuristics
fn format_proxy_url(raw: &str, default_proto: &str) -> String {
    let mut clean = raw.trim().to_string();
    
    // 1. Fix common typos
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5://"); }
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5://"); }
    
    // 2. Optimization: Upgrade socks5 to socks5h (Remote DNS)
    if default_proto == "socks5" || default_proto == "socks5h" {
        if clean.starts_with("socks5://") {
            return clean.replace("socks5://", "socks5h://");
        }
        if !clean.contains("://") {
            return format!("socks5h://{}", clean);
        }
    }

    // 3. Heuristic for HTTP vs HTTPS based on Port
    // If user selected HTTP mode but no scheme is present, check port.
    if default_proto == "http" && !clean.contains("://") {
        if let Some(port_str) = clean.split(':').last() {
            if let Ok(port) = port_str.parse::<u16>() {
                // Ports commonly used for HTTPS/SSL Proxies
                if [443, 8443, 2053, 2083, 2087, 2096].contains(&port) {
                    return format!("https://{}", clean);
                }
            }
        }
        // Fallback to standard http
        return format!("http://{}", clean);
    }

    // 4. Default Fallback
    if !clean.contains("://") { 
        return format!("{}://{}", default_proto, clean); 
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
        .timeout(Duration::from_secs(15));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

// --- Fetch Logic ---

async fn fetch_proxies_list(_token: String, filter: ProxyFilter) -> Result<Vec<String>> {
    println!("⏳ Connecting to GitHub to fetch proxy sources...");
    let fetcher = Client::builder().timeout(Duration::from_secs(30)).build()?;
    
    let json_text = match fetcher.get(SOURCE_OF_SOURCES_URL).send().await {
        Ok(res) => res.text().await?,
        Err(e) => return Err(anyhow!("Failed to fetch source JSON: {}", e)),
    };

    let sources: ProxySourceConfig = match serde_json::from_str(&json_text) {
        Ok(s) => s,
        Err(e) => return Err(anyhow!("Invalid JSON format in source file: {}", e)),
    };

    let mut raw_proxies = HashSet::new();
    let mut tasks = Vec::new();

    let spawn_download = |url: String, proto: &'static str, client: Client| {
        tokio::spawn(async move {
            let mut found = Vec::new();
            if let Ok(resp) = client.get(&url).timeout(Duration::from_secs(15)).send().await {
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

    match filter {
        ProxyFilter::All => {
            println!("🌐 Fetching ALL proxy types...");
            for url in sources.http { tasks.push(spawn_download(url, "http", fetcher.clone())); }
            for url in sources.socks4 { tasks.push(spawn_download(url, "socks4", fetcher.clone())); }
            for url in sources.socks5 { tasks.push(spawn_download(url, "socks5h", fetcher.clone())); }
        },
        ProxyFilter::Http => {
            println!("🌐 Fetching ONLY HTTP/HTTPS proxies...");
            for url in sources.http { tasks.push(spawn_download(url, "http", fetcher.clone())); }
        },
        ProxyFilter::Socks4 => {
            println!("🌐 Fetching ONLY SOCKS4 proxies...");
            for url in sources.socks4 { tasks.push(spawn_download(url, "socks4", fetcher.clone())); }
        },
        ProxyFilter::Socks5 => {
            println!("🌐 Fetching ONLY SOCKS5 proxies...");
            for url in sources.socks5 { tasks.push(spawn_download(url, "socks5h", fetcher.clone())); }
        },
    }

    if tasks.is_empty() {
        return Err(anyhow!("No sources found for the selected type."));
    }

    println!("⬇️  Downloading from {} sources...", tasks.len());
    
    for task in tasks {
        if let Ok(proxies) = task.await {
            for p in proxies {
                raw_proxies.insert(p); 
            }
        }
    }
    
    let mut final_list: Vec<String> = raw_proxies.into_iter().collect();
    final_list.shuffle(&mut rand::rng());
    Ok(final_list)
}

fn read_local_list(file_path: &str, default_proto: &str) -> Result<Vec<String>> {
    let clean_path = file_path.trim().trim_matches('"').trim_matches('\'');
    
    let file = File::open(clean_path).context(format!("Could not open file: {}", clean_path))?;
    let reader = BufReader::new(file);
    let mut unique_set = HashSet::new();
    println!("📁 Processing local proxies from '{}'...", clean_path);
    
    for line in reader.lines() {
        if let Ok(l) = line {
            let p = l.trim().to_string();
            if !p.is_empty() { 
                unique_set.insert(format_proxy_url(&p, default_proto)); 
            }
        }
    }
    
    println!("🧹 Local List: Loaded {} unique proxies.", unique_set.len());
    let mut proxies: Vec<String> = unique_set.into_iter().collect();
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
    debug_mode: bool,
    use_send_invite: bool
) {
    let (status_tx, mut status_rx) = mpsc::channel::<ProxyStatus>(concurrency_limit + 10);
    let sem = Arc::new(Semaphore::new(concurrency_limit));

    let mut current_client: Option<Client> = None;
    let mut current_proxy_addr: String = String::new();
    let mut consecutive_errors: u8 = 0;

    loop {
        if *shutdown_rx.borrow() { break; }

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

        if consecutive_errors >= MAX_RETRIES_BEFORE_SWITCH {
            if debug_mode { 
                log_debug(format!("♻️ Worker {} switching. Failed: {}", worker_id, current_proxy_addr)).await; 
            }
            current_client = None; 
            consecutive_errors = 0; 
        }

        if current_client.is_none() {
            let mut pool = proxy_pool.lock().await;
            if let Some(proxy_url) = pool.pop() {
                // proxy_url is already formatted correctly
                if let Ok(proxy_obj) = Proxy::all(&proxy_url) {
                    if let Ok(c) = build_client(&token, Some(proxy_obj)) {
                        current_client = Some(c);
                        current_proxy_addr = proxy_url;
                        consecutive_errors = 0;
                    }
                }
            } else {
                drop(pool);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        }

        if let Some(client) = current_client.clone() {
            if let Ok(permit) = sem.clone().acquire_owned().await {
                let p_ref = prefixes.clone();
                let s_ref = success_counter.clone();
                let p_addr = current_proxy_addr.clone();
                let tx = status_tx.clone();
                let s_rx = shutdown_rx.clone();
                let use_send = use_send_invite;
                
                tokio::spawn(async move {
                    let _p = permit; 
                    if *s_rx.borrow() { return; } 

                    let prefix = p_ref.choose(&mut rand::rng()).unwrap();
                    let phone = format!("98{}{}", prefix, generate_random_suffix());

                    let status = perform_invite(worker_id, &p_addr, &client, phone, &s_ref, debug_mode, target, use_send).await;
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
    target: usize,
    use_send_invite: bool
) -> ProxyStatus {
    if *success_counter.lock().await >= target { return ProxyStatus::Healthy; }

    let data = InviteData { application_name: "NGMI".to_string(), friend_number: phone.clone() };

    let res1 = client.post(API_CHECK_APP)
        .header("Referer", "https://my.irancell.ir/invite")
        .json(&data).send().await;

    match res1 {
        Ok(resp) => {
            let status_code = resp.status();
            
            if status_code == 403 || status_code == 429 || status_code == 407 {
                return ProxyStatus::HardFail; 
            }

            if status_code.as_u16() == 200 {
                let text = resp.text().await.unwrap_or_default();
                if let Ok(body) = serde_json::from_str::<Value>(&text) {
                     if body["message"] == "done" {
                        if !use_send_invite {
                            let mut lock = success_counter.lock().await;
                            *lock += 1;
                            println!("✅ Worker #{} | Proxy {} | Sent: {} ({}/{})", id, proxy_name, phone, *lock, target);
                            return ProxyStatus::Healthy;
                        }

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

    let use_send_invite = prompt_yes_no("📩 Use api_send_invite (notify) after api_phone_has_app? [y/N]: ", false);

    println!("\n✨ Select Mode:");
    println!("1) Direct Mode (No Proxy) 🌐");
    println!("2) Auto Proxy Mode (JSON Sources) 🚀");
    println!("3) Local Proxy Mode (File) 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    
    let mut proxy_filter = ProxyFilter::All;
    let mut default_local_proto = "socks5h"; 
    let mut local_file_path = String::from("socks5.txt"); 

    let mode = match mode_input.as_str() {
        "2" => {
            println!("\n🔍 Select Proxy Protocol to Download:");
            println!("1) All (Default) 🌍");
            println!("2) HTTP/HTTPS 🌐");
            println!("3) SOCKS4 🔌");
            println!("4) SOCKS5 (Remote DNS) 🛡️");
            
            let filter_input = prompt_input("Choice [1-4]: ");
            proxy_filter = match filter_input.as_str() {
                "2" => ProxyFilter::Http,
                "3" => ProxyFilter::Socks4,
                "4" => ProxyFilter::Socks5,
                _ => ProxyFilter::All,
            };
            RunMode::AutoProxy
        },
        "3" => {
            let path_input = prompt_input("📁 Enter Proxy File Path (Drag & Drop): ");
            if !path_input.trim().is_empty() {
                local_file_path = path_input;
            }

            println!("\n🔍 Select Default Protocol for Local File:");
            println!("1) Mixed/Auto (Respect schemes, default to SOCKS5) ⚡");
            println!("2) HTTP/HTTPS 🌐");
            println!("3) SOCKS4 🔌");
            println!("4) SOCKS5 (Remote DNS) 🛡️");
            
            let filter_input = prompt_input("Choice [1-4]: ");
            default_local_proto = match filter_input.as_str() {
                "2" => "http",
                "3" => "socks4",
                "4" => "socks5h",
                _ => "socks5h",
            };
            RunMode::LocalProxy
        },
        _ => RunMode::Direct,
    };

    let concurrent_input = prompt_input("⚡ Requests PER PROXY (simultaneous): ");
    let requests_per_proxy: usize = concurrent_input.parse().unwrap_or(5);

    let workers_input = prompt_input("👷 Total Worker Threads: ");
    let worker_count: usize = workers_input.parse().unwrap_or(50);

    // --- PREPARE POOL ---
    let mut raw_pool: Vec<String> = Vec::new();

    if mode == RunMode::LocalProxy {
        raw_pool = read_local_list(&local_file_path, default_local_proto)?;
        println!("📦 Loaded {} local proxies.", raw_pool.len());
    } else if mode == RunMode::AutoProxy {
        raw_pool = fetch_proxies_list(token.clone(), proxy_filter).await?;
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
        let use_send_invite_flag = use_send_invite;
        
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
                            perform_invite(id, "DIRECT", &c, phone, &sr, debug_mode, target_count, use_send_invite_flag).await;
                        });
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                run_worker_robust(id, pool, token_clone, requests_per_proxy, p_ref, s_ref, target_count, rx, debug_mode, use_send_invite_flag).await;
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
