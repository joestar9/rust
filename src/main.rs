use anyhow::Result;
use crossterm::{
    cursor,
    style::{self, Stylize},
    terminal::{self, Clear, ClearType},
    ExecutableCommand,
};
use dashmap::DashMap;
use parking_lot::Mutex;
use rand::prelude::*;
use reqwest::{header, Client, Proxy};
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

// دقیقاً همان یوزر ایجنت نسخه پایتون که کار می‌کرد
const STATIC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

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
    queue: VecDeque<String>,
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

// --- UI Logic ---
fn setup_terminal() {
    let mut stdout = io::stdout();
    let _ = stdout.execute(terminal::SetTitle("Irancell Bot - Stable Rust"));
    let _ = stdout.execute(Clear(ClearType::All));
    let _ = stdout.execute(cursor::MoveTo(0, 0));
    
    println!("{}", "╔═══════════════════════════════════════════════════════════════╗".cyan().bold());
    println!("{}", "║    IRANCELL BOT - FIXED & STABLE (NATIVE TLS)                 ║".cyan().bold());
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

    // --- CONFIG ---
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

    let limit: usize = input("Enter Limit (e.g., 20000): ").parse().unwrap_or(100000);
    let worker_count: usize = input("Number of Managers (default 50): ").parse().unwrap_or(50);
    let concurrency: usize = input("Concurrency per Manager (default 5): ").parse().unwrap_or(5);
    let use_proxies = input("Use Auto-Proxy List? (y/n): ").to_lowercase() == "y";

    // --- SETUP ---
    let mut initial_queue = VecDeque::new();
    if use_proxies {
        let list = fetch_proxies().await;
        if list.is_empty() {
            println!("{}", "Warning: No proxies found. Switching to DIRECT mode.".red());
        } else {
            initial_queue = list.into();
        }
    } else {
        println!("{}", "Running in DIRECT mode.".yellow());
    }

    // --- HEADERS (Exaclty matching Python) ---
    let mut headers = header::HeaderMap::new();
    headers.insert("Authorization", header::HeaderValue::from_str(&token).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("Cookie", header::HeaderValue::from_str(&cookie).unwrap_or(header::HeaderValue::from_static("")));
    headers.insert("User-Agent", header::HeaderValue::from_static(STATIC_USER_AGENT));
    headers.insert("Origin", header::HeaderValue::from_static("https://my.irancell.ir"));
    headers.insert("Referer", header::HeaderValue::from_static("https://my.irancell.ir/"));
    headers.insert("Content-Type", header::HeaderValue::from_static("application/json"));
    headers.insert("Accept", header::HeaderValue::from_static("application/json, text/plain, */*"));
    headers.insert("Accept-Language", header::HeaderValue::from_static("fa"));

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
        patterns: Arc::new(DashMap::with_capacity(5000)),
        token,
        cookie,
        use_proxies,
        base_headers: headers,
    });

    println!("\n{} Launching {} Managers x {} Concurrency...", ">>>".green(), worker_count, concurrency);
    
    // --- SPAWN MANAGERS ---
    for _ in 0..worker_count {
        let state = app_state.clone();
        tokio::spawn(async move {
            manager_loop(state, concurrency).await;
        });
    }

    // --- MONITOR ---
    monitor_loop(app_state).await;
    Ok(())
}

async fn manager_loop(state: Arc<AppState>, concurrency: usize) {
    let mut current_proxy: Option<String> = None;
    let mut client: Option<Client> = None;
    let mut forced_direct = false;

    // Use a Semaphore to control "In-Flight" requests
    let semaphore = Arc::new(Semaphore::new(concurrency));

    loop {
        if !state.stats.is_running.load(Ordering::Relaxed) { break; }
        if state.stats.success.load(Ordering::Relaxed) >= state.stats.limit {
            state.stats.is_running.store(false, Ordering::Relaxed);
            break;
        }

        // --- 1. PROXY ACQUISITION ---
        if state.use_proxies {
            if current_proxy.is_none() && !forced_direct {
                let mut lock = state.proxy_state.lock();
                if let Some(p) = lock.queue.pop_front() {
                    lock.active.insert(p.clone());
                    current_proxy = Some(p);
                    client = None; 
                } else if lock.active.is_empty() {
                    forced_direct = true;
                    client = None;
                }
            }
        }

        // --- 2. CLIENT SETUP (Native TLS for Compatibility) ---
        if client.is_none() {
            let mut builder = Client::builder()
                .timeout(Duration::from_secs(10))
                .tcp_nodelay(true)
                .pool_idle_timeout(Duration::from_secs(30))
                .pool_max_idle_per_host(concurrency)
                // Important: Default TLS (Native) matches Python requests
                .danger_accept_invalid_certs(true);

            if let Some(ref p_url) = current_proxy {
                if let Ok(proxy) = Proxy::all(p_url) {
                    builder = builder.proxy(proxy);
                }
            }

            match builder.build() {
                Ok(c) => client = Some(c),
                Err(_) => {
                    if let Some(dead) = current_proxy.take() {
                        let mut lock = state.proxy_state.lock();
                        lock.active.remove(&dead);
                        lock.dead.insert(dead);
                    }
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }

        // --- 3. PIPELINE REQUESTS ---
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, 
        };

        let cli = client.as_ref().unwrap().clone();
        let state_ref = state.clone();
        let num = generate_number(&state.patterns);
        let is_proxy = state.use_proxies && !forced_direct;

        tokio::spawn(async move {
            // Using serde_json::json! macro is safer and cleaner than custom struct for this API
            let payload = serde_json::json!({
                "application_name": "NGMI",
                "friend_number": format!("98{}", &num[1..])
            });

            // Use pre-computed headers, only clone the map (cheap enough in modern Rust)
            let res = cli.post(API_URL)
                .headers(state_ref.base_headers.clone()) 
                .json(&payload)
                .send()
                .await;

            match res {
                Ok(resp) => {
                    if resp.status().is_success() {
                        state_ref.stats.success.fetch_add(1, Ordering::Relaxed);
                        update_pattern(&state_ref.patterns, &num);
                    } else {
                        // 200 OK didn't come, so it's a Logic Fail
                        state_ref.stats.logic_fail.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    // Network error (Timeout, Proxy dead, etc)
                    if is_proxy {
                        state_ref.stats.proxy_fail.fetch_add(1, Ordering::Relaxed);
                    } else {
                        state_ref.stats.net_fail.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            drop(permit); 
        });
    }
}

fn generate_number(patterns: &DashMap<String, u32>) -> String {
    let mut rng = rand::thread_rng();
    let prefix = PREFIXES.choose(&mut rng).unwrap();
    let mut suffix = String::with_capacity(7);
    for _ in 0..7 {
        suffix.push_str(&rng.gen_range(0..=9).to_string().as_str());
    }
    format!("{}{}", prefix, suffix)
}

fn update_pattern(patterns: &DashMap<String, u32>, num: &str) {
    if num.len() < 7 { return; }
    let prefix = &num[0..4];
    let suffix = &num[4..7];
    let key = format!("{}-{}", prefix, suffix);
    *patterns.entry(key).or_insert(0) += 1;
    if rand::thread_rng().gen_bool(0.001) && patterns.len() > 3000 {
        patterns.retain(|_, v| *v > 1);
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
