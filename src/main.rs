use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::prelude::*;
use reqwest::{Client, Proxy};
use reqwest::header::{HeaderMap, HeaderValue, COOKIE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Semaphore, watch};
use tokio::time::sleep;

const CONFIG_FILE: &str = "config.json";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.json";
const MAX_RETRIES_BEFORE_SWITCH: u8 = 3; 
const PRIME_MULTIPLIER: usize = 6_786_793; 
const MODULO_SPACE: usize = 10_000_000;

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
    Https,
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

fn generate_unique_number(global_index: usize, start_offset: usize, prefixes: &[String]) -> String {
    let prefix_idx = global_index % prefixes.len();
    let prefix = &prefixes[prefix_idx];
    let sequence_num = global_index / prefixes.len();
    let effective_index = sequence_num + start_offset;
    let unique_suffix_val = (effective_index * PRIME_MULTIPLIER) % MODULO_SPACE;
    format!("98{}{:07}", prefix, unique_suffix_val)
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
    
    if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5://"); }
    if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5://"); }
    
    if default_proto == "socks5" || default_proto == "socks5h" {
        if clean.starts_with("socks5://") {
            return clean.replace("socks5://", "socks5h://");
        }
        if !clean.contains("://") {
            return format!("socks5h://{}", clean);
        }
    }

    if default_proto == "http" && !clean.contains("://") {
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

async fn get_initial_cookie_string(token: &str) -> Result<String> {
    println!("🍪 Fetching initial cookies...");
    
    let mut headers = HeaderMap::new();
    headers.insert("Authorization", HeaderValue::from_str(token)?);
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse().unwrap());
    
    let client = Client::builder()
        .default_headers(headers)
        .cookie_store(true) 
        .timeout(Duration::from_secs(10))
        .build()?;

    let resp = client.get(API_CHECK_APP).send().await?;
    
    let cookies: Vec<String> = resp.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap_or("").split(';').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if cookies.is_empty() {
        println!("⚠️ No cookies received from server. Proceeding without cookies.");
        return Ok(String::new());
    }

    let cookie_string = cookies.join("; ");
    println!("✅ Cookies acquired: {}", cookie_string);
    Ok(cookie_string)
}

fn build_client(token: &str, proxy: Option<Proxy>, cookie_string: &str) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse().unwrap());
    headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    headers.insert("Accept-Language", "fa".parse().unwrap());
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Authorization", HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse().unwrap());

    if !cookie_string.is_empty() {
        if let Ok(val) = HeaderValue::from_str(cookie_string) {
            headers.insert(COOKIE, val);
        }
    }

    let mut builder = Client::builder()
        .default_headers(headers)
        .tcp_nodelay(true)
        .cookie_store(false)
        .pool_idle_timeout(Duration::from_secs(90))
        .timeout(Duration::from_secs(15));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

async fn fetch_proxies_list(_token: String, filter: ProxyFilter, silent: bool) -> Result<Vec<String>> {
    if !silent { println!("⏳ Connecting to GitHub to fetch proxy sources..."); }
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
            if !silent { println!("🌐 Fetching ALL proxy types..."); }
            for url in sources.http { tasks.push(spawn_download(url, "http", fetcher.clone())); }
            for url in sources.https { tasks.push(spawn_download(url, "https", fetcher.clone())); }
            for url in sources.socks4 { tasks.push(spawn_download(url, "socks4", fetcher.clone())); }
            for url in sources.socks5 { tasks.push(spawn_download(url, "socks5h", fetcher.clone())); }
        },
        ProxyFilter::Http => {
            if !silent { println!("🌐 Fetching ONLY HTTP proxies..."); }
            for url in sources.http { tasks.push(spawn_download(url, "http", fetcher.clone())); }
        },
        ProxyFilter::Https => {
            if !silent { println!("🌐 Fetching ONLY HTTPS proxies..."); }
            for url in sources.https { tasks.push(spawn_download(url, "https", fetcher.clone())); }
        },
        ProxyFilter::Socks4 => {
            if !silent { println!("🌐 Fetching ONLY SOCKS4 proxies..."); }
            for url in sources.socks4 { tasks.push(spawn_download(url, "socks4", fetcher.clone())); }
        },
        ProxyFilter::Socks5 => {
            if !silent { println!("🌐 Fetching ONLY SOCKS5 proxies..."); }
            for url in sources.socks5 { tasks.push(spawn_download(url, "socks5h", fetcher.clone())); }
        },
    }

    if tasks.is_empty() {
        return Err(anyhow!("No sources found for the selected type."));
    }

    if !silent { println!("⬇️  Downloading from {} sources...", tasks.len()); }
    
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

async fn run_worker_robust(
    _worker_id: usize,
    proxy_pool: Arc<Mutex<Vec<String>>>,
    token: String,
    concurrency_limit: usize,
    prefixes: Arc<Vec<String>>,
    success_counter: Arc<AtomicUsize>,
    failure_counter: Arc<AtomicUsize>,
    global_index_counter: Arc<AtomicUsize>,
    start_offset: usize,
    target: usize,
    shutdown_rx: watch::Receiver<bool>,
    use_send_invite: bool,
    refill_filter: Option<ProxyFilter>,
    cookie_str: Arc<String>
) {
    let (status_tx, mut status_rx) = mpsc::channel::<ProxyStatus>(concurrency_limit + 10);
    let sem = Arc::new(Semaphore::new(concurrency_limit));

    let mut current_client: Option<(Client, Arc<AtomicBool>)> = None;
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
            if let Some((_, active_flag)) = &current_client {
                active_flag.store(false, Ordering::Relaxed);
            }
            current_client = None; 
            consecutive_errors = 0; 
        }

        if current_client.is_none() {
            let needs_refill = {
                let pool = proxy_pool.lock().await;
                pool.is_empty()
            };

            if needs_refill {
                if let Some(filter) = refill_filter {
                    if let Ok(new_proxies) = fetch_proxies_list(token.clone(), filter, true).await {
                        let mut pool = proxy_pool.lock().await;
                        pool.extend(new_proxies);
                    }
                }
            }

            let mut pool = proxy_pool.lock().await;

            if let Some(proxy_url) = pool.pop() {
                if let Ok(proxy_obj) = Proxy::all(&proxy_url) {
                    if let Ok(c) = build_client(&token, Some(proxy_obj), &cookie_str) {
                        current_client = Some((c, Arc::new(AtomicBool::new(true))));
                        consecutive_errors = 0;
                    }
                }
            } else {
                drop(pool);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        }

        if let Some((client, active_flag)) = current_client.clone() {
            if let Ok(permit) = sem.clone().acquire_owned().await {
                let p_ref = prefixes.clone();
                let s_ref = success_counter.clone();
                let f_ref = failure_counter.clone();
                let idx_ref = global_index_counter.clone();
                let tx = status_tx.clone();
                let s_rx = shutdown_rx.clone();
                let flag_clone = active_flag.clone();
                
                tokio::spawn(async move {
                    let _p = permit; 
                    if *s_rx.borrow() { return; } 
                    if !flag_clone.load(Ordering::Relaxed) { return; }

                    let current_idx = idx_ref.fetch_add(1, Ordering::Relaxed);
                    let phone = generate_unique_number(current_idx, start_offset, &p_ref);

                    let status = perform_invite(&client, phone, &s_ref, &f_ref, target, use_send_invite).await;
                    let _ = tx.send(status).await;
                });
            }
        }
    }
}

async fn perform_invite(
    client: &Client,
    phone: String,
    success_counter: &Arc<AtomicUsize>,
    failure_counter: &Arc<AtomicUsize>,
    target: usize,
    use_send_invite: bool
) -> ProxyStatus {
    if success_counter.load(Ordering::Relaxed) >= target { return ProxyStatus::Healthy; }

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
                            success_counter.fetch_add(1, Ordering::Relaxed);
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
                                            success_counter.fetch_add(1, Ordering::Relaxed);
                                            return ProxyStatus::Healthy;
                                        }
                                    }
                                }
                                failure_counter.fetch_add(1, Ordering::Relaxed);
                                return ProxyStatus::SoftFail;
                            },
                            Err(_) => {
                                failure_counter.fetch_add(1, Ordering::Relaxed);
                                return ProxyStatus::HardFail;
                            }
                        }
                    }
                }
                return ProxyStatus::Healthy;
            } else {
                failure_counter.fetch_add(1, Ordering::Relaxed);
                return ProxyStatus::SoftFail; 
            }
        },
        Err(_) => {
            failure_counter.fetch_add(1, Ordering::Relaxed);
            return ProxyStatus::HardFail; 
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = read_config()?;
    
    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Loaded {} prefixes.", config.prefixes.len());
    let count_input = prompt_input("🎯 Target SUCCESS Count: ");
    let target_count: usize = count_input.parse().unwrap_or(1000);

    println!("\n✨ Select Mode:");
    println!("1) Direct Mode (No Proxy) 🌐");
    println!("2) Auto Proxy Mode (Online Sources) 🚀");
    println!("3) Local Proxy Mode (File) 📁");
    
    let mode_input = prompt_input("Choice [1-3]: ");
    
    let mut proxy_filter = ProxyFilter::All;
    let mut default_local_proto = "socks5h"; 
    let mut local_file_path = String::from("socks5.txt"); 

    let mode = match mode_input.as_str() {
        "2" => {
            println!("\n🔍 Select Proxy Protocol to Download:");
            println!("1) All (Default) 🌍");
            println!("2) HTTP 🌐");
            println!("3) HTTPS 🔒");
            println!("4) SOCKS4 🔌");
            println!("5) SOCKS5 🛡️");
            
            let filter_input = prompt_input("Choice [1-5]: ");
            proxy_filter = match filter_input.as_str() {
                "2" => ProxyFilter::Http,
                "3" => ProxyFilter::Https,
                "4" => ProxyFilter::Socks4,
                "5" => ProxyFilter::Socks5,
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
            println!("2) HTTP 🌐");
            println!("3) HTTPS 🔒");
            println!("4) SOCKS4 🔌");
            println!("5) SOCKS5 🛡️");
            
            let filter_input = prompt_input("Choice [1-5]: ");
            default_local_proto = match filter_input.as_str() {
                "2" => "http",
                "3" => "https",
                "4" => "socks4",
                "5" => "socks5h",
                _ => "socks5h",
            };
            RunMode::LocalProxy
        },
        _ => RunMode::Direct,
    };

    println!("\n🔧 Select Method:");
    println!("1) Send Invite SMS");
    println!("2) Don't Send Invite SMS");
    
    let api_input = prompt_input("Choice [1-2]: ");
    let use_send_invite = match api_input.as_str() {
        "2" => {
            println!("✅ Only checking app (NO SMS invites will be sent)");
            false
        },
        _ => {
            println!("⚠️ Using Send SMS Invite Method");
            true
        }
    };

    let concurrent_input = prompt_input("⚡ Requests PER Worker: ");
    let requests_per_proxy: usize = concurrent_input.parse().unwrap_or(5);

    let workers_input = prompt_input("👷 Total Worker Threads: ");
    let worker_count: usize = workers_input.parse().unwrap_or(50);

    let mut raw_pool: Vec<String> = Vec::new();

    if mode == RunMode::LocalProxy {
        raw_pool = read_local_list(&local_file_path, default_local_proto)?;
        println!("📦 Loaded {} local proxies.", raw_pool.len());
    } else if mode == RunMode::AutoProxy {
        raw_pool = fetch_proxies_list(token.clone(), proxy_filter, false).await?;
        println!("📦 Downloaded {} proxies.", raw_pool.len());
    } else {
        raw_pool.push("Direct".to_string());
    }

    if raw_pool.is_empty() { return Err(anyhow!("❌ Pool is empty.")); }

    let shared_cookie = Arc::new(get_initial_cookie_string(&token).await.unwrap_or_default());

    let shared_pool = Arc::new(Mutex::new(raw_pool));
    let success_counter = Arc::new(AtomicUsize::new(0));
    let failure_counter = Arc::new(AtomicUsize::new(0));
    let global_index_counter = Arc::new(AtomicUsize::new(0));
    let start_offset = rand::rng().random_range(0..MODULO_SPACE);
    
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let prefixes = Arc::new(config.prefixes);

    println!("🚀 Launching {} worker threads...", worker_count);
    sleep(Duration::from_secs(1)).await;

    print!("\x1B[2J\x1B[1;1H");

    for id in 0..worker_count {
        let pool = shared_pool.clone();
        let token_clone = token.clone();
        let p_ref = prefixes.clone();
        let s_ref = success_counter.clone();
        let f_ref = failure_counter.clone();
        let idx_ref = global_index_counter.clone();
        let rx = shutdown_rx.clone();
        let cookie_ref = shared_cookie.clone();
        
        let refill_filter = if mode == RunMode::AutoProxy { Some(proxy_filter) } else { None };

        if mode == RunMode::Direct {
            let client = build_client(&token_clone, None, &cookie_ref)?;
            tokio::spawn(async move {
                let sem = Arc::new(Semaphore::new(requests_per_proxy));
                loop {
                    if *rx.borrow() { break; }
                    if let Ok(permit) = sem.clone().acquire_owned().await {
                        let c = client.clone();
                        let pr = p_ref.clone();
                        let sr = s_ref.clone();
                        let fr = f_ref.clone();
                        let ir = idx_ref.clone();
                        tokio::spawn(async move {
                            let _p = permit;
                            let current_idx = ir.fetch_add(1, Ordering::Relaxed);
                            let phone = generate_unique_number(current_idx, start_offset, &pr);
                            perform_invite(&c, phone, &sr, &fr, target_count, use_send_invite).await;
                        });
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                run_worker_robust(id, pool, token_clone, requests_per_proxy, p_ref, s_ref, f_ref, idx_ref, start_offset, target_count, rx, use_send_invite, refill_filter, cookie_ref).await;
            });
        }
    }

    let monitor_success = success_counter.clone();
    let monitor_failure = failure_counter.clone();
    let monitor_tx = shutdown_tx.clone();
    let start_time = Instant::now();
    
    loop {
        sleep(Duration::from_secs(2)).await;

        let success = monitor_success.load(Ordering::Relaxed);
        let failures = monitor_failure.load(Ordering::Relaxed);
        
        print!("\x1B[2J\x1B[1;1H"); 
        
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("           🚀 IRANCELL INVITE BOT STATUS         ");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(" ✅ Success: \x1B[32m{}\x1B[0m", success);
        println!(" ❌ Failed:  \x1B[31m{}\x1B[0m", failures);
        println!(" ⏳ Elapsed: {:.2?}", start_time.elapsed());
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        if success >= target_count {
            let _ = monitor_tx.send(true);
            break;
        }
    }

    println!("🏁 Target reached. Stopping...");
    sleep(Duration::from_secs(2)).await;
    println!("\n📊 Final Results: {} Successes, {} Failures", 
        success_counter.load(Ordering::Relaxed), 
        failure_counter.load(Ordering::Relaxed));
    Ok(())
}
