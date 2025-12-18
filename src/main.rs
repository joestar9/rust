use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::prelude::*;
use reqwest::{Client, Proxy};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, watch};
use tokio::time::sleep;

mod core {
    pub const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
    pub const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
    pub const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.json";
    pub const MAX_RETRIES_BEFORE_SWITCH: u8 = 3; 
    pub const CONFIG_FILE: &str = "config.json";
    pub const LOG_FILE: &str = "debug.log";
}

mod models {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, PartialEq)]
    pub enum RunMode {
        Direct,
        AutoProxy,
        LocalProxy,
    }

    #[derive(Clone, Copy, PartialEq)]
    pub enum ProxyFilter {
        All,
        Http,
        Https,
        Socks4,
        Socks5,
        Unknown,
    }

    pub enum ProxyStatus {
        Healthy,
        SoftFail,
        HardFail,
    }

    #[derive(Serialize)]
    pub struct InviteData {
        pub application_name: String,
        pub friend_number: String,
    }

    #[derive(Deserialize, Debug)]
    pub struct AppConfig {
        pub token: Option<String>,
        pub prefixes: Vec<String>,
        pub debug: Option<bool>,
    }

    #[derive(Deserialize, Debug)]
    pub struct ProxySourceConfig {
        #[serde(default)]
        pub http: Vec<String>,
        #[serde(default)]
        pub https: Vec<String>,
        #[serde(default)]
        pub socks4: Vec<String>,
        #[serde(default)]
        pub socks5: Vec<String>,
        #[serde(default)]
        pub unknown: Vec<String>,
    }
}

mod utils {
    use super::*;

    pub fn read_config() -> Result<models::AppConfig> {
        if !std::path::Path::new(core::CONFIG_FILE).exists() {
            return Err(anyhow!("❌ '{}' not found.", core::CONFIG_FILE));
        }
        let file = File::open(core::CONFIG_FILE).context("Error opening config file")?;
        let reader = BufReader::new(file);
        let config: models::AppConfig = serde_json::from_reader(reader).context("❌ Failed to parse config.json.")?;
        if config.prefixes.is_empty() { return Err(anyhow!("❌ Prefixes list is empty!")); }
        Ok(config)
    }

    pub fn generate_random_suffix() -> String {
        let chars: Vec<char> = "1234567890".chars().collect();
        let mut rng = rand::rng(); 
        chars.choose_multiple(&mut rng, 7).into_iter().collect()
    }

    pub fn prompt_input(prompt: &str) -> String {
        print!("{}", prompt);
        io::stdout().flush().unwrap();
        let mut buffer = String::new();
        io::stdin().read_line(&mut buffer).unwrap();
        buffer.trim().to_string()
    }

    pub async fn log_debug(msg: String) {
        let timestamp = Local::now().format("%H:%M:%S");
        let log_line = format!("[{}] {}\n", timestamp, msg);
        let result = OpenOptions::new().create(true).append(true).open(core::LOG_FILE);
        if let Ok(mut file) = result { let _ = file.write_all(log_line.as_bytes()); }
    }
}

mod network {
    use super::*;

    pub fn format_proxy_url(raw: &str, proto: &str) -> String {
        let mut clean = raw.trim().to_string();
        if clean.starts_with("soks5://") { clean = clean.replace("soks5://", "socks5h://"); }
        if clean.starts_with("sock5://") { clean = clean.replace("sock5://", "socks5h://"); }
        if clean.starts_with("socks5://") { clean = clean.replace("socks5://", "socks5h://"); }
        if !clean.contains("://") { return format!("{}://{}", proto, clean); }
        clean
    }
    
    pub fn guess_proxy_url(raw: &str) -> String {
        let clean = raw.trim();
        if clean.contains("://") { return format_proxy_url(clean, "http"); }
        if let Some(port_str) = clean.split(':').last() {
            if let Ok(port) = port_str.parse::<u16>() {
                return match port {
                    1080 => format_proxy_url(clean, "socks5h"),
                    443 | 8443 => format_proxy_url(clean, "https"),
                    _ => format_proxy_url(clean, "http"),
                };
            }
        }
        format_proxy_url(clean, "http")
    }

    pub fn build_client(token: &str, proxy: Option<Proxy>) -> Result<Client> {
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
            .pool_max_idle_per_host(100)
            .timeout(Duration::from_secs(20));

        if let Some(p) = proxy {
            builder = builder.proxy(p);
        }
        builder.build().context("Failed to build client")
    }

    pub async fn fetch_proxies_list(_token: String, filter: models::ProxyFilter) -> Result<Vec<String>> {
        println!("⏳ Connecting to GitHub to fetch proxy sources...");
        let fetcher = Client::builder().timeout(Duration::from_secs(30)).build()?;
        
        let json_text = match fetcher.get(core::SOURCE_OF_SOURCES_URL).send().await {
            Ok(res) => res.text().await?,
            Err(e) => return Err(anyhow!("Failed to fetch source JSON: {}", e)),
        };

        let sources: models::ProxySourceConfig = match serde_json::from_str(&json_text) {
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
                            if !p.is_empty() && p.contains(':') { found.push(format_proxy_url(p, proto)); }
                        }
                    }
                }
                found
            })
        };
        
        let spawn_guess_download = |url: String, client: Client| {
            tokio::spawn(async move {
                let mut found = Vec::new();
                if let Ok(resp) = client.get(&url).timeout(Duration::from_secs(15)).send().await {
                    if let Ok(text) = resp.text().await {
                        for line in text.lines() {
                            let p = line.trim();
                            if !p.is_empty() && p.contains(':') { found.push(guess_proxy_url(p)); }
                        }
                    }
                }
                found
            })
        };

        match filter {
            models::ProxyFilter::All => {
                println!("🌐 Fetching ALL proxy types...");
                for url in sources.http { tasks.push(spawn_download(url, "http", fetcher.clone())); }
                for url in sources.https { tasks.push(spawn_download(url, "https", fetcher.clone())); }
                for url in sources.socks4 { tasks.push(spawn_download(url, "socks4", fetcher.clone())); }
                for url in sources.socks5 { tasks.push(spawn_download(url, "socks5h", fetcher.clone())); }
                for url in sources.unknown { tasks.push(spawn_guess_download(url, fetcher.clone())); }
            },
            models::ProxyFilter::Http => {
                println!("🌐 Fetching ONLY HTTP proxies...");
                for url in sources.http { tasks.push(spawn_download(url, "http", fetcher.clone())); }
            },
            models::ProxyFilter::Https => {
                println!("🌐 Fetching ONLY HTTPS proxies...");
                for url in sources.https { tasks.push(spawn_download(url, "https", fetcher.clone())); }
            },
            models::ProxyFilter::Socks4 => {
                println!("🌐 Fetching ONLY SOCKS4 proxies...");
                for url in sources.socks4 { tasks.push(spawn_download(url, "socks4", fetcher.clone())); }
            },
            models::ProxyFilter::Socks5 => {
                println!("🌐 Fetching ONLY SOCKS5 proxies...");
                for url in sources.socks5 { tasks.push(spawn_download(url, "socks5h", fetcher.clone())); }
            },
            models::ProxyFilter::Unknown => {
                println!("🌐 Fetching UNKNOWN proxies (Smart Guess)...");
                for url in sources.unknown { tasks.push(spawn_guess_download(url, fetcher.clone())); }
            }
        }

        if tasks.is_empty() { return Err(anyhow!("No sources found for the selected type.")); }

        println!("⬇️  Downloading from {} sources...", tasks.len());
        
        for task in tasks {
            if let Ok(proxies) = task.await {
                for p in proxies { raw_proxies.insert(p); }
            }
        }
        
        let mut final_list: Vec<String> = raw_proxies.into_iter().collect();
        final_list.shuffle(&mut rand::rng());
        Ok(final_list)
    }

    pub fn read_local_list(file_path: &str, default_proto: &str) -> Result<Vec<String>> {
        let clean_path = file_path.trim().trim_matches('"').trim_matches('\'');
        
        let file = File::open(clean_path).context(format!("Could not open file: {}", clean_path))?;
        let reader = BufReader::new(file);
        let mut unique_set = HashSet::new();
        
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
}

mod worker {
    use super::*;
    use tokio::sync::mpsc;

    pub async fn run_worker_robust(
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
        let (status_tx, mut status_rx) = mpsc::channel::<models::ProxyStatus>(concurrency_limit + 10);
        let sem = Arc::new(Semaphore::new(concurrency_limit));

        let mut current_client: Option<Client> = None;
        let mut current_proxy_addr: String = String::new();
        let mut consecutive_errors: u8 = 0;

        loop {
            if *shutdown_rx.borrow() { break; }

            while let Ok(status) = status_rx.try_recv() {
                match status {
                    models::ProxyStatus::Healthy | models::ProxyStatus::SoftFail => {
                        if consecutive_errors > 0 { consecutive_errors = 0; }
                    },
                    models::ProxyStatus::HardFail => {
                        consecutive_errors += 1;
                    }
                }
            }

            if consecutive_errors >= core::MAX_RETRIES_BEFORE_SWITCH {
                if debug_mode { 
                    utils::log_debug(format!("♻️ Worker {} switching. Failed: {}", worker_id, current_proxy_addr)).await; 
                }
                current_client = None; 
                consecutive_errors = 0; 
            }

            if current_client.is_none() {
                let mut pool = proxy_pool.lock().await;
                if let Some(proxy_url) = pool.pop() {
                    if let Ok(proxy_obj) = Proxy::all(&proxy_url) {
                        if let Ok(c) = network::build_client(&token, Some(proxy_obj)) {
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
                    let sd_tx = shutdown_rx.sender().clone();
                    
                    tokio::spawn(async move {
                        let _p = permit; 
                        if *s_rx.borrow() { return; } 

                        let prefix = p_ref.as_slice().choose(&mut rand::rng()).unwrap();
                        let phone = format!("98{}{}", prefix, utils::generate_random_suffix());

                        let status = perform_invite(worker_id, &p_addr, &client, phone, &s_ref, debug_mode, target, use_send_invite, &sd_tx).await;
                        let _ = tx.send(status).await;
                    });
                }
            }
        }
    }

    pub async fn perform_invite(
        id: usize,
        proxy_name: &str,
        client: &Client,
        phone: String,
        success_counter: &Arc<Mutex<usize>>,
        debug_mode: bool,
        target: usize,
        use_send_invite: bool,
        shutdown_tx: &watch::Sender<bool>
    ) -> models::ProxyStatus {
        if *success_counter.lock().await >= target { return models::ProxyStatus::Healthy; }

        let data = models::InviteData { application_name: "NGMI".to_string(), friend_number: phone.clone() };

        let res1 = client.post(core::API_CHECK_APP)
            .header("Referer", "https://my.irancell.ir/invite")
            .json(&data).send().await;

        match res1 {
            Ok(resp) => {
                let status_code = resp.status();
                
                if status_code == 403 || status_code == 429 || status_code == 407 {
                    return models::ProxyStatus::HardFail; 
                }

                if status_code.as_u16() == 200 {
                    let text = resp.text().await.unwrap_or_default();
                    if let Ok(body) = serde_json::from_str::<Value>(&text) {
                         if body["message"] == "done" {
                            
                            if use_send_invite {
                                let res2 = client.post(core::API_SEND_INVITE)
                                    .header("Referer", "https://my.irancell.ir/invite/confirm")
                                    .json(&data).send().await;

                                match res2 {
                                    Ok(resp2) => {
                                        if resp2.status().as_u16() != 200 { return models::ProxyStatus::SoftFail; }
                                    },
                                    Err(_) => { return models::ProxyStatus::HardFail; }
                                }
                            }
                            
                            let mut lock = success_counter.lock().await;
                            *lock += 1;
                            let current = *lock;
                            let action = if use_send_invite { "Sent" } else { "Checked" };
                            println!("✅ Worker #{} | Proxy {} | {}: {} ({}/{})", id, proxy_name, action, phone, current, target);

                            if current >= target { let _ = shutdown_tx.send(true); }

                            return models::ProxyStatus::Healthy;
                         }
                    }
                    return models::ProxyStatus::Healthy;
                } else {
                    if debug_mode { utils::log_debug(format!("Worker {} HTTP {} on {}", id, status_code, phone)).await; }
                    return models::ProxyStatus::SoftFail; 
                }
            },
            Err(e) => {
                if debug_mode { utils::log_debug(format!("Worker {} Net Error: {}", id, e)).await; }
                return models::ProxyStatus::HardFail; 
            }
        }
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    let config = utils::read_config()?;
    let debug_mode = config.debug.unwrap_or(false);
    if debug_mode {
        println!("🐞 Debug Mode ON.");
        utils::log_debug("--- SESSION STARTED ---".to_string()).await;
    }

    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => utils::prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Loaded {} prefixes.", config.prefixes.len());
    let count_input = utils::prompt_input("🎯 Target SUCCESS Count: ");
    let target_count: usize = count_input.parse().unwrap_or(1000);

    println!("\n✨ Select Mode:");
    println!("1) Direct Mode (No Proxy) 🌐");
    println!("2) Auto Proxy Mode (JSON Sources) 🚀");
    println!("3) Local Proxy Mode (File) 📁");
    
    let mode_input = utils::prompt_input("Choice [1-3]: ");
    
    let mut proxy_filter = models::ProxyFilter::All;
    let mut default_local_proto = "socks5h"; 
    let mut local_file_path = String::from("proxies.txt"); 

    let mode = match mode_input.as_str() {
        "2" => {
            println!("\n🔍 Select Proxy Protocol to Download:");
            println!("1) All (Default) 🌍");
            println!("2) HTTP 🌐");
            println!("3) HTTPS 🔒");
            println!("4) SOCKS4 🔌");
            println!("5) SOCKS5 (Remote DNS) 🛡️");
            println!("6) Unknown (Smart Guess) 🤔");
            
            let filter_input = utils::prompt_input("Choice [1-6]: ");
            proxy_filter = match filter_input.as_str() {
                "2" => models::ProxyFilter::Http,
                "3" => models::ProxyFilter::Https,
                "4" => models::ProxyFilter::Socks4,
                "5" => models::ProxyFilter::Socks5,
                "6" => models::ProxyFilter::Unknown,
                _ => models::ProxyFilter::All,
            };
            models::RunMode::AutoProxy
        },
        "3" => {
            let path_input = utils::prompt_input("📁 Enter Proxy File Path (Drag & Drop): ");
            if !path_input.trim().is_empty() { local_file_path = path_input; }

            println!("\n🔍 Select Default Protocol for Local File:");
            println!("1) Smart Guess (Based on Port) 🤔");
            println!("2) HTTP 🌐");
            println!("3) HTTPS 🔒");
            println!("4) SOCKS4 🔌");
            println!("5) SOCKS5 (Remote DNS) 🛡️");
            
            let filter_input = utils::prompt_input("Choice [1-5]: ");
            default_local_proto = match filter_input.as_str() {
                "1" => "guess",
                "2" => "http",
                "3" => "https",
                "4" => "socks4",
                "5" => "socks5h",
                _ => "guess",
            };
            models::RunMode::LocalProxy
        },
        _ => models::RunMode::Direct,
    };

    let concurrent_input = utils::prompt_input("⚡ Requests PER PROXY (simultaneous): ");
    let requests_per_proxy: usize = concurrent_input.parse().unwrap_or(5);

    let workers_input = utils::prompt_input("👷 Total Worker Threads: ");
    let worker_count: usize = workers_input.parse().unwrap_or(50);
    
    let send_invite_input = utils::prompt_input("\n❓ Send Invite Notification (Step 2)? [Y/n]: ");
    let use_send_invite = !send_invite_input.to_lowercase().starts_with('n');

    let mut raw_pool: Vec<String> = Vec::new();

    if mode == models::RunMode::LocalProxy {
        raw_pool = network::read_local_list(&local_file_path, default_local_proto)?;
    } else if mode == models::RunMode::AutoProxy {
        raw_pool = network::fetch_proxies_list(token.clone(), proxy_filter).await?;
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
        
        if mode == models::RunMode::Direct {
            let client = network::build_client(&token_clone, None)?;
            let sd_tx_clone = shutdown_tx.clone();
            tokio::spawn(async move {
                let sem = Arc::new(Semaphore::new(requests_per_proxy));
                loop {
                    if *rx.borrow() { break; }
                    if let Ok(permit) = sem.clone().acquire_owned().await {
                        let c = client.clone();
                        let pr = p_ref.clone();
                        let sr = s_ref.clone();
                        let sd_tx_inner = sd_tx_clone.clone();
                        tokio::spawn(async move {
                            let _p = permit;
                            let prefix = pr.as_slice().choose(&mut rand::rng()).unwrap();
                            let phone = format!("98{}{}", prefix, utils::generate_random_suffix());
                            worker::perform_invite(id, "DIRECT", &c, phone, &sr, debug_mode, target_count, use_send_invite, &sd_tx_inner).await;
                        });
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                worker::run_worker_robust(id, pool, token_clone, requests_per_proxy, p_ref, s_ref, target_count, rx, debug_mode, use_send_invite).await;
            });
        }
    }

    let monitor_success = success_counter.clone();
    
    loop {
        sleep(Duration::from_secs(1)).await;
        if *monitor_success.lock().await >= target_count {
            let _ = shutdown_tx.send(true);
            break;
        }
    }

    println!("🏁 Target reached. Stopping...");
    sleep(Duration::from_secs(2)).await;
    println!("\n📊 Final Results: {} Successes", *monitor_success.lock().await);
    Ok(())
}
