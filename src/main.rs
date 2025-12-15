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
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::sleep;

// --- Constants ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const TARGET_CHECK_URL: &str = "https://my.irancell.ir/invite"; 

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
    let config: AppConfig = serde_json::from_reader(reader)
        .context("❌ Failed to parse config.json.")?;
    
    if config.prefixes.is_empty() {
        return Err(anyhow!("❌ The 'prefixes' list cannot be empty!"));
    }
    
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
    
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE);

    if let Ok(mut file) = result {
        let _ = file.write_all(log_line.as_bytes());
    }
}

// --- Proxy Logic ---

async fn fetch_all_proxies_from_sources() -> Result<Vec<String>> {
    println!("⏳ Initializing HTTP Client (System Proxy/VPN)...");
    
    let client = Client::builder()
        .timeout(Duration::from_secs(30)) 
        .build()?;

    println!("📄 Connecting to GitHub: {}", SOURCE_OF_SOURCES_URL);
    io::stdout().flush()?;

    let sources_text = match client.get(SOURCE_OF_SOURCES_URL).send().await {
        Ok(resp) => {
            println!("✅ Connected to GitHub. Reading list...");
            resp.text().await?
        },
        Err(e) => {
            println!("❌ Error connecting to GitHub: {}", e);
            println!("💡 Make sure your System VPN is ON.");
            return Err(anyhow::Error::new(e));
        }
    };

    let source_urls: Vec<String> = sources_text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.starts_with("http"))
        .collect();

    println!("🔗 Found {} sources. Downloading proxies...", source_urls.len());

    let mut all_proxies = HashSet::new();
    let mut tasks = Vec::new();

    for url in source_urls {
        let c = client.clone();
        let u = url.clone();
        let task = tokio::spawn(async move {
            match c.get(&u).timeout(Duration::from_secs(15)).send().await {
                Ok(resp) => {
                    if let Ok(text) = resp.text().await {
                        let lines: Vec<String> = text.lines()
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect();
                        return Some(lines);
                    }
                }
                Err(_) => {}
            }
            None
        });
        tasks.push(task);
    }

    println!("⏳ Waiting for downloads...");
    for task in tasks {
        if let Ok(Some(proxies)) = task.await {
            for p in proxies {
                all_proxies.insert(p);
            }
        }
    }

    Ok(all_proxies.into_iter().collect())
}

fn read_local_proxies() -> Result<Vec<String>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let mut unique_proxies = HashSet::new();
    for line in reader.lines() {
        if let Ok(l) = line {
            let trimmed = l.trim().to_string();
            if !trimmed.is_empty() {
                unique_proxies.insert(trimmed);
            }
        }
    }
    Ok(unique_proxies.into_iter().collect())
}

/// Helper function to fix and format proxy strings
fn sanitize_proxy_url(raw: &str) -> String {
    let mut clean = raw.trim().to_string();
    
    // Fix common typo "soks5" -> "socks5"
    if clean.starts_with("soks5://") {
        clean = clean.replace("soks5://", "socks5://");
    }

    // If it has no scheme (e.g. "1.1.1.1:80"), assume socks5
    if !clean.contains("://") {
        return format!("socks5://{}", clean);
    }

    clean
}

async fn build_client_with_proxy(proxy_addr: &str, token: &str, debug: bool) -> Option<Client> {
    // 1. Sanitize the proxy URL
    let proxy_str = sanitize_proxy_url(proxy_addr);

    if debug {
        // Print purely for debugging logic, remove in prod if too noisy
        // println!("🔍 Testing Proxy: {}", proxy_str);
    }

    let proxy = match Proxy::all(&proxy_str) {
        Ok(p) => p,
        Err(e) => {
            if debug { log_to_file(format!("❌ Bad Proxy Format [{}]: {}", proxy_str, e)).await; }
            return None;
        }
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

    // 2. Build Client with tight timeout
    let client_builder = Client::builder()
        .default_headers(headers)
        .proxy(proxy)
        .timeout(Duration::from_secs(6)); // 6 seconds is enough for a handshake

    match client_builder.build() {
        Ok(client) => {
            // 3. Health Check
            match client.get(TARGET_CHECK_URL).send().await {
                Ok(_) => {
                    // if debug { println!("✅ Proxy Healthy: {}", proxy_str); }
                    Some(client)
                },
                Err(_) => {
                    // if debug { println!("💀 Proxy Dead: {}", proxy_str); }
                    None
                }
            }
        }
        Err(_) => None,
    }
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

    Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(15))
        .build()
        .context("Failed to build direct client")
}

async fn process_number(
    client: &Client,
    phone: String,
    success_counter: &Arc<Mutex<usize>>,
    debug_mode: bool,
) {
    let data = InviteData {
        application_name: "NGMI".to_string(),
        friend_number: phone.clone(),
    };

    let res1 = client
        .post(API_CHECK_APP)
        .header("Referer", "https://my.irancell.ir/invite")
        .json(&data)
        .send()
        .await;

    match res1 {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            
            if status.is_success() {
                match serde_json::from_str::<Value>(&text) {
                    Ok(body) => {
                         if body["message"] == "done" {
                            let res2 = client
                                .post(API_SEND_INVITE)
                                .header("Referer", "https://my.irancell.ir/invite/confirm")
                                .json(&data)
                                .send()
                                .await;

                            match res2 {
                                Ok(resp2) => {
                                    let status2 = resp2.status();
                                    let text2 = resp2.text().await.unwrap_or_default();
                                    
                                    if status2.is_success() {
                                        match serde_json::from_str::<Value>(&text2) {
                                            Ok(body2) => {
                                                if body2["message"] == "done" {
                                                    println!("✅ Invite sent: {}", phone);
                                                    let mut lock = success_counter.lock().await;
                                                    *lock += 1;
                                                } else if debug_mode {
                                                    log_to_file(format!("❌ [Step 2 Logic] {}: {}", phone, text2)).await;
                                                }
                                            },
                                            Err(_) => {
                                                if debug_mode { log_to_file(format!("❌ [Step 2 JSON] {}: {}", phone, text2)).await; }
                                            }
                                        }
                                    } else if debug_mode {
                                        log_to_file(format!("❌ [Step 2 HTTP {}] {}: {}", status2, phone, text2)).await;
                                    }
                                },
                                Err(e) => {
                                    if debug_mode { log_to_file(format!("❌ [Step 2 Net] {}: {}", phone, e)).await; }
                                },
                            }
                        }
                    },
                    Err(_) => {
                        if debug_mode { log_to_file(format!("❌ [Step 1 JSON] {}: {}", phone, text)).await; }
                    },
                }
            } else {
                println!("❌ Request Failed {}: {}", status, phone);
                if debug_mode {
                    log_to_file(format!("❌ [Step 1 HTTP {}] {}: {}", status, phone, text)).await;
                }
            }
        },
        Err(e) => {
            if debug_mode {
                log_to_file(format!("❌ [Step 1 Net] {}: {}", phone, e)).await;
            }
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
        println!("🐞 Debug Mode is ON. Logs: '{}'", LOG_FILE);
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
    println!("1) Direct Mode (Your IP/VPN) 🌐");
    println!("2) Auto Proxy Mode (GitHub Source) 🔄");
    println!("3) Local Proxy Mode (socks5.txt) 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    let mode = match mode_input.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    let workers_input = prompt_input("👷 Number of Workers: ");
    let worker_count: usize = workers_input.parse().unwrap_or(5);

    let proxy_list: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));

    if mode == RunMode::LocalProxy {
        let loaded = read_local_proxies()?;
        println!("📁 Loaded {} unique local proxies.", loaded.len());
        *proxy_list.write().await = loaded;
    } else if mode == RunMode::AutoProxy {
        println!("🚀 Starting Proxy Fetch Process...");
        match fetch_all_proxies_from_sources().await {
            Ok(proxies) => {
                if proxies.is_empty() {
                    return Err(anyhow!("No proxies found. Check VPN/Connection."));
                }
                println!("📥 Aggregated {} unique proxies.", proxies.len());
                *proxy_list.write().await = proxies;
            }
            Err(e) => {
                return Err(e);
            }
        }

        let p_list_clone = proxy_list.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(600)).await;
                if let Ok(new_proxies) = fetch_all_proxies_from_sources().await {
                    if !new_proxies.is_empty() {
                        println!("\n🔄 Auto-Update: {} proxies found.", new_proxies.len());
                        *p_list_clone.write().await = new_proxies;
                    }
                }
            }
        });
    }

    println!("🚀 Start processing {} numbers at: {}", target_count, Local::now().format("%c"));
    let success_count = Arc::new(Mutex::new(0));
    
    let (tx, rx) = mpsc::channel::<String>(100);
    let shared_rx = Arc::new(Mutex::new(rx));
    let prefixes = Arc::new(config.prefixes);

    let prefixes_clone = prefixes.clone();
    tokio::spawn(async move {
        for _ in 0..target_count {
            let num_opt = {
                let mut rng = rand::thread_rng();
                if let Some(prefix) = prefixes_clone.choose(&mut rng) {
                    Some(format!("98{}{}", prefix, generate_random_suffix()))
                } else {
                    None
                }
            };
            if let Some(num) = num_opt {
                if tx.send(num).await.is_err() { break; }
            }
        }
    });

    let direct_client_base = if mode == RunMode::Direct {
        Some(build_direct_client(&token)?)
    } else { None };

    let mut handles = Vec::new();

    for id in 0..worker_count {
        let rx_clone = shared_rx.clone();
        let proxies_clone = proxy_list.clone();
        let token_clone = token.clone();
        let success_clone = success_count.clone();
        let direct_client = direct_client_base.clone();
        let current_mode = mode;

        let handle = tokio::spawn(async move {
            loop {
                let phone_opt = {
                    let mut rx_guard = rx_clone.lock().await;
                    rx_guard.recv().await
                };

                let phone = match phone_opt {
                    Some(p) => p,
                    None => break,
                };

                if current_mode == RunMode::Direct {
                    if let Some(c) = &direct_client {
                        process_number(c, phone, &success_clone, debug_mode).await;
                    }
                } else {
                    let mut selected_client = None;
                    let mut attempt = 0;
                    // Retry up to 5 times to find a working proxy
                    while attempt < 5 {
                        let proxy_addr = {
                            let r_guard = proxies_clone.read().await;
                            if r_guard.is_empty() { None } 
                            else { 
                                let mut rng = rand::thread_rng();
                                r_guard.choose(&mut rng).cloned() 
                            }
                        };

                        if let Some(p) = proxy_addr {
                             // Pass debug_mode to build_client
                             if let Some(c) = build_client_with_proxy(&p, &token_clone, debug_mode).await {
                                selected_client = Some(c);
                                break;
                            }
                        } else {
                            sleep(Duration::from_millis(2000)).await;
                            break;
                        }
                        attempt += 1;
                    }

                    if let Some(client) = selected_client {
                        process_number(&client, phone, &success_clone, debug_mode).await;
                    } else if debug_mode {
                        log_to_file(format!("⚠️ [Worker {}] Failed to find healthy proxy for {}", id, phone)).await;
                    }
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.await;
    }

    println!("\n📊 Final Results: {}", *success_count.lock().await);
    println!("🏁 End: {}", Local::now().format("%c"));

    Ok(())
}
