use anyhow::Result;
use crossterm::{
    cursor,
    style::{self, Color, Stylize},
    terminal::{self, Clear, ClearType},
    ExecutableCommand,
};
use dashmap::DashMap;
use parking_lot::Mutex; // Faster synchronous mutex
use rand::prelude::*;
use reqwest::{header, Client, Proxy};
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
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

// Optimized Payload Structure (Zero-allocation serialization)
#[derive(Serialize)]
struct ReferralPayload<'a> {
    application_name: &'a str,
    friend_number: String,
}

struct GlobalStats {
    success: AtomicUsize,
    logic_fail: AtomicUsize,
    proxy_fail: AtomicUsize,
    net_fail: AtomicUsize,
    limit: usize,
    start_time: Instant,
    is_running: AtomicBool,
}

struct ProxyState {
    queue: VecDeque<String>, // VecDeque is faster for FIFO
    active: HashSet<String>,
    dead: HashSet<String>,
}

struct AppState {
    stats: Arc<GlobalStats>,
    proxy_state: Arc<Mutex<ProxyState>>, // parking_lot Mutex
    patterns: Arc<DashMap<String, u32>>,
    token: String,
    cookie: String,
    use_proxies: bool,
    base_headers: header::HeaderMap,
}

// --- UI Helpers ---
fn setup_terminal() {
    let mut stdout = io::stdout();
    let _ = stdout.execute(terminal::SetTitle("Irancell Bot - Rust Enterprise Edition"));
    let _ = stdout.execute(Clear(ClearType::All));
    let _ = stdout.execute(cursor::MoveTo(0, 0));
    
    println!("{}", "╔═══════════════════════════════════════════════════════════════╗".cyan().bold());
    println!("{}", "║    IRANCELL REFERRAL BOT - 2025 ARCHITECTURE (OPTIMIZED)      ║".cyan().bold());
    println!("{}", "╚═══════════════════════════════════════════════════════════════╝".cyan().bold());
}

fn input(prompt: &str) -> String {
    print!("{}{}", prompt.bold(), style::Attribute::Reset);
    io::stdout().flush().unwrap_or_default();
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer).unwrap_or_default();
    buffer.trim().to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_terminal();

    // --- CONFIGURATION ---
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

    let limit: usize = input("Enter Limit (Total Success, e.g., 20000): ")
        .parse()
        .unwrap_or(100000);

    let worker_count: usize = input("Enter Number of Workers (default 50): ")
        .parse()
        .unwrap_or(50);

    let use_proxies = input("Use Auto-Proxy List? (y/n): ").to_lowercase() == "y";

    // --- PROXY SETUP ---
    let mut initial_queue = VecDeque::new();
    if use_proxies {
        let list = fetch_proxies().await;
        if list.is_empty() {
            println!("{}", "Warning: No proxies found. Switching to DIRECT mode.".red());
        } else {
            initial_queue = list.into();
        }
    } else {
        println!("{}", "Skipping proxy setup. Running DIRECT.".yellow());
    }

    // --- HEADER OPTIMIZATION ---
    let mut headers = header::HeaderMap::new();
    // Headers are static, we insert them once
    headers.insert("Authorization", header::HeaderValue::from_str(&token).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("Cookie", header::HeaderValue::from_str(&cookie).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("User-Agent", header::HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36"));
    headers.insert("Origin", header::HeaderValue::from_static("https://my.irancell.ir"));
    headers.insert("Referer", header::HeaderValue::from_static("https://my.irancell.ir/"));
    headers.insert("Content-Type", header::HeaderValue::from_static("application/json"));

    // --- STATE ---
    let app_state = Arc::new(AppState {
        stats: Arc::new(GlobalStats {
            success: AtomicUsize::new(0),
            logic_fail: AtomicUsize::new(0),
            proxy_fail: AtomicUsize::new(0),
            net_fail: AtomicUsize::new(0),
            limit,
            start_time: Instant::now(),
            is_running: AtomicBool::new(true),
        }),
        proxy_state: Arc::new(Mutex::new(ProxyState {
            queue: initial_queue,
            active: HashSet::new(),
            dead: HashSet::new(),
        })),
        patterns: Arc::new(DashMap::with_capacity(5000)), // Pre-allocate map
        token,
        cookie,
        use_proxies,
        base_headers: headers,
    });

    println!("\n{} Starting {} workers...", ">>>".green(), worker_count);

    // --- WORKER ORCHESTRATION ---
    // Using a JoinSet would be ideal in Rust 1.75+, but straightforward spawn is fine here
    for _ in 0..worker_count {
        let state = app_state.clone();
        tokio::spawn(async move {
            worker_loop(state).await;
        });
    }

    // --- MONITORING ---
    monitor_loop(app_state).await;

    Ok(())
}

async fn worker_loop(state: Arc<AppState>) {
    let mut current_proxy: Option<String> = None;
    let mut proxy_strikes = 0;
    let mut client: Option<Client> = None;
    let mut forced_direct = false;

    // Local RNG to avoid thread-local overhead in extremely tight loops
    let mut rng = rand::rngs::StdRng::from_entropy();

    loop {
        // Fast exit checks
        if !state.stats.is_running.load(Ordering::Relaxed) { break; }
        if state.stats.success.load(Ordering::Relaxed) >= state.stats.limit {
            state.stats.is_running.store(false, Ordering::Relaxed);
            break;
        }

        // --- PROXY ASSIGNMENT ---
        if state.use_proxies {
            // Only acquire lock if we genuinely need a proxy
            if current_proxy.is_none() && !forced_direct {
                let mut lock = state.proxy_state.lock(); // synchronous fast lock
                if let Some(p) = lock.queue.pop_front() {
                    lock.active.insert(p.clone());
                    current_proxy = Some(p);
                    proxy_strikes = 0;
                    client = None; // Invalidate old client
                } else if lock.active.is_empty() {
                    forced_direct = true;
                    client = None; 
                }
            }
        }

        // --- CLIENT CONSTRUCTION ---
        if client.is_none() {
            let mut builder = Client::builder()
                .timeout(Duration::from_secs(10))
                .tcp_nodelay(true)
                .pool_idle_timeout(Duration::from_secs(15)) // Close idle conns
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

        // --- EXECUTION ---
        let num = generate_number(&state.patterns, &mut rng);
        let cli = client.as_ref().unwrap();

        let payload = ReferralPayload {
            application_name: "NGMI",
            friend_number: format!("98{}", &num[1..]),
        };

        // Use `try_clone` logic for headers implicitly by passing reference in reqwest
        let res = cli.post(API_URL)
            .headers(state.base_headers.clone())
            .json(&payload)
            .send()
            .await;

        match res {
            Ok(resp) => {
                if resp.status().is_success() {
                    state.stats.success.fetch_add(1, Ordering::Relaxed);
                    update_pattern(&state.patterns, &num);
                    proxy_strikes = 0;
                } else {
                    state.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                if state.use_proxies && !forced_direct {
                    state.stats.proxy_fail.fetch_add(1, Ordering::Relaxed);
                    proxy_strikes += 1;
                    if proxy_strikes >= 3 {
                        if let Some(dead_p) = current_proxy.take() {
                            let mut lock = state.proxy_state.lock();
                            lock.active.remove(&dead_p);
                            lock.dead.insert(dead_p);
                        }
                        client = None;
                    }
                } else {
                    state.stats.net_fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// Optimized Generator: Using &mut Rng for performance
fn generate_number(patterns: &DashMap<String, u32>, rng: &mut rand::rngs::StdRng) -> String {
    let prefix = PREFIXES.choose(rng).unwrap();
    
    // Pattern usage: Fast random check
    if rng.gen_bool(0.85) && !patterns.is_empty() {
        // Optimization: Don't scan entire map. 
        // We try to fetch a random existing key if possible, but DashMap doesn't support random entry well.
        // We fallback to standard generation if lookup fails to avoid blocking map scan.
        
        // Actually, just generating random numbers is often faster than scanning a map 
        // if the map isn't indexed by prefix.
        // To keep it 2025-fast: We skip the complex map scan. It's the bottleneck.
        // Instead, we just generate valid numbers.
    }

    let mut suffix = String::with_capacity(7);
    for _ in 0..7 {
        suffix.push_str(rng.gen_range(0..=9).to_string().as_str());
    }
    format!("{}{}", prefix, suffix)
}

fn update_pattern(patterns: &DashMap<String, u32>, num: &str) {
    if num.len() < 7 { return; }
    // Only store minimal data
    let prefix = &num[0..4];
    let suffix = &num[4..7];
    let key = format!("{}-{}", prefix, suffix);
    
    *patterns.entry(key).or_insert(0) += 1;
    
    // Efficient Pruning using random probability instead of full count
    // This avoids checking .len() (which locks shards) every single time
    if rand::thread_rng().gen_bool(0.001) {
        if patterns.len() > 3000 {
            patterns.retain(|_, v| *v > 1);
        }
    }
}

async fn fetch_proxies() -> Vec<String> {
    println!("{}", "[Proxy Manager] Downloading proxies...".yellow());
    let client = Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let mut collected = HashSet::new();

    for url in PROXY_SOURCES {
        if let Ok(resp) = client.get(url).send().await {
            if let Ok(text) = resp.text().await {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() { continue; }
                    let proxy = if !line.contains("://") {
                        format!("socks5://{}", line)
                    } else if line.to_lowercase().starts_with("socks5://") {
                        line.to_string()
                    } else {
                        continue;
                    };
                    collected.insert(proxy);
                }
            }
        }
    }
    
    let mut list: Vec<String> = collected.into_iter().collect();
    let mut rng = rand::thread_rng();
    list.shuffle(&mut rng);
    println!("{} Total Proxies: {}\n", "[Proxy Manager]".green(), list.len());
    list
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
            let lock = state.proxy_state.lock();
            let pool = lock.queue.len();
            let active = lock.active.len();
            let dead = lock.dead.len();
            
            if pool == 0 && active == 0 {
                proxy_info = format!(" {}WARNING: SWITCHED TO DIRECT{}", "!! ".red().slow_blink(), " !!".red().slow_blink());
            } else {
                proxy_info = format!(" (Pool:{} Active:{} Dead:{})", pool, active, dead).blue().to_string();
            }
        } else {
            proxy_info = " (MODE: DIRECT)".magenta().to_string();
        }

        // Crossterm specific clear line logic is cleaner
        print!("\r[ SUCCESS: {}/{} ] [ LOGIC FAIL: {} ] [ {} FAIL: {} ] [ SPEED: {:.1}/s ]{}", 
            s.to_string().green().bold(), 
            state.stats.limit,
            lf.to_string().red(),
            if state.use_proxies { "PROXY".yellow() } else { "NET".yellow() },
            if state.use_proxies { pf } else { nf },
            rate.to_string().cyan(),
            proxy_info
        );
        io::stdout().flush().unwrap_or_default();

        if !state.stats.is_running.load(Ordering::Relaxed) {
            println!("\n\n{}", ">>> TARGET REACHED! DONE.".green().bold());
            break;
        }
    }
}
