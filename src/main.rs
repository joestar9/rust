use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::seq::SliceRandom;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::sleep;

// --- Constants ---
const CONFIG_FILE: &str = "config.json";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const PROXY_URL_SOURCE: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.txt";
const TARGET_COUNT: usize = 1000;

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
}

// --- Helper Functions ---

/// Reads config.json
fn read_config() -> Result<AppConfig> {
    let file = File::open(CONFIG_FILE)
        .context(format!("Could not find '{}'. Please create it.", CONFIG_FILE))?;
    let reader = BufReader::new(file);
    let config: AppConfig = serde_json::from_reader(reader)
        .context("Failed to parse config.json format")?;
    
    if config.prefixes.is_empty() {
        return Err(anyhow!("The 'prefixes' list in config.json cannot be empty!"));
    }
    
    Ok(config)
}

/// Generates a random 7-digit suffix
fn generate_random_suffix() -> String {
    let chars: Vec<char> = "1234567890".chars().collect();
    let mut rng = rand::thread_rng();
    chars.choose_multiple(&mut rng, 7).collect()
}

/// Helper to get user input if config is missing values
fn prompt_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer).unwrap();
    buffer.trim().to_string()
}

/// Fetches proxies from GitHub
async fn fetch_online_proxies() -> Result<Vec<String>> {
    let client = Client::new();
    let resp = client.get(PROXY_URL_SOURCE).send().await?.text().await?;
    let proxies: Vec<String> = resp
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(proxies)
}

/// Reads local proxies
fn read_local_proxies() -> Result<Vec<String>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = io::BufReader::new(file);
    let proxies: Vec<String> = reader
        .lines()
        .map(|l| l.unwrap_or_default().trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(proxies)
}

/// Validates proxy health
async fn build_client_with_proxy(proxy_addr: &str, token: &str) -> Option<Client> {
    let proxy = match Proxy::all(proxy_addr) {
        Ok(p) => p,
        Err(_) => return None,
    };

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(token).unwrap_or(reqwest::header::HeaderValue::from_static("")));
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Content-Type", "application/json".parse().unwrap()); // Ensure content-type is set

    let client_builder = Client::builder()
        .default_headers(headers)
        .proxy(proxy)
        .timeout(Duration::from_secs(8)); 

    match client_builder.build() {
        Ok(client) => {
            // Health check: HEAD request to base domain
            if client.head("https://my.irancell.ir").send().await.is_ok() {
                Some(client)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Process a single number
async fn process_number(
    client: &Client,
    phone: String,
    success_counter: &Arc<Mutex<usize>>,
) {
    let data = InviteData {
        application_name: "NGMI".to_string(),
        friend_number: phone.clone(),
    };

    // Step 1: Check Eligibility
    let res1 = client
        .post(API_CHECK_APP)
        .header("Referer", "https://my.irancell.ir/invite")
        .json(&data)
        .send()
        .await;

    if let Ok(resp) = res1 {
        if resp.status().is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            if body["message"] == "done" {
                // Step 2: Send Invite
                let res2 = client
                    .post(API_SEND_INVITE)
                    .header("Referer", "https://my.irancell.ir/invite/confirm")
                    .json(&data)
                    .send()
                    .await;

                if let Ok(resp2) = res2 {
                    if resp2.status().is_success() {
                        let body2: Value = resp2.json().await.unwrap_or(Value::Null);
                        if body2["message"] == "done" {
                            println!("✅ Invite sent: {}", phone);
                            let mut lock = success_counter.lock().await;
                            *lock += 1;
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // --- Load Config ---
    let config = read_config()?;
    
    // Determine Token
    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("Enter Token: "),
    };

    println!("📱 Loaded {} prefixes from config.", config.prefixes.len());

    // --- Select Mode ---
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

    // --- Worker Count ---
    let workers_input = prompt_input("Number of Workers (Threads): ");
    let worker_count: usize = workers_input.parse().unwrap_or(5);

    // --- Proxy Setup ---
    let proxy_list: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));

    if mode == RunMode::LocalProxy {
        let loaded = read_local_proxies()?;
        println!("📁 Loaded {} local proxies.", loaded.len());
        *proxy_list.write().await = loaded;
    } else if mode == RunMode::AutoProxy {
        println!("🌍 Fetching proxies...");
        match fetch_online_proxies().await {
            Ok(proxies) => {
                println!("📥 Downloaded {} proxies.", proxies.len());
                *proxy_list.write().await = proxies;
            }
            Err(e) => eprintln!("❌ Failed to fetch proxies: {}", e),
        }

        // Background Updater Task
        let p_list_clone = proxy_list.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(600)).await;
                println!("\n🔄 Updating Proxies...");
                if let Ok(new_proxies) = fetch_online_proxies().await {
                    *p_list_clone.write().await = new_proxies;
                }
            }
        });
    }

    println!("Start: {}", Local::now().format("%c"));
    let success_count = Arc::new(Mutex::new(0));
    
    // Channel for distribution
    let (tx, rx) = mpsc::channel::<String>(100);
    let shared_rx = Arc::new(Mutex::new(rx));
    let prefixes = Arc::new(config.prefixes);

    // --- Generator Task ---
    let prefixes_clone = prefixes.clone();
    tokio::spawn(async move {
        let mut rng = rand::thread_rng();
        for _ in 0..TARGET_COUNT {
            // Pick a random prefix from config
            if let Some(prefix) = prefixes_clone.choose(&mut rng) {
                let num = format!("98{}{}", prefix, generate_random_suffix());
                if tx.send(num).await.is_err() {
                    break;
                }
            }
        }
    });

    // --- Worker Tasks ---
    let mut handles = Vec::new();

    for _ in 0..worker_count {
        let rx_clone = shared_rx.clone();
        let proxies_clone = proxy_list.clone();
        let token_clone = token.clone();
        let success_clone = success_count.clone();

        let handle = tokio::spawn(async move {
            loop {
                // Fetch number
                let phone_opt = {
                    let mut rx_guard = rx_clone.lock().await;
                    rx_guard.recv().await
                };

                let phone = match phone_opt {
                    Some(p) => p,
                    None => break,
                };

                // Prepare Client
                let client_opt: Option<Client> = if mode == RunMode::Direct {
                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(&token_clone).unwrap_or(reqwest::header::HeaderValue::from_static("")));
                    headers.insert("Content-Type", "application/json".parse().unwrap());
                    Client::builder().default_headers(headers).build().ok()
                } else {
                    // Proxy Retry Logic
                    let mut selected_client = None;
                    for _ in 0..3 { // Try 3 times to find a working proxy
                        let proxy_addr = {
                            let r_guard = proxies_clone.read().await;
                            if r_guard.is_empty() { None } 
                            else { r_guard.choose(&mut rand::thread_rng()).cloned() }
                        };

                        if let Some(p) = proxy_addr {
                            if let Some(c) = build_client_with_proxy(&p, &token_clone).await {
                                selected_client = Some(c);
                                break;
                            }
                        } else {
                            break; // No proxies loaded
                        }
                    }
                    selected_client
                };

                if let Some(client) = client_opt {
                    process_number(&client, phone, &success_clone).await;
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.await;
    }

    println!("numbers: {}", *success_count.lock().await);
    println!("End: {}", Local::now().format("%c"));

    Ok(())
}
