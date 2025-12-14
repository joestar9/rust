use colored::*;
use dashmap::DashMap;
use rand::seq::SliceRandom;
use rand::Rng;
use reqwest::{header, Client, Proxy};
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::sleep;

// ==========================================
//      تنظیمات هارد کد (HARDCODED AUTH)
// ==========================================
const FIXED_TOKEN: &str = "";
const FIXED_COOKIE: &str = "";
// ==========================================

const API_URL: &str = "https://my.irancell.ir/api/gift/v1/refer_a_friend";
const PREFIXES: [&str; 14] = [
    "0900", "0901", "0902", "0903", "0904", "0905", "0930", "0933", "0935", "0936", "0937", "0938",
    "0939", "0941",
];

const PROXY_SOURCES: [&str; 6] = [
    "https://raw.githubusercontent.com/ClearProxy/checked-proxy-list/refs/heads/main/socks5/raw/all.txt",
    "https://raw.githubusercontent.com/proxifly/free-proxy-list/refs/heads/main/proxies/protocols/socks5/data.txt",
    "https://raw.githubusercontent.com/vmheaven/VMHeaven-Free-Proxy-Updated/refs/heads/main/socks5.txt",
    "https://raw.githubusercontent.com/s0mecode/socks5-proxies/refs/heads/main/socks5_5000ms.txt",
    "https://raw.githubusercontent.com/vakhov/fresh-proxy-list/refs/heads/master/socks5.txt",
    "https://raw.githubusercontent.com/trio666/proxy-checker/refs/heads/main/socks5.txt"
];

// --- Helper Struct for Styling ---
struct Style;
impl Style {
    const RESET: &'static str = "\x1b[0m";
    const BOLD: &'static str = "\x1b[1m";
    
    fn header() {
        // Clear screen command for Windows/Linux
        print!("\x1b[2J\x1b[1;1H"); 
        println!("{}", "╔═══════════════════════════════════════════════════════════════╗".cyan().bold());
        println!("{}", "║    IRANCELL REFERRAL BOT - RUST ASYNC EDITION (NO ERRORS)     ║".cyan().bold());
        println!("{}", "╚═══════════════════════════════════════════════════════════════╝".cyan().bold());
    }
}

struct GlobalStats {
    success: AtomicUsize,
    logic_fail: AtomicUsize,
    proxy_fail: AtomicUsize,
    net_fail: AtomicUsize,
    limit: usize,
    start_time: std::time::Instant,
    is_running: AtomicBool,
}

struct ProxyState {
    queue: Vec<String>,
    active: HashSet<String>,
    dead: HashSet<String>,
}

struct AppState {
    stats: Arc<GlobalStats>,
    proxy_state: Arc<Mutex<ProxyState>>,
    patterns: Arc<DashMap<String, u32>>, 
    token: String,
    cookie: String,
    use_proxies: bool,
    base_headers: header::HeaderMap,
}

#[tokio::main]
async fn main() {
    setup_ui();

    // --- INPUT SECTION ---
    let token = if !FIXED_TOKEN.is_empty() {
        println!("{} Using Hardcoded Token.", ">>>".green());
        FIXED_TOKEN.to_string()
    } else {
        input("Enter Authorization Token (Bearer ...): ")
    };

    let cookie = if !FIXED_COOKIE.is_empty() {
        println!("{} Using Hardcoded Cookie.", ">>>".green());
        FIXED_COOKIE.to_string()
    } else {
        input("Enter Cookie: ")
    };

    let limit_str = input("Enter Limit (Total Success, e.g., 20000): ");
    let limit: usize = limit_str.parse().unwrap_or(100000);

    let workers_str = input("Enter Number of Workers (default 50): ");
    let worker_count: usize = workers_str.parse().unwrap_or(50);

    let use_proxy_in = input("Use Auto-Proxy List? (y/n): ");
    let use_proxies = use_proxy_in.to_lowercase() == "y";

    // --- PROXY FETCH ---
    let mut initial_proxies = Vec::new();
    if use_proxies {
        initial_proxies = fetch_proxies().await;
        if initial_proxies.is_empty() {
            println!("{}", "Warning: No proxies found. Switching to DIRECT mode.".red());
        }
    } else {
        println!("{}", "Skipping proxy setup. Running DIRECT.".yellow());
    }

    // --- PRE-COMPUTE HEADERS ---
    let mut headers = header::HeaderMap::new();
    headers.insert("Authorization", header::HeaderValue::from_str(&token).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("Cookie", header::HeaderValue::from_str(&cookie).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("User-Agent", header::HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36"));
    headers.insert("Origin", header::HeaderValue::from_static("https://my.irancell.ir"));
    headers.insert("Referer", header::HeaderValue::from_static("https://my.irancell.ir/"));
    headers.insert("Content-Type", header::HeaderValue::from_static("application/json"));

    // --- STATE INITIALIZATION ---
    let stats = Arc::new(GlobalStats {
        success: AtomicUsize::new(0),
        logic_fail: AtomicUsize::new(0),
        proxy_fail: AtomicUsize::new(0),
        net_fail: AtomicUsize::new(0),
        limit,
        start_time: std::time::Instant::now(),
        is_running: AtomicBool::new(true),
    });

    let proxy_state = Arc::new(Mutex::new(ProxyState {
        queue: initial_proxies,
        active: HashSet::new(),
        dead: HashSet::new(),
    }));

    let app_state = Arc::new(AppState {
        stats: stats.clone(),
        proxy_state: proxy_state.clone(),
        patterns: Arc::new(DashMap::new()),
        token,
        cookie,
        use_proxies,
        base_headers: headers,
    });

    println!("\n{} Starting {} workers (Optimized Async Engine)...", ">>>".green(), worker_count);
    println!("{} Target: {} Successful Referrals\n", ">>>".green(), limit);

    // --- SPAWN WORKERS ---
    let _semaphore = Arc::new(Semaphore::new(worker_count));

    for _ in 0..worker_count {
        let state = app_state.clone();
        tokio::spawn(async move {
            worker_loop(state).await;
        });
    }

    // --- MONITOR LOOP ---
    monitor_loop(app_state.clone()).await;
}

async fn worker_loop(state: Arc<AppState>) {
    let mut current_proxy: Option<String> = None;
    let mut proxy_strikes = 0;
    let mut client: Option<Client> = None;
    let mut forced_direct = false;

    loop {
        if !state.stats.is_running.load(Ordering::Relaxed) { break; }
        
        if state.stats.success.load(Ordering::Relaxed) >= state.stats.limit {
            state.stats.is_running.store(false, Ordering::Relaxed);
            break;
        }

        // --- PROXY LOGIC ---
        // If client is dead (None), we need to rebuild it.
        // If we are using proxies but don't have one, try to get one.
        
        if state.use_proxies {
            if current_proxy.is_none() && !forced_direct {
                let mut lock = state.proxy_state.lock().await;
                if let Some(p) = lock.queue.pop() {
                    lock.active.insert(p.clone());
                    current_proxy = Some(p);
                    proxy_strikes = 0;
                    client = None; // Force rebuild with new proxy
                } else if lock.active.is_empty() {
                    // Fallback to direct
                    forced_direct = true;
                    client = None; // Force rebuild without proxy
                }
            }
        }

        // Build Client if missing
        if client.is_none() {
            let mut builder = Client::builder()
                .timeout(Duration::from_secs(10))
                .tcp_nodelay(true)
                .danger_accept_invalid_certs(true);

            if let Some(ref p_url) = current_proxy {
                if let Ok(proxy) = Proxy::all(p_url) {
                    builder = builder.proxy(proxy);
                }
            }

            match builder.build() {
                Ok(c) => client = Some(c),
                Err(_) => {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }

        // Generate Number
        let num = generate_number(&state.patterns).await;

        let cli = client.as_ref().unwrap();
        let payload = serde_json::json!({
            "application_name": "NGMI",
            "friend_number": format!("98{}", &num[1..])
        });

        let req_res = cli.post(API_URL)
            .headers(state.base_headers.clone()) 
            .json(&payload)
            .send()
            .await;

        match req_res {
            Ok(resp) => {
                if resp.status().is_success() {
                    state.stats.success.fetch_add(1, Ordering::Relaxed);
                    update_pattern(&state.patterns, &num); 
                    proxy_strikes = 0;
                } else {
                    state.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                }
            },
            Err(_) => {
                // Network Fail
                if state.use_proxies && !forced_direct {
                    state.stats.proxy_fail.fetch_add(1, Ordering::Relaxed);
                    proxy_strikes += 1;
                    
                    if proxy_strikes >= 3 {
                        if let Some(dead_p) = current_proxy.take() {
                            let mut lock = state.proxy_state.lock().await;
                            lock.active.remove(&dead_p);
                            lock.dead.insert(dead_p);
                        }
                        client = None; // Force rebuild next loop
                    }
                } else {
                    state.stats.net_fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

async fn generate_number(patterns: &Arc<DashMap<String, u32>>) -> String {
    let mut rng = rand::thread_rng();
    let prefix = PREFIXES.choose(&mut rng).unwrap();
    
    if rng.gen_bool(0.85) {
        // Optimization: Don't iterate everything if not needed.
        // We just check a few shards or pick randomly if possible.
        // For simplicity and speed in this demo, strict filtering on DashMap can be slow
        // if it has millions of keys. We'll use a simplified random check or iterate if small.
        
        if !patterns.is_empty() {
             // Since we can't easily pick a random element from DashMap efficiently without iterating,
             // and iterating is slow, we will skip complex pattern matching for maximum CPU speed 
             // in this specific "max performance" version, OR we assume map is small (<3000).
             // Given the pruning logic, iterating is acceptable.
             
             // Try to find a pattern for this prefix
             let entry = patterns.iter().find(|r| r.key().starts_with(prefix));
             
             if let Some(r) = entry {
                let k = r.key();
                let parts: Vec<&str> = k.split('-').collect();
                if parts.len() > 1 {
                    let suffix = parts[1]; 
                    let rest_len = 7 - suffix.len();
                    let mut rest = String::with_capacity(rest_len);
                    for _ in 0..rest_len {
                        rest.push_str(&rng.gen_range(0..=9).to_string());
                    }
                    return format!("{}{}{}", prefix, suffix, rest);
                }
             }
        }
    }

    let mut suffix = String::with_capacity(7);
    for _ in 0..7 {
        suffix.push_str(&rng.gen_range(0..=9).to_string());
    }
    format!("{}{}", prefix, suffix)
}

fn update_pattern(patterns: &Arc<DashMap<String, u32>>, num: &str) {
    if num.len() < 7 { return; }
    let prefix = &num[0..4];
    let suffix = &num[4..7];
    let key = format!("{}-{}", prefix, suffix);
    
    *patterns.entry(key).or_insert(0) += 1;
    
    // Pruning
    if patterns.len() > 3000 {
        patterns.retain(|_, &mut v| v > 1); 
    }
}

async fn monitor_loop(state: Arc<AppState>) {
    loop {
        sleep(Duration::from_secs(1)).await;

        let s = state.stats.success.load(Ordering::Relaxed);
        let lf = state.stats.logic_fail.load(Ordering::Relaxed);
        let pf = state.stats.proxy_fail.load(Ordering::Relaxed);
        let nf = state.stats.net_fail.load(Ordering::Relaxed);
        let elapsed = state.stats.start_time.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 { s as f64 / elapsed } else { 0.0 };

        let proxy_info;
        if state.use_proxies {
            let lock = state.proxy_state.lock().await;
            let pool = lock.queue.len();
            let active = lock.active.len();
            let dead = lock.dead.len();
            
            if pool == 0 && active == 0 {
                proxy_info = format!(" {}WARNING: SWITCHED TO DIRECT{}", "!! ".red().blink(), " !!".red().blink());
            } else {
                proxy_info = format!(" (Pool:{} Active:{} Dead:{})", pool, active, dead).blue().to_string();
            }
        } else {
            proxy_info = " (MODE: DIRECT)".magenta().to_string();
        }

        print!("\r[ SUCCESS: {}/{} ] [ LOGIC FAIL: {} ] [ {} FAIL: {} ] [ SPEED: {:.1}/s ]{}", 
            s.to_string().green(), 
            state.stats.limit,
            lf.to_string().red(),
            if state.use_proxies { "PROXY".yellow() } else { "NET".yellow() },
            if state.use_proxies { pf } else { nf },
            rate.to_string().cyan(),
            proxy_info
        );
        io::stdout().flush().unwrap();

        if !state.stats.is_running.load(Ordering::Relaxed) {
            println!("\n\n{}", ">>> TARGET REACHED! DONE.".green().bold());
            break;
        }
    }
}

async fn fetch_proxies() -> Vec<String> {
    println!("{}", "[Proxy Manager] Downloading proxies...".yellow());
    let client = Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let mut collected = HashSet::new();

    for url in PROXY_SOURCES 
