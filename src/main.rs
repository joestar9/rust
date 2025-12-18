use anyhow::{anyhow, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rand::prelude::*;
use rquest::{Client, Proxy, Impersonate}; // استفاده از rquest
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, watch};
use tokio::time::sleep;

// --- CONSTANTS ---
const CONFIG_FILE: &str = "config.json";
const LOG_FILE: &str = "debug.log";
const API_CHECK_APP: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const API_SEND_INVITE: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend/notify";
const SOURCE_OF_SOURCES_URL: &str = "https://raw.githubusercontent.com/joestar9/jojo/refs/heads/main/proxy_links.json";
const MAX_RETRIES_BEFORE_SWITCH: usize = 3;
const CHARSET_NUMBERS: &[u8] = b"0123456789";

// --- ENUMS & STRUCTS ---

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
    Socks5,
}

#[derive(Clone, PartialEq)]
enum ProxyStatus {
    Healthy,
    SoftFail,
    HardFail,
    GlobalCoolDown,
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

// --- UTILS ---

fn generate_random_suffix() -> String {
    let mut rng = rand::rng();
    (0..7)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET_NUMBERS.len());
            CHARSET_NUMBERS[idx] as char
        })
        .collect()
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

// --- PROXY MANAGER ---

struct ProxyManager {
    pool: Arc<Mutex<Vec<String>>>,
    mode: RunMode,
    token: String,
    filter: ProxyFilter,
    local_path: String,
    local_proto: String,
    is_refilling: Arc<AtomicBool>,
}

impl ProxyManager {
    fn new(mode: RunMode, token: String, filter: ProxyFilter, local_path: String, local_proto: String) -> Self {
        Self {
            pool: Arc::new(Mutex::new(Vec::new())),
            mode,
            token,
            filter,
            local_path,
            local_proto,
            is_refilling: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn get_proxy(&self) -> Option<String> {
        loop {
            let mut lock = self.pool.lock().await;
            if let Some(proxy) = lock.pop() {
                return Some(proxy);
            }
            drop(lock);

            if self.mode == RunMode::Direct { return None; }
            self.trigger_refill_if_needed().await;
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn trigger_refill_if_needed(&self) {
        if self.is_refilling.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            let new_proxies = match self.mode {
                RunMode::AutoProxy => self.fetch_online_proxies().await,
                RunMode::LocalProxy => self.read_local_proxies().await,
                _ => Ok(vec![]),
            };

            match new_proxies {
                Ok(mut list) => {
                    if !list.is_empty() {
                        let mut lock = self.pool.lock().await;
                        lock.append(&mut list);
                    } else {
                        sleep(Duration::from_secs(5)).await;
                    }
                },
                Err(_) => { sleep(Duration::from_secs(5)).await; }
            }
            self.is_refilling.store(false, Ordering::SeqCst);
        }
    }

    async fn fetch_online_proxies(&self) -> Result<Vec<String>> {
        // برای دانلود لیست پراکسی از کلاینت معمولی استفاده می‌کنیم
        let client = Client::builder().build()?; 
        let json_text = client.get(SOURCE_OF_SOURCES_URL).send().await?.text().await?;
        let sources: ProxySourceConfig = serde_json::from_str(&json_text)?;

        let mut raw_proxies = HashSet::new();
        let mut tasks = Vec::new();

        let spawn_dl = |url: String, proto: &'static str, c: Client| {
            tokio::spawn(async move {
                let mut found = Vec::new();
                if let Ok(resp) = c.get(&url).timeout(Duration::from_secs(10)).send().await {
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

        match self.filter {
            ProxyFilter::All => {
                for url in sources.http { tasks.push(spawn_dl(url, "http", client.clone())); }
                for url in sources.https { tasks.push(spawn_dl(url, "https", client.clone())); }
                for url in sources.socks5 { tasks.push(spawn_dl(url, "socks5h", client.clone())); }
            },
            ProxyFilter::Socks5 => for url in sources.socks5 { tasks.push(spawn_dl(url, "socks5h", client.clone())); },
            _ => {}, 
        }

        for task in tasks {
            if let Ok(proxies) = task.await {
                for p in proxies { raw_proxies.insert(p); }
            }
        }
        let mut final_list: Vec<String> = raw_proxies.into_iter().collect();
        final_list.shuffle(&mut rand::rng());
        Ok(final_list)
    }

    async fn read_local_proxies(&self) -> Result<Vec<String>> {
        let path = self.local_path.clone();
        let proto = self.local_proto.clone();
        tokio::task::spawn_blocking(move || {
            let file = File::open(&path).context("File not found")?;
            let reader = BufReader::new(file);
            let mut unique = HashSet::new();
            for line in reader.lines() {
                if let Ok(l) = line {
                    let p = l.trim();
                    if !p.is_empty() { unique.insert(format_proxy_url(p, &proto)); }
                }
            }
            let mut list: Vec<String> = unique.into_iter().collect();
            list.shuffle(&mut rand::rng());
            Ok(list)
        }).await?
    }
}

// --- WORKER ---

async fn build_client(token: &str, proxy: Option<Proxy>) -> Result<Client> {
    // rquest به طور خودکار هدرهای کروم را تنظیم میکند
    // اما هدرهای اختصاصی ایرانسل را باید دستی اضافه کنیم
    let mut headers = rquest::header::HeaderMap::new();
    headers.insert("Accept-Language", "fa-IR,fa;q=0.9,en-US;q=0.8,en;q=0.7".parse().unwrap());
    headers.insert("Origin", "https://my.irancell.ir".parse().unwrap());
    headers.insert("x-app-version", "9.62.0".parse().unwrap());
    headers.insert("Authorization", rquest::header::HeaderValue::from_str(token)?);
    headers.insert("Content-Type", "application/json".parse().unwrap());

    let mut builder = Client::builder()
        .impersonate(Impersonate::Chrome126) // شبیه سازی کروم ۱۲۶
        .default_headers(headers)
        .cookie_store(true)
        .timeout(Duration::from_secs(15));

    if let Some(p) = proxy {
        builder = builder.proxy(p);
    }

    builder.build().context("Failed to build client")
}

async fn run_worker(
    id: usize,
    manager: Arc<ProxyManager>,
    token: String,
    prefixes: Arc<Vec<String>>,
    counters: Arc<(AtomicUsize, AtomicUsize, AtomicUsize)>,
    target: usize,
    mut shutdown: watch::Receiver<bool>,
    pb: ProgressBar, 
    send_invite: bool,
    req_per_proxy: usize
) {
    let mut client: Option<Client> = None;
    let mut current_proxy = String::new();
    let mut fails = 0;
    let mut reqs = 0;

    loop {
        if *shutdown.borrow() { break; }

        // Global Backoff
        if counters.2.load(Ordering::Relaxed) > 20 {
            sleep(Duration::from_secs(5)).await;
            if id == 0 { counters.2.store(0, Ordering::Relaxed); }
        }

        if client.is_none() || fails >= MAX_RETRIES_BEFORE_SWITCH || reqs >= req_per_proxy {
            match manager.mode {
                RunMode::Direct => {
                     if let Ok(c) = build_client(&token, None).await {
                         client = Some(c);
                         current_proxy = "DIRECT".to_string();
                         fails = 0; reqs = 0;
                     } else { sleep(Duration::from_secs(5)).await; }
                },
                _ => {
                    if let Some(p_addr) = manager.get_proxy().await {
                        if let Ok(proxy_obj) = Proxy::all(&p_addr) {
                            if let Ok(c) = build_client(&token, Some(proxy_obj)).await {
                                client = Some(c);
                                current_proxy = p_addr;
                                fails = 0; reqs = 0;
                            } else { fails += 1; }
                        }
                    } 
                }
            }
        }

        if let Some(c) = &client {
             if counters.0.load(Ordering::Relaxed) >= target { break; }

             let prefix = prefixes.choose(&mut rand::rng()).unwrap();
             let phone = format!("98{}{}", prefix, generate_random_suffix());

             let status = perform_invite(c, phone, &counters, target, &pb, send_invite).await;
             
             reqs += 1;
             match status {
                 ProxyStatus::Healthy => { fails = 0; },
                 ProxyStatus::SoftFail => { 
                     fails += 1; 
                     counters.1.fetch_add(1, Ordering::Relaxed);
                 },
                 ProxyStatus::HardFail => { 
                     fails = MAX_RETRIES_BEFORE_SWITCH + 1; 
                     counters.1.fetch_add(1, Ordering::Relaxed);
                 },
                 ProxyStatus::GlobalCoolDown => {
                     fails = MAX_RETRIES_BEFORE_SWITCH + 1;
                     counters.2.fetch_add(1, Ordering::Relaxed);
                 }
             }
        }
    }
}

async fn perform_invite(
    client: &Client,
    phone: String,
    counters: &Arc<(AtomicUsize, AtomicUsize, AtomicUsize)>,
    target: usize,
    pb: &ProgressBar,
    send_sms: bool,
) -> ProxyStatus {
    let data = InviteData { application_name: "NGMI".to_string(), friend_number: phone };

    match client.post(API_CHECK_APP).json(&data).send().await {
        Ok(resp) => {
            let status = resp.status();
            
            if status == 429 { return ProxyStatus::GlobalCoolDown; }
            if status == 403 || status == 407 { return ProxyStatus::HardFail; }
            
            if status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                if text.contains(r#""message":"done""#) {
                    if send_sms {
                        match client.post(API_SEND_INVITE).json(&data).send().await {
                            Ok(resp2) => {
                                if resp2.status().is_success() && resp2.text().await.unwrap_or_default().contains("done") {
                                    counters.0.fetch_add(1, Ordering::SeqCst);
                                    pb.inc(1);
                                    return ProxyStatus::Healthy;
                                }
                                return ProxyStatus::SoftFail;
                            },
                            Err(_) => return ProxyStatus::HardFail,
                        }
                    } else {
                        counters.0.fetch_add(1, Ordering::SeqCst);
                        pb.inc(1);
                        return ProxyStatus::Healthy;
                    }
                }
                return ProxyStatus::Healthy; 
            }
            return ProxyStatus::SoftFail;
        },
        Err(_) => return ProxyStatus::HardFail,
    }
}

// --- MAIN ---

fn read_config() -> Result<AppConfig> {
    if !std::path::Path::new(CONFIG_FILE).exists() { return Err(anyhow!("config.json not found")); }
    let file = File::open(CONFIG_FILE)?;
    let reader = BufReader::new(file);
    Ok(serde_json::from_reader(reader)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = read_config()?;
    
    // Log file reset
    { File::create(LOG_FILE)?; }

    let token = match config.token {
        Some(t) if !t.trim().is_empty() => t,
        _ => prompt_input("🔑 Enter Token: "),
    };

    println!("📱 Prefixes: {}", config.prefixes.len());
    let target_count: usize = prompt_input("🎯 Target: ").parse().unwrap_or(1000);

    println!("\n✨ Mode: 1)Direct 2)Auto 3)Local");
    let mode_choice = prompt_input("Choice: ");

    let mut run_mode = RunMode::Direct;
    let mut proxy_filter = ProxyFilter::All;
    let mut local_path = "socks5.txt".to_string();

    match mode_choice.as_str() {
        "2" => {
            run_mode = RunMode::AutoProxy;
            println!("🔍 Filter: 1)All 5)SOCKS5");
            if prompt_input("Choice: ") == "5" { proxy_filter = ProxyFilter::Socks5; }
        },
        "3" => {
            run_mode = RunMode::LocalProxy;
            local_path = prompt_input("📁 File Path: ");
        },
        _ => {}
    }

    let send_invite = prompt_input("🔧 Send SMS? (y/n): ") == "y";
    let req_per_proxy: usize = prompt_input("⚡ Reqs/Proxy: ").parse().unwrap_or(5);
    let worker_count: usize = prompt_input("👷 Workers: ").parse().unwrap_or(50);

    let manager = Arc::new(ProxyManager::new(run_mode, token.clone(), proxy_filter, local_path, "socks5h".to_string()));
    
    manager.trigger_refill_if_needed().await;
    if run_mode != RunMode::Direct { 
        println!("⏳ Fetching proxies..."); 
        sleep(Duration::from_secs(3)).await; 
    }

    let multi_pb = MultiProgress::new();
    let sty = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) \n{msg}",
    ).unwrap().progress_chars("#>-");

    let pb = multi_pb.add(ProgressBar::new(target_count as u64));
    pb.set_style(sty);

    let counters = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let prefixes = Arc::new(config.prefixes);

    let mut handles = Vec::new();
    for id in 0..worker_count {
        let mgr = manager.clone();
        let tok = token.clone();
        let pre = prefixes.clone();
        let cnt = counters.clone();
        let rx = shutdown_rx.clone();
        let pb_clone = pb.clone();
        
        handles.push(tokio::spawn(async move {
            run_worker(id, mgr, tok, pre, cnt, target_count, rx, pb_clone, send_invite, req_per_proxy).await;
        }));
    }

    let monitor_cnt = counters.clone();
    let monitor_pb = pb.clone();
    tokio::spawn(async move {
        let start = Instant::now();
        loop {
            sleep(Duration::from_millis(500)).await;
            let succ = monitor_cnt.0.load(Ordering::Relaxed);
            let fails = monitor_cnt.1.load(Ordering::Relaxed);
            let errors_429 = monitor_cnt.2.load(Ordering::Relaxed);
            
            let elapsed_mins = start.elapsed().as_secs_f64() / 60.0;
            let rpm = if elapsed_mins > 0.0 { (succ as f64 / elapsed_mins) as u64 } else { 0 };

            let msg = format!(
                "✅ Success: {} | ❌ Fails: {} | ⚠️ 429s: {} | 🚀 Speed: {} RPM",
                succ, fails, errors_429, rpm
            );
            monitor_pb.set_message(msg);
        }
    });

    loop {
        sleep(Duration::from_secs(1)).await;
        if counters.0.load(Ordering::Relaxed) >= target_count {
            let _ = shutdown_tx.send(true);
            break;
        }
    }

    pb.finish_with_message("🏁 Done!");
    for h in handles { let _ = h.await; }
    
    println!("Session Finished.");
    Ok(())
}
