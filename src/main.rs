use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::seq::SliceRandom;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
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

fn read_config() -> Result<AppConfig> {
    // Check if file exists
    if !std::path::Path::new(CONFIG_FILE).exists() {
        return Err(anyhow!("❌ '{}' not found. Please create it with 'token' and 'prefixes'.", CONFIG_FILE));
    }

    let file = File::open(CONFIG_FILE).context("Error opening config file")?;
    let reader = BufReader::new(file);
    let config: AppConfig = serde_json::from_reader(reader)
        .context("❌ Failed to parse config.json. Check syntax.")?;
    
    if config.prefixes.is_empty() {
        return Err(anyhow!("❌ The 'prefixes' list in config.json cannot be empty!"));
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

async fn fetch_online_proxies() -> Result<Vec<String>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
        
    let resp = client.get(PROXY_URL_SOURCE).send().await?.text().await?;
    let proxies: Vec<String> = resp
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(proxies)
}

fn read_local_proxies() -> Result<Vec<String>> {
    let file = File::open("socks5.txt").context("Could not open socks5.txt")?;
    let reader = BufReader::new(file);
    let proxies: Vec<String> = reader
        .lines() // This works now because BufRead is imported
        .map(|l| l.unwrap_or_default().trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(proxies)
}

/// Builds a client with specific headers and optional proxy
/// Used for Proxy Mode validation and requests
async fn build_client_with_proxy(proxy_addr: &str, token: &str) -> Option<Client> {
    let proxy = match Proxy::all(proxy_addr) {
        Ok(p) => p,
        Err(_) => return None,
    };

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    // Note: Do not manually set Accept-Encoding, let reqwest handle it with features
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    
    // Safety check for token characters
    if let Ok(val) = reqwest::header::HeaderValue::from_str(token) {
        headers.insert("Authorization", val);
    }

    let client_builder = Client::builder()
        .default_headers(headers)
        .proxy(proxy)
        .timeout(Duration::from_secs(10)); 

    match client_builder.build() {
        Ok(client) => {
            // Health check: Quick HEAD request
            if client.head("https://my.irancell.ir").send().await.is_ok() {
                Some(client)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Helper to build the main Direct Mode client once
fn build_direct_client(token: &str) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
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

    match res1 {
        Ok(resp) if resp.status().is_success() => {
             let body: Value = resp.json().await.unwrap_or(Value::Null);
             if body["message"] == "done" {
                // Step 2: Send Invite
                let res2 = client
                    .post(API_SEND_INVITE)
                    .header("Referer", "https://my.irancell.ir/invite/confirm")
                    .json(&data)
                    .send()
                    .await;

                match res2 {
                    Ok(resp2) if resp2.status().is_success() => {
                         let body2: Value = resp2.json().await.unwrap_or(Value::Null);
                         if body2["message"] == "done" {
                             println!("✅ Invite sent: {}", phone);
                             let mut lock = success_counter.lock().await;
                             *lock += 1;
                         }
                    },
                    _ => {} // Ignore errors
                }
             }
        },
        _ => {} // Ignore errors
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // --- 1. Config Loading ---
    let config = read_config()?;
    
    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Loaded {} prefixes.", config.prefixes.len());

    // --- 2. Mode Selection ---
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

    // --- 3. Proxy Initialization ---
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

        // Auto-Update Task
        let p_list_clone = proxy_list.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(600)).await; // 10 minutes
                if let Ok(new_proxies) = fetch_online_proxies().await {
                    println!("\n🔄 Proxies updated: {} found.", new_proxies.len());
                    *p_list_clone.write().await = new_proxies;
                }
            }
        });
    }

    println!("🚀 Start: {}", Local::now().format("%c"));
    let success_count = Arc::new(Mutex::new(0));
    
    // --- 4. Task Distribution ---
    let (tx, rx) = mpsc::channel::<String>(100);
    let shared_rx = Arc::new(Mutex::new(rx));
    let prefixes = Arc::new(config.prefixes);

    // Generator
    let prefixes_clone = prefixes.clone();
    tokio::spawn(async move {
        let mut rng = rand::thread_rng();
        for _ in 0..TARGET_COUNT {
            if let Some(prefix) = prefixes_clone.choose(&mut rng) {
                let num = format!("98{}{}", prefix, generate_random_suffix());
                if tx.send(num).await.is_err() {
                    break;
                }
            }
        }
    });

    // Prepare Direct Client (Optimization: Build once, reuse many times)
    let direct_client_base = if mode == RunMode::Direct {
        Some(build_direct_client(&token)?)
    } else {
        None
    };

    // --- 5. Workers ---
    let mut handles = Vec::new();

    for _ in 0..worker_count {
        let rx_clone = shared_rx.clone();
        let proxies_clone = proxy_list.clone();
        let token_clone = token.clone();
        let success_clone = success_count.clone();
        let direct_client = direct_client_base.clone();

        let handle = tokio::spawn(async move {
            loop {
                // Get next number
                let phone_opt = {
                    let mut rx_guard = rx_clone.lock().await;
                    rx_guard.recv().await
                };

                let phone = match phone_opt {
                    Some(p) => p,
                    None => break, // Channel empty
                };

                if mode == RunMode::Direct {
                    // Fast path: Use existing client
                    if let Some(c) = &direct_client {
                        process_number(c, phone, &success_clone).await;
                    }
                } else {
                    // Proxy path
                    let mut selected_client = None;
                    
                    // Retry Logic
                    for _ in 0..3 {
                        let proxy_addr = {
                            let r_guard = proxies_clone.read().await;
                            if r_guard.is_empty() { 
                                None 
                            } else { 
                                r_guard.choose(&mut rand::thread_rng()).cloned() 
                            }
                        };

                        if let Some(p) = proxy_addr {
                            if let Some(c) = build_client_with_proxy(&p, &token_clone).await {
                                selected_client = Some(c);
                                break;
                            }
                        } else {
                            // No proxies available! Sleep briefly to avoid CPU spin
                            sleep(Duration::from_millis(1000)).await;
                            break;
                        }
                    }

                    if let Some(client) = selected_client {
                        process_number(&client, phone, &success_clone).await;
                    }
                }
            }
        });
        handles.push(handle);
    }

    // Wait for completion
    for h in handles {
        let _ = h.await;
    }

    println!("\n📊 Final Results: {}", *success_count.lock().await);
    println!("🏁 End: {}", Local::now().format("%c"));

    Ok(())
}
