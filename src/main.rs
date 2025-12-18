use anyhow::{Context, Result, anyhow};
use chrono::Local;
use rand::prelude::*;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::fs::{OpenOptions, File};
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Semaphore, watch, RwLock};
use tokio::time::sleep;

const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.json";
const MAX_RETRIES_BEFORE_SWITCH: u8 = 3;
const PROXY_REFRESH_THRESHOLD: usize = 10;
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Debug)]
enum RunMode {
    Direct,
    AutoProxy,
    LocalProxy,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ProxyFilter {
    All,
    Http,
    Https,
    Socks4,
    Socks5,
}

#[derive(Debug, Clone, Copy)]
enum ProxyStatus {
    Healthy,
    SoftFail,
    HardFail,
}

#[derive(Debug, Clone)]
struct ProxyHealth {
    url: String,
    success_count: u32,
    fail_count: u32,
    last_used: Instant,
    last_success: Option<Instant>,
}

impl ProxyHealth {
    fn new(url: String) -> Self {
        Self {
            url,
            success_count: 0,
            fail_count: 0,
            last_used: Instant::now(),
            last_success: None,
        }
    }

    fn success(&mut self) {
        self.success_count += 1;
        self.fail_count = 0;
        self.last_success = Some(Instant::now());
        self.last_used = Instant::now();
    }

    fn fail(&mut self) {
        self.fail_count += 1;
        self.last_used = Instant::now();
    }

    fn score(&self) -> f32 {
        let success_weight = self.success_count as f32 * 1.0;
        let fail_penalty = self.fail_count as f32 * 2.0;
        let recency_bonus = if self.last_success.is_some() {
            let elapsed = self.last_success.unwrap().elapsed().as_secs() as f32;
            1000.0 / (elapsed + 1.0)
        } else {
            0.0
        };
        
        success_weight - fail_penalty + recency_bonus
    }
}

struct ProxyManager {
    proxies: RwLock<VecDeque<ProxyHealth>>,
    mode: RunMode,
    filter: ProxyFilter,
    local_path: Option<String>,
    default_proto: String,
    client: Client,
    last_refresh: Mutex<Instant>,
}

impl ProxyManager {
    async fn new(
        mode: RunMode,
        filter: ProxyFilter,
        local_path: Option<String>,
        default_proto: String,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        
        let manager = Self {
            proxies: RwLock::new(VecDeque::new()),
            mode,
            filter,
            local_path,
            default_proto,
            client,
            last_refresh: Mutex::new(Instant::now()),
        };
        
        manager.refresh_proxies().await?;
        Ok(manager)
    }
    
    async fn refresh_proxies(&self) -> Result<()> {
        let mut proxies = Vec::new();
        
        match self.mode {
            RunMode::AutoProxy => {
                println!("🔄 Fetching proxies from online sources...");
                proxies = self.fetch_online_proxies().await?;
            }
            RunMode::LocalProxy => {
                if let Some(path) = &self.local_path {
                    println!("📁 Loading proxies from local file: {}", path);
                    proxies = self.load_local_proxies(path).await?;
                }
            }
            RunMode::Direct => {
                proxies = vec!["direct://".to_string()];
            }
        }
        
        let mut proxy_health_list: VecDeque<ProxyHealth> = proxies
            .into_iter()
            .map(ProxyHealth::new)
            .collect();
        
        // Sort by randomness initially
        let mut rng = rand::rng();
        proxy_health_list.make_contiguous().shuffle(&mut rng);
        
        let mut guard = self.proxies.write().await;
        *guard = proxy_health_list;
        *self.last_refresh.lock().await = Instant::now();
        
        println!("✅ Loaded {} proxies", guard.len());
        Ok(())
    }
    
    async fn fetch_online_proxies(&self) -> Result<Vec<String>> {
        let json_text = self.client
            .get(SOURCE_OF_SOURCES_URL)
            .send()
            .await?
            .text()
            .await?;
        
        let sources: ProxySourceConfig = serde_json::from_str(&json_text)?;
        
        let mut tasks = Vec::new();
        let client = self.client.clone();
        
        match self.filter {
            ProxyFilter::All => {
                for url in sources.http {
                    tasks.push(Self::download_proxies(url, "http", client.clone()));
                }
                for url in sources.https {
                    tasks.push(Self::download_proxies(url, "https", client.clone()));
                }
                for url in sources.socks4 {
                    tasks.push(Self::download_proxies(url, "socks4", client.clone()));
                }
                for url in sources.socks5 {
                    tasks.push(Self::download_proxies(url, "socks5h", client.clone()));
                }
            }
            ProxyFilter::Http => {
                for url in sources.http {
                    tasks.push(Self::download_proxies(url, "http", client.clone()));
                }
            }
            ProxyFilter::Https => {
                for url in sources.https {
                    tasks.push(Self::download_proxies(url, "https", client.clone()));
                }
            }
            ProxyFilter::Socks4 => {
                for url in sources.socks4 {
                    tasks.push(Self::download_proxies(url, "socks4", client.clone()));
                }
            }
            ProxyFilter::Socks5 => {
                for url in sources.socks5 {
                    tasks.push(Self::download_proxies(url, "socks5h", client.clone()));
                }
            }
        }
        
        let mut all_proxies = HashSet::new();
        for task in tasks {
            if let Ok(proxies) = task.await {
                for proxy in proxies {
                    all_proxies.insert(proxy);
                }
            }
        }
        
        Ok(all_proxies.into_iter().collect())
    }
    
    async fn download_proxies(url: String, proto: &str, client: Client) -> Result<Vec<String>> {
        let mut proxies = Vec::new();
        
        if let Ok(resp) = client.get(&url).timeout(Duration::from_secs(15)).send().await {
            if let Ok(text) = resp.text().await {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && trimmed.contains(':') {
                        proxies.push(format_proxy_url(trimmed, proto));
                    }
                }
            }
        }
        
        Ok(proxies)
    }
    
    async fn load_local_proxies(&self, path: &str) -> Result<Vec<String>> {
        let clean_path = path.trim().trim_matches('"').trim_matches('\'');
        let file = File::open(clean_path)?;
        let reader = BufReader::new(file);
        
        let mut proxies = HashSet::new();
        for line in reader.lines() {
            if let Ok(line) = line {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    proxies.insert(format_proxy_url(trimmed, &self.default_proto));
                }
            }
        }
        
        Ok(proxies.into_iter().collect())
    }
    
    async fn get_proxy(&self) -> Option<String> {
        let mut proxies = self.proxies.write().await;
        
        // Check if we need to refresh
        if proxies.len() < PROXY_REFRESH_THRESHOLD && self.mode != RunMode::Direct {
            let last_refresh = self.last_refresh.lock().await;
            if last_refresh.elapsed() > Duration::from_secs(30) {
                drop(last_refresh);
                drop(proxies); // Release lock before async call
                
                if let Err(e) = self.refresh_proxies().await {
                    log_debug(format!("Failed to refresh proxies: {}", e)).await;
                }
                
                proxies = self.proxies.write().await;
            }
        }
        
        if proxies.is_empty() {
            return None;
        }
        
        // Smart proxy selection based on health score
        let len = proxies.len();
        let mut rng = rand::rng();
        let selection_range = (len / 4).max(1);
        let start_idx = rng.gen_range(0..len);
        
        let mut best_idx = start_idx;
        let mut best_score = f32::MIN;
        
        for i in 0..selection_range {
            let idx = (start_idx + i) % len;
            if let Some(proxy) = proxies.get(idx) {
                let score = proxy.score();
                if score > best_score {
                    best_score = score;
                    best_idx = idx;
                }
            }
        }
        
        if let Some(mut proxy_health) = proxies.remove(best_idx) {
            proxy_health.last_used = Instant::now();
            proxies.push_back(proxy_health.clone());
            Some(proxy_health.url)
        } else {
            None
        }
    }
    
    async fn update_proxy_health(&self, proxy_url: &str, status: ProxyStatus) {
        let mut proxies = self.proxies.write().await;
        
        for proxy in proxies.iter_mut() {
            if proxy.url == proxy_url {
                match status {
                    ProxyStatus::Healthy => proxy.success(),
                    ProxyStatus::SoftFail => proxy.fail(),
                    ProxyStatus::HardFail => {
                        proxy.fail();
                        // Remove proxies that fail too often
                        if proxy.fail_count > 5 {
                            if let Some(pos) = proxies.iter().position(|p| p.url == proxy_url) {
                                proxies.remove(pos);
                                log_debug(format!("Removed bad proxy: {}", proxy_url)).await;
                            }
                        }
                    }
                }
                break;
            }
        }
    }
    
    async fn health_check(&self) -> Result<()> {
        println!("🏥 Running proxy health check...");
        let proxies = self.proxies.read().await.clone();
        
        let mut tasks = Vec::new();
        for proxy in proxies.iter() {
            let url = proxy.url.clone();
            let client = self.client.clone();
            
            tasks.push(tokio::spawn(async move {
                if url == "direct://" {
                    return (url, true);
                }
                
                match Proxy::all(&url) {
                    Ok(proxy_obj) => {
                        let test_client = Client::builder()
                            .proxy(proxy_obj)
                            .timeout(Duration::from_secs(10))
                            .build();
                        
                        match test_client {
                            Ok(c) => {
                                let result = c.get("http://httpbin.org/ip").send().await;
                                (url, result.is_ok())
                            }
                            Err(_) => (url, false),
                        }
                    }
                    Err(_) => (url, false),
                }
            }));
        }
        
        let mut healthy_count = 0;
        for task in tasks {
            if let Ok((url, is_healthy)) = task.await {
                if is_healthy {
                    healthy_count += 1;
                } else if self.mode != RunMode::Direct {
                    let mut proxies = self.proxies.write().await;
                    if let Some(pos) = proxies.iter().position(|p| p.url == url) {
                        proxies.remove(pos);
                    }
                }
            }
        }
        
        println!("✅ Health check: {}/{} proxies healthy", healthy_count, proxies.len());
        
        // Auto-refresh if too many proxies are bad
        if self.mode != RunMode::Direct && healthy_count < PROXY_REFRESH_THRESHOLD {
            println!("⚠️  Too few healthy proxies, triggering refresh...");
            self.refresh_proxies().await?;
        }
        
        Ok(())
    }
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
    let file = File::open(CONFIG_FILE)?;
    let reader = BufReader::new(file);
    let config: AppConfig = serde_json::from_reader(reader)?;
    
    if config.prefixes.is_empty() {
        return Err(anyhow!("❌ Prefixes list is empty!"));
    }
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
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_line = format!("[{}] {}\n", timestamp, msg);
    
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
    {
        let _ = file.write_all(log_line.as_bytes());
    }
}

fn format_proxy_url(raw: &str, default_proto: &str) -> String {
    let mut clean = raw.trim().to_string();
    
    if clean.starts_with("soks5://") {
        clean = clean.replace("soks5://", "socks5://");
    }
    if clean.starts_with("sock5://") {
        clean = clean.replace("sock5://", "socks5://");
    }
    
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

fn build_client(token: &str, proxy_url: Option<&str>) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0".parse()?);
    headers.insert("Accept", "application/json, text/plain, */*".parse()?);
    headers.insert("Accept-Language", "fa".parse()?);
    headers.insert("Origin", "https://my.irancell.ir".parse()?);
    headers.insert("x-app-version", "9.62.0".parse()?);
    headers.insert("Authorization", reqwest::header::HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse()?);

    let mut builder = Client::builder()
        .default_headers(headers)
        .tcp_nodelay(true)
        .cookie_store(true)
        .pool_idle_timeout(Duration::from_secs(90))
        .timeout(Duration::from_secs(15));

    if let Some(url) = proxy_url {
        if url != "direct://" {
            if let Ok(proxy) = Proxy::all(url) {
                builder = builder.proxy(proxy);
            }
        }
    }

    Ok(builder.build()?)
}

async fn perform_invite(
    worker_id: usize,
    proxy_url: &str,
    client: &Client,
    phone: String,
    success_counter: &Arc<Mutex<usize>>,
    debug_mode: bool,
    target: usize,
    use_send_invite: bool,
    proxy_manager: Option<Arc<ProxyManager>>,
) -> ProxyStatus {
    if *success_counter.lock().await >= target {
        return ProxyStatus::Healthy;
    }

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
            
            if status == 403 || status == 429 || status == 407 {
                return ProxyStatus::HardFail;
            }

            if status.is_success() {
                if let Ok(text) = resp.text().await {
                    if let Ok(body) = serde_json::from_str::<Value>(&text) {
                        if body["message"] == "done" {
                            if !use_send_invite {
                                let mut lock = success_counter.lock().await;
                                *lock += 1;
                                println!("✅ Worker #{:03} | Proxy: {} | Checked: {} ({}/{})", 
                                    worker_id, proxy_url, phone, *lock, target);
                                
                                if let Some(manager) = &proxy_manager {
                                    manager.update_proxy_health(proxy_url, ProxyStatus::Healthy).await;
                                }
                                return ProxyStatus::Healthy;
                            }
                            
                            // Send invite if enabled
                            let res2 = client
                                .post(API_SEND_INVITE)
                                .header("Referer", "https://my.irancell.ir/invite/confirm")
                                .json(&data)
                                .send()
                                .await;

                            match res2 {
                                Ok(resp2) if resp2.status().is_success() => {
                                    if let Ok(text2) = resp2.text().await {
                                        if let Ok(body2) = serde_json::from_str::<Value>(&text2) {
                                            if body2["message"] == "done" {
                                                let mut lock = success_counter.lock().await;
                                                *lock += 1;
                                                println!("✅ Worker #{:03} | Proxy: {} | Sent: {} ({}/{})", 
                                                    worker_id, proxy_url, phone, *lock, target);
                                                
                                                if let Some(manager) = &proxy_manager {
                                                    manager.update_proxy_health(proxy_url, ProxyStatus::Healthy).await;
                                                }
                                                return ProxyStatus::Healthy;
                                            }
                                        }
                                    }
                                    ProxyStatus::SoftFail
                                }
                                _ => ProxyStatus::SoftFail,
                            }
                        } else {
                            ProxyStatus::Healthy
                        }
                    } else {
                        ProxyStatus::Healthy
                    }
                } else {
                    ProxyStatus::Healthy
                }
            } else {
                if debug_mode {
                    log_debug(format!("Worker {} HTTP {} on {}", worker_id, status, phone)).await;
                }
                ProxyStatus::SoftFail
            }
        }
        Err(e) => {
            if debug_mode {
                log_debug(format!("Worker {} Network Error: {}", worker_id, e)).await;
            }
            ProxyStatus::HardFail
        }
    }
}

async fn worker_direct(
    worker_id: usize,
    token: String,
    prefixes: Arc<Vec<String>>,
    success_counter: Arc<Mutex<usize>>,
    shutdown_rx: watch::Receiver<bool>,
    debug_mode: bool,
    target: usize,
    use_send_invite: bool,
    concurrency: usize,
) -> Result<()> {
    let client = build_client(&token, None)?;
    let semaphore = Arc::new(Semaphore::new(concurrency));

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        if *success_counter.lock().await >= target {
            break;
        }

        let permit = semaphore.clone().acquire_owned().await?;
        let client_clone = client.clone();
        let prefixes_clone = prefixes.clone();
        let success_clone = success_counter.clone();
        let shutdown_clone = shutdown_rx.clone();

        tokio::spawn(async move {
            let _permit = permit;
            
            if *shutdown_clone.borrow() {
                return;
            }

            let prefix = prefixes_clone.choose(&mut rand::rng()).unwrap();
            let phone = format!("98{}{}", prefix, generate_random_suffix());
            
            perform_invite(
                worker_id,
                "DIRECT",
                &client_clone,
                phone,
                &success_clone,
                debug_mode,
                target,
                use_send_invite,
                None,
            ).await;
        });
    }

    Ok(())
}

async fn worker_proxy(
    worker_id: usize,
    token: String,
    proxy_manager: Arc<ProxyManager>,
    prefixes: Arc<Vec<String>>,
    success_counter: Arc<Mutex<usize>>,
    shutdown_rx: watch::Receiver<bool>,
    debug_mode: bool,
    target: usize,
    use_send_invite: bool,
    concurrency: usize,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut current_client: Option<Client> = None;
    let mut current_proxy: String = String::new();
    let mut error_count: u8 = 0;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        if *success_counter.lock().await >= target {
            break;
        }

        // Get a new proxy if needed
        if current_client.is_none() || error_count >= MAX_RETRIES_BEFORE_SWITCH {
            if let Some(proxy_url) = proxy_manager.get_proxy().await {
                match build_client(&token, Some(&proxy_url)) {
                    Ok(client) => {
                        current_client = Some(client);
                        current_proxy = proxy_url;
                        error_count = 0;
                        
                        if debug_mode {
                            log_debug(format!("Worker {} using proxy: {}", worker_id, current_proxy)).await;
                        }
                    }
                    Err(_) => {
                        proxy_manager.update_proxy_health(&current_proxy, ProxyStatus::HardFail).await;
                        current_client = None;
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                }
            } else {
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        }

        let permit = semaphore.clone().acquire_owned().await?;
        let client_clone = current_client.clone().unwrap();
        let proxy_clone = current_proxy.clone();
        let prefixes_clone = prefixes.clone();
        let success_clone = success_counter.clone();
        let shutdown_clone = shutdown_rx.clone();
        let proxy_manager_clone = proxy_manager.clone();

        tokio::spawn(async move {
            let _permit = permit;
            
            if *shutdown_clone.borrow() {
                return;
            }

            let prefix = prefixes_clone.choose(&mut rand::rng()).unwrap();
            let phone = format!("98{}{}", prefix, generate_random_suffix());
            
            let status = perform_invite(
                worker_id,
                &proxy_clone,
                &client_clone,
                phone,
                &success_clone,
                debug_mode,
                target,
                use_send_invite,
                Some(proxy_manager_clone),
            ).await;

            // Update error count based on status
            let mut error_count_mut = error_count;
            match status {
                ProxyStatus::Healthy | ProxyStatus::SoftFail => {
                    error_count_mut = 0;
                }
                ProxyStatus::HardFail => {
                    error_count_mut += 1;
                }
            }
        });

        // Small delay to prevent overwhelming
        sleep(Duration::from_millis(10)).await;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 Irancell Referral System v2.0");
    println!("===============================\n");

    let config = read_config()?;
    let debug_mode = config.debug.unwrap_or(false);
    
    if debug_mode {
        println!("🐞 Debug Mode: ON");
        log_debug("=== SESSION STARTED ===".to_string()).await;
    }

    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Authorization Token: "),
    };

    println!("📱 Loaded {} phone prefixes", config.prefixes.len());
    
    let target_input = prompt_input("🎯 Target success count: ");
    let target: usize = target_input.parse().unwrap_or(1000);
    
    println!("\n✨ Select Operation Mode:");
    println!("1) Direct Mode (No Proxy)");
    println!("2) Auto Proxy Mode (Online Sources)");
    println!("3) Local Proxy Mode (From File)");
    
    let mode_choice = prompt_input("Choice [1-3]: ");
    let mode = match mode_choice.as_str() {
        "2" => RunMode::AutoProxy,
        "3" => RunMode::LocalProxy,
        _ => RunMode::Direct,
    };

    let mut filter = ProxyFilter::All;
    let mut local_path = None;
    let mut default_proto = "socks5h".to_string();

    if mode == RunMode::AutoProxy {
        println!("\n🔍 Select Proxy Protocol:");
        println!("1) All Types");
        println!("2) HTTP Only");
        println!("3) HTTPS Only");
        println!("4) SOCKS4 Only");
        println!("5) SOCKS5 Only");
        
        let filter_choice = prompt_input("Choice [1-5]: ");
        filter = match filter_choice.as_str() {
            "2" => ProxyFilter::Http,
            "3" => ProxyFilter::Https,
            "4" => ProxyFilter::Socks4,
            "5" => ProxyFilter::Socks5,
            _ => ProxyFilter::All,
        };
    } else if mode == RunMode::LocalProxy {
        let path_input = prompt_input("📁 Enter proxy file path: ");
        if !path_input.trim().is_empty() {
            local_path = Some(path_input);
        }
        
        println!("\n🔍 Select Default Protocol:");
        println!("1) Auto-detect");
        println!("2) HTTP");
        println!("3) HTTPS");
        println!("4) SOCKS4");
        println!("5) SOCKS5");
        
        let proto_choice = prompt_input("Choice [1-5]: ");
        default_proto = match proto_choice.as_str() {
            "2" => "http".to_string(),
            "3" => "https".to_string(),
            "4" => "socks4".to_string(),
            "5" => "socks5h".to_string(),
            _ => "socks5h".to_string(),
        };
    }

    println!("\n🔧 Select Operation Method:");
    println!("1) Send SMS Invites (Full Process)");
    println!("2) Only Check App (No SMS)");
    
    let method_choice = prompt_input("Choice [1-2]: ");
    let use_send_invite = method_choice == "1";
    
    println!("\n⚙️ Configure Workers:");
    let workers_input = prompt_input("Number of workers [1-100]: ");
    let workers: usize = workers_input.parse().unwrap_or(10).clamp(1, 100);
    
    let concurrency_input = prompt_input("Concurrent requests per worker [1-20]: ");
    let concurrency: usize = concurrency_input.parse().unwrap_or(5).clamp(1, 20);

    // Initialize proxy manager if needed
    let proxy_manager = if mode == RunMode::Direct {
        None
    } else {
        Some(Arc::new(ProxyManager::new(
            mode,
            filter,
            local_path,
            default_proto,
        ).await?))
    };

    let prefixes = Arc::new(config.prefixes);
    let success_counter = Arc::new(Mutex::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start health check task for proxy mode
    if let Some(manager) = &proxy_manager {
        let health_manager = manager.clone();
        tokio::spawn(async move {
            loop {
                sleep(HEALTH_CHECK_INTERVAL).await;
                if let Err(e) = health_manager.health_check().await {
                    log_debug(format!("Health check failed: {}", e)).await;
                }
            }
        });
    }

    println!("\n🚀 Starting {} workers...", workers);
    let start_time = Instant::now();

    let mut handles = Vec::new();
    for worker_id in 0..workers {
        let token_clone = token.clone();
        let prefixes_clone = prefixes.clone();
        let success_clone = success_counter.clone();
        let shutdown_clone = shutdown_rx.clone();
        
        if mode == RunMode::Direct {
            let handle = tokio::spawn(async move {
                if let Err(e) = worker_direct(
                    worker_id,
                    token_clone,
                    prefixes_clone,
                    success_clone,
                    shutdown_clone,
                    debug_mode,
                    target,
                    use_send_invite,
                    concurrency,
                ).await {
                    log_debug(format!("Worker {} error: {}", worker_id, e)).await;
                }
            });
            handles.push(handle);
        } else if let Some(manager) = &proxy_manager {
            let manager_clone = manager.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = worker_proxy(
                    worker_id,
                    token_clone,
                    manager_clone,
                    prefixes_clone,
                    success_clone,
                    shutdown_clone,
                    debug_mode,
                    target,
                    use_send_invite,
                    concurrency,
                ).await {
                    log_debug(format!("Worker {} error: {}", worker_id, e)).await;
                }
            });
            handles.push(handle);
        }
    }

    // Monitor progress
    let monitor_counter = success_counter.clone();
    let monitor_tx = shutdown_tx.clone();
    
    tokio::spawn(async move {
        let mut last_count = 0;
        let mut last_time = Instant::now();
        
        loop {
            sleep(Duration::from_secs(5)).await;
            
            let current = *monitor_counter.lock().await;
            if current >= target {
                let _ = monitor_tx.send(true);
                break;
            }
            
            let elapsed = last_time.elapsed();
            if elapsed.as_secs() >= 10 {
                let speed = (current - last_count) as f32 / elapsed.as_secs_f32();
                println!("📊 Progress: {}/{} ({:.1}/sec)", current, target, speed);
                last_count = current;
                last_time = Instant::now();
            }
        }
    });

    // Wait for completion
    for handle in handles {
        let _ = handle.await;
    }

    let _ = shutdown_tx.send(true);
    let final_count = *success_counter.lock().await;
    let elapsed = start_time.elapsed();
    
    println!("\n🎉 Operation Completed!");
    println!("===============================");
    println!("✅ Total Successes: {}", final_count);
    println!("⏱️  Time Elapsed: {:.2?}", elapsed);
    println!("⚡ Average Speed: {:.1}/sec", final_count as f32 / elapsed.as_secs_f32());
    
    if debug_mode {
        log_debug(format!("=== SESSION ENDED: {} successes ===", final_count)).await;
    }

    Ok(())
}
