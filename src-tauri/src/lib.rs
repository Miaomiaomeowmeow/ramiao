// RAMeow 内存喵 — by Miaomiao (github.com/Miaomiaomeowmeow) · CC BY-NC-SA 4.0
// 内存看板桌面版：读真实内存 + 规则层找“没人管”的网页服务 + 托盘 / 快捷键 / 开机启动
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalPosition, Manager};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::ShortcutState;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
// 开发时常见的“会开网页服务”的程序
const DEV: &[&str] = &["node", "python", "pythonw", "deno", "bun", "java", "ruby", "php", "dotnet", "uvicorn"];
// 启动壳：往上找主人时跳过它们
const SHELL: &[&str] = &["cmd", "bash", "sh", "powershell", "pwsh", "conhost", "zsh", "fish", "login", "wsl", "uv", "uvx", "npx"];
// 系统程序：不给“结束”
const SYSTEM: &[&str] = &[
    "system", "registry", "smss", "csrss", "wininit", "services", "lsass", "svchost", "winlogon", "dwm", "explorer",
    "fontdrvhost", "memory compression", "msmpeng", "securityhealthservice", "spoolsv", "audiodg", "sihost", "taskhostw",
    "ctfmon", "searchhost", "startmenuexperiencehost", "runtimebroker", "kernel_task", "launchd", "windowserver",
];

struct Watch {
    sys: System,
    first_seen: HashMap<u32, Instant>,
    last_conn: HashMap<u32, Instant>,
    // Claude Code 对话：上一次看到的 CPU 累计用量、最后一次“在干活”的时间
    // 第三个 = 从开始看以来有没有真的看到它干过活（没看到过就不算“正在用”）
    cc_busy: HashMap<u32, (u64, Instant, bool)>,
    // 每个进程是谁：只在第一次见到时认一次
    who: HashMap<u32, Who>,
}
struct AppState(Mutex<Watch>);
// 看门员：是否到 1 GB 自动停 AI 开的程序（设置里能改成只提醒）
static GUARD_AUTO: AtomicBool = AtomicBool::new(true);

// 电脑的地区语言（界面默认跟它走；有的人系统界面是英文、地区是中文）
#[tauri::command]
fn sys_lang() -> String {
    #[cfg(windows)]
    {
        let mut buf = [0u16; 85];
        let n = unsafe { windows_sys::Win32::Globalization::GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n > 1 {
            return String::from_utf16_lossy(&buf[..(n - 1) as usize]);
        }
    }
    std::env::var("LANG").unwrap_or_default()
}

#[tauri::command]
fn set_guard(auto: bool) {
    GUARD_AUTO.store(auto, Ordering::Relaxed);
}

#[derive(Serialize, Clone)]
struct GuardEv {
    level: u8,
    avail: f64,
    eta: Option<f64>,
    who: Option<String>,
    conv: Option<String>,
    agent: Option<String>,
    ai: bool,
    stopped: Option<String>,
    freed: f64,
}

fn avail_gb() -> Option<f64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        unsafe {
            let mut m: MEMORYSTATUSEX = std::mem::zeroed();
            m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut m) != 0 {
                return Some(m.ullAvailPhys as f64 / GB);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let mut s = System::new();
        s.refresh_memory();
        Some(s.available_memory() as f64 / GB)
    }
}

fn guard_loop(app: AppHandle) {
    let mut hist: VecDeque<(Instant, f64)> = VecDeque::new();
    let mut prev: HashMap<u32, u64> = HashMap::new();
    let mut prev_at = Instant::now() - Duration::from_secs(5);
    let mut cool = Instant::now() - Duration::from_secs(60);
    let mut last_emit = Instant::now() - Duration::from_secs(5);
    let mut last_level = 0u8;
    // 防来回跳（照 Datadog / Nagios 的做法）：连着 3 秒都快满才报警；涨回 stop+0.7 以上、10 秒不再往下掉才解除；真不到 1 GB 马上报
    let mut since_up: Option<Instant> = None;
    let mut since_down: Option<Instant> = None;
    // 涨得最快的那个：(pid, 名字, 对话名, 是不是 AI 开的, 哪家 AI)
    let mut culprit: Option<(Pid, String, Option<String>, bool, Option<String>)> = None;
    // 停的线：默认 1 GB（测试时可以用环境变量 MEMO_GUARD_STOP_GB 调高，免得真把电脑压到 1 GB）
    let stop: f64 = std::env::var("MEMO_GUARD_STOP_GB").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let Some(a) = avail_gb() else { continue };
        let now = Instant::now();
        hist.push_back((now, a));
        while hist.front().map(|x| now.duration_since(x.0).as_secs_f64() > 10.0).unwrap_or(false) {
            hist.pop_front();
        }
        let (t0, a0) = *hist.front().unwrap();
        let dt = now.duration_since(t0).as_secs_f64();
        let slope = if dt >= 2.0 { (a - a0) / dt } else { 0.0 };
        let eta = if slope < -0.02 && a > stop { Some((a - stop) / -slope) } else { None };
        let raw = if a < stop { 2 } else if a < stop + 0.5 || eta.map(|e| e < 60.0).unwrap_or(false) { 1 } else { 0 };
        let calm = a > stop + 0.7 && eta.is_none();
        let up = raw > last_level;
        let down = raw < last_level && (raw > 0 || calm);
        if !up {
            since_up = None;
        }
        if !down {
            since_down = None;
        }
        let level = if raw == 2 {
            2
        } else if up && now.duration_since(*since_up.get_or_insert(now)) >= Duration::from_secs(3) {
            raw
        } else if down && now.duration_since(*since_down.get_or_insert(now)) >= Duration::from_secs(10) {
            raw
        } else {
            last_level
        };
        // 有危险时每秒找一次“谁在涨得最快”
        if level > 0 && prev_at.elapsed() >= Duration::from_secs(1) {
            let st = app.state::<AppState>();
            let mut g = st.0.lock().unwrap();
            g.sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_memory()
                    .with_exe(UpdateKind::OnlyIfNotSet)
                    .with_cmd(UpdateKind::OnlyIfNotSet)
                    .with_environ(UpdateKind::OnlyIfNotSet),
            );
            let me = std::process::id();
            let mine = descendants(&g.sys, Pid::from_u32(me));
            let mut best: Option<(Pid, u64)> = None;
            let mut next = HashMap::new();
            for (pid, p) in g.sys.processes() {
                let m = p.memory();
                next.insert(pid.as_u32(), m);
                if pid.as_u32() == me || mine.contains(pid) || SYSTEM.contains(&stem(&p.name().to_string_lossy()).as_str()) {
                    continue;
                }
                let grow = m.saturating_sub(*prev.get(&pid.as_u32()).unwrap_or(&m));
                if grow > best.map(|b| b.1).unwrap_or(32 * 1024 * 1024) {
                    best = Some((*pid, grow));
                }
            }
            if let Some((pid, _)) = best {
                let name = g.sys.process(pid).map(|p| pretty(&stem(&p.name().to_string_lossy()))).unwrap_or_default();
                let tag = tag_of(&g.sys, pid);
                let conv = tag.as_ref().and_then(|t| tag_label(&g.sys, t));
                culprit = Some((pid, name, conv, tag.is_some(), tag.map(|t| t.agent)));
            }
            prev = next;
            prev_at = now;
        }
        if level == 0 {
            culprit = None;
        }
        let mut stopped = None;
        let mut freed = 0.0;
        // 低于 1 GB：自动停掉 AI 开的、涨得最快的那个（连它开的一起）；你自己开的只提醒
        if level == 2 && GUARD_AUTO.load(Ordering::Relaxed) && cool.elapsed() > Duration::from_secs(10) {
            if let Some((pid, name, _, true, _)) = culprit.clone() {
                let st = app.state::<AppState>();
                let g = st.0.lock().unwrap();
                let mut all = descendants(&g.sys, pid);
                all.push(pid);
                for x in all {
                    if let Some(p) = g.sys.process(x) {
                        freed += p.memory() as f64 / GB;
                        let _ = p.kill();
                    }
                }
                stopped = Some(name);
                cool = now;
            }
        }
        if level != last_level || stopped.is_some() || (level > 0 && last_emit.elapsed() >= Duration::from_secs(1)) {
            let (who, conv, ai, agent) = culprit.clone().map(|c| (Some(c.1), c.2, c.3, c.4)).unwrap_or((None, None, false, None));
            let _ = app.emit("guard", GuardEv { level, avail: a, eta, who, conv, agent, ai, stopped, freed });
            last_emit = now;
        }
        last_level = level;
    }
}

// 机器在窗口里占的那一块（逻辑像素）；外面透明的地方让鼠标点穿
struct Hit(Mutex<(f64, f64, f64, f64)>);

#[tauri::command]
fn set_hit(hit: tauri::State<Hit>, x: f64, y: f64, w: f64, h: f64) {
    *hit.0.lock().unwrap() = (x, y, w, h);
}

#[derive(Serialize)]
struct Why {
    k: &'static str,
    v: String,
}

#[derive(Serialize)]
struct Group {
    id: String,
    name: String,
    name_en: Option<String>,
    n: usize,
    ram: f64,
    vm: f64,
    pids: Vec<u32>,
    exe: Option<String>,
    since: u64,
    sys: bool,
    score: u8,
    why: Vec<Why>,
    // AI 开的、对话已经关了：开它的对话叫什么、跳过去的链接
    conv: Option<String>,
    link: Option<String>,
    title: Option<String>,
    port: Option<u16>,
    #[serde(skip)]
    cc_kids: usize,
}

#[derive(Serialize)]
struct Snap {
    total: f64,
    used: f64,
    commit_used: f64,
    commit_limit: f64,
    sys_ram: f64,
    groups: Vec<Group>,
    chats: Vec<Chat>,
}

// ── AI 工具认人表（agents.json，用户可以在设置文件夹里放一份同格式的 agents.json 加行 / 改行）──
// 认法照 Vercel detect-agent：各家 AI 给自己跑的命令贴环境变量标记，子孙都继承；对话名照 AgentSight 读各家自己的记录
#[derive(Deserialize, Clone, Default)]
#[serde(default, rename_all = "camelCase")]
struct Match {
    exe: Vec<String>,
    path: Vec<String>,
    args: Vec<String>,
    not_args: Vec<String>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, rename_all = "camelCase")]
struct Agent {
    id: String,
    name: String,
    env: Vec<String>,
    ai_agent: Vec<String>,
    thread_env: String,
    name_env: String,
    name_index: String,
    conv: Vec<Match>,
    host: Vec<Match>,
    app: Vec<Match>,
    app_name: String,
    app_name_en: String,
    // 开出来的东西身上带着的对话编号（对话关了也还在）→ 去 titleDir/titleFile 里找 titleKey 就是对话名
    session_env: String,
    title_dir: String,
    title_file: String,
    title_key: String,
    // 跳到对话的链接：linkEnv 里的编号填进 link 的 {id}
    link_env: String,
    link: String,
    // 结束一个对话会怎样、怎么接着聊：App 里的（endNote）/ 终端里的（cliNote）；拿不准就不写，界面只说“会打断它正在做的事”
    end_note: String,
    end_note_en: String,
    cli_note: String,
    cli_note_en: String,
}

const BUILTIN_AGENTS: &str = include_str!("../agents.json");
// 表里的顺序 = 标记重叠时谁优先：越里层越具体（Codex 在 Claude 里跑，身上两家标记都有，算 Codex 的）
static AGENTS: std::sync::LazyLock<std::sync::RwLock<Vec<Agent>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(serde_json::from_str(BUILTIN_AGENTS).unwrap_or_default()));

// 用户自己的表：同 id 换掉，新 id 加在最前面（最优先）
fn load_user_agents(file: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(file) else { return };
    let Ok(mine) = serde_json::from_str::<Vec<Agent>>(&text) else { return };
    let mut all = AGENTS.write().unwrap();
    for a in mine.into_iter().rev() {
        all.retain(|x| x.id != a.id);
        all.insert(0, a);
    }
}

fn lower_path(p: &sysinfo::Process) -> String {
    p.exe().map(|e| e.to_string_lossy().to_lowercase().replace('\\', "/")).unwrap_or_default()
}

impl Match {
    fn hit(&self, p: &sysinfo::Process) -> bool {
        self.hit_in(p, false)
    }
    // keep_parts = true：App 本身的页面、显卡这些小进程也算（认 App 用）
    fn hit_in(&self, p: &sysinfo::Process, keep_parts: bool) -> bool {
        if self.exe.is_empty() && self.path.is_empty() && self.args.is_empty() {
            return false;
        }
        let s = stem(&p.name().to_string_lossy());
        if !self.exe.is_empty() && !self.exe.iter().any(|e| e.to_lowercase() == s) {
            return false;
        }
        if !self.path.is_empty() {
            let path = lower_path(p);
            if !self.path.iter().any(|x| path.contains(&x.to_lowercase())) {
                return false;
            }
        }
        let args: Vec<String> = p.cmd().iter().skip(1).map(|a| a.to_string_lossy().to_lowercase().replace('\\', "/")).collect();
        if !keep_parts && args.iter().any(|a| a.starts_with("--type=")) {
            return false; // 浏览器内核的页面、显卡这些小进程不算
        }
        if !self.args.is_empty() && !self.args.iter().any(|x| args.iter().any(|a| a.contains(&x.to_lowercase()))) {
            return false;
        }
        !self.not_args.iter().any(|x| args.iter().any(|a| a == &x.to_lowercase()))
    }
}

fn any_hit(ms: &[Match], p: &sysinfo::Process) -> bool {
    ms.iter().any(|m| m.hit(p))
}

fn app_hit(ms: &[Match], p: &sysinfo::Process) -> bool {
    ms.iter().any(|m| m.hit_in(p, true))
}

// 这个进程本身是不是某家 AI 的一个对话（每个对话各一个进程的那种）
fn conv_agent(p: &sysinfo::Process) -> Option<Agent> {
    AGENTS.read().unwrap().iter().find(|a| any_hit(&a.conv, p)).cloned()
}

// 是不是 AI 工具自己（对话、共用后台、App 本身）：这些不算“AI 开的”
fn is_agent_itself(p: &sysinfo::Process) -> bool {
    AGENTS.read().unwrap().iter().any(|a| any_hit(&a.conv, p) || any_hit(&a.host, p) || app_hit(&a.app, p))
}

// 是哪家 AI 开的、哪个对话开的
#[derive(Clone)]
struct Tag {
    agent: String,
    conv: Option<Pid>,
    thread: Option<String>,
    sess: Option<String>,
}

// 编号只许字母数字和 - _（拿去拼文件路径、链接，防止被塞进别的东西）
fn safe_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 80 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn link_for(a: &Agent, id: &str) -> Option<String> {
    (!a.link.is_empty() && safe_id(id)).then(|| a.link.replace("{id}", id))
}

// 只放行表里写过的链接格式
fn allowed_link(l: &str) -> bool {
    AGENTS.read().unwrap().iter().any(|a| {
        let Some((pre, post)) = a.link.split_once("{id}") else { return false };
        l.strip_prefix(pre).and_then(|r| r.strip_suffix(post)).map(safe_id).unwrap_or(false)
    })
}

fn mark(p: &sysinfo::Process, a: &Agent, thread: Option<String>) -> Tag {
    let get = |k: &str| if k.is_empty() { None } else { env_val(p, k).filter(|v| safe_id(v)) };
    Tag { agent: a.name.clone(), conv: None, thread, sess: get(&a.session_env) }
}

// 身上带着的“跳到对话”的链接，各家都看（Claude 对话叫来的 Codex，身上两家的都有）；按表里的顺序
fn links_of(p: &sysinfo::Process) -> Vec<String> {
    AGENTS.read().unwrap().iter().filter(|a| !a.link_env.is_empty()).filter_map(|a| env_val(p, &a.link_env).filter(|v| safe_id(v)).and_then(|v| link_for(a, &v))).collect()
}

fn env_val(p: &sysinfo::Process, key: &str) -> Option<String> {
    p.environ().iter().find_map(|e| {
        let e = e.to_string_lossy();
        let (k, v) = e.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

// 程序身上的标记：只认表里写的那几个名字，别的环境变量一概不看
fn env_tag(p: &sysinfo::Process) -> Option<Tag> {
    let agents = AGENTS.read().unwrap();
    let ai = env_val(p, "AI_AGENT").map(|v| v.to_lowercase());
    for a in agents.iter() {
        if !a.thread_env.is_empty() {
            if let Some(t) = env_val(p, &a.thread_env).filter(|t| !t.is_empty()) {
                return Some(mark(p, a, Some(t)));
            }
        }
    }
    for a in agents.iter() {
        let by_ai = ai.as_ref().map(|v| a.ai_agent.iter().any(|x| v.starts_with(&x.to_lowercase()))).unwrap_or(false);
        if by_ai || a.env.iter().any(|k| env_val(p, k).is_some()) {
            return Some(mark(p, a, None));
        }
    }
    // 通用约定 AI_AGENT=工具名：表里没有的新工具也认，名字就用它写的
    let v = ai?;
    let name = v.split(['_', ' ']).next().unwrap_or(&v).to_string();
    (!name.is_empty()).then(|| Tag { agent: name, conv: None, thread: None, sess: None })
}

// 先往上找最近的对话进程（找得到就知道是哪个对话）；找不到再看自己身上的标记（开它的对话关了也认得出是哪家）
fn tag_of(sys: &System, pid: Pid) -> Option<Tag> {
    let me = sys.process(pid)?;
    if is_agent_itself(me) {
        return None;
    }
    let mut cur = pid;
    for _ in 0..24 {
        let Some(p) = sys.process(cur) else { break };
        let Some(par) = p.parent() else { break };
        let Some(pp) = sys.process(par) else { break };
        if pp.start_time() > p.start_time() {
            break;
        }
        if let Some(a) = conv_agent(pp) {
            return Some(Tag { agent: a.name, conv: Some(par), thread: None, sess: None });
        }
        cur = par;
    }
    env_tag(me)
}

// 对话名：先看表里说的环境变量（Claude Code），再看各家的对话索引（Codex），都没有就空着（界面写文件夹 + 几点开的）
fn conv_name(p: &sysinfo::Process) -> Option<String> {
    let a = conv_agent(p)?;
    if a.name_env.is_empty() { None } else { env_val(p, &a.name_env).filter(|v| !v.trim().is_empty()) }
}

static NAME_INDEX: Mutex<Option<HashMap<String, (SystemTime, HashMap<String, String>)>>> = Mutex::new(None);

fn thread_name(agent: &str, thread: &str) -> Option<String> {
    let file = AGENTS.read().unwrap().iter().find(|a| a.name == agent).map(|a| a.name_index.clone()).filter(|f| !f.is_empty())?;
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()?;
    let path = std::path::PathBuf::from(file.replacen('~', &home, 1));
    let mtime = path.metadata().and_then(|m| m.modified()).ok()?;
    let mut cache = NAME_INDEX.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    let fresh = cache.get(&file).map(|c| c.0 == mtime).unwrap_or(false);
    if !fresh {
        let mut names = HashMap::new();
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let (Some(id), Some(n)) = (v["id"].as_str(), v["thread_name"].as_str()) {
                    names.insert(id.to_string(), n.to_string());
                }
            }
        }
        cache.insert(file.clone(), (mtime, names));
    }
    cache.get(&file)?.1.get(thread).cloned()
}

// 按对话编号查对话名（对话关了也查得到）：表里写的文件夹 + 文件，取最后一次记下的名字；两分钟查一次
static TITLES: Mutex<Option<HashMap<String, (Instant, Option<String>)>>> = Mutex::new(None);

fn sess_title(agent: &str, id: &str, p: &sysinfo::Process) -> Option<String> {
    if !safe_id(id) {
        return None;
    }
    let a = AGENTS.read().unwrap().iter().find(|a| a.name == agent).cloned()?;
    if a.title_file.is_empty() || a.title_key.is_empty() {
        return None;
    }
    if let Some((t, v)) = TITLES.lock().unwrap().get_or_insert_with(HashMap::new).get(id) {
        if t.elapsed().as_secs() < 120 {
            return v.clone();
        }
    }
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    // 文件夹：表里按顺序试——标记名（先看那个程序自己身上的，再看小方块自己的），~ 开头的是默认位置
    let base = a.title_dir.split('|').find_map(|d| match d.strip_prefix('~') {
        Some(rest) => Some(format!("{home}{rest}")),
        None => env_val(p, d).or_else(|| std::env::var(d).ok()).filter(|v| !v.is_empty()),
    });
    let got = base.and_then(|b| find_file(std::path::PathBuf::from(b), &a.title_file.replace("{id}", id))).and_then(|f| last_value(&f, &a.title_key));
    TITLES.lock().unwrap().get_or_insert_with(HashMap::new).insert(id.to_string(), (Instant::now(), got.clone()));
    got
}

// 路径里一段写 * = 这一层随便哪个文件夹
fn find_file(base: std::path::PathBuf, rel: &str) -> Option<std::path::PathBuf> {
    let mut cands = vec![base];
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        cands = if seg == "*" {
            cands.iter().filter_map(|c| std::fs::read_dir(c).ok()).flatten().flatten().map(|e| e.path()).filter(|x| x.is_dir()).collect()
        } else {
            cands.into_iter().map(|c| c.join(seg)).collect()
        };
    }
    cands.into_iter().find(|c| c.is_file())
}

// 一行一条的记录里，最后一次出现的 key 的值；先读最后 2MB，没有再读整个
fn last_value(f: &std::path::Path, key: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let pat = format!("\"{key}\"");
    let scan = |text: &str| {
        text.lines().filter(|l| l.contains(&pat)).filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).filter_map(|v| v[key].as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).last()
    };
    let mut file = std::fs::File::open(f).ok()?;
    let len = file.metadata().ok()?.len();
    let tail = 2 * 1024 * 1024;
    if len > tail {
        file.seek(SeekFrom::Start(len - tail)).ok()?;
        let mut buf = vec![];
        file.read_to_end(&mut buf).ok()?;
        if let Some(v) = scan(&String::from_utf8_lossy(&buf)) {
            return Some(v);
        }
        if len > 200 * 1024 * 1024 {
            return None;
        }
        file.seek(SeekFrom::Start(0)).ok()?;
    }
    let mut buf = vec![];
    file.read_to_end(&mut buf).ok()?;
    scan(&String::from_utf8_lossy(&buf))
}

// 对话叫什么：有名字用名字，没有就空（界面用文件夹兜底）
fn tag_label(sys: &System, t: &Tag) -> Option<String> {
    if let Some(c) = t.conv {
        return sys.process(c).and_then(conv_name);
    }
    t.thread.as_ref().and_then(|th| thread_name(&t.agent, th))
}

fn folder(p: &sysinfo::Process) -> Option<String> {
    p.cwd().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string())
}

#[derive(Serialize)]
struct Part {
    kind: &'static str,
    n: usize,
    ram: f64,
}
#[derive(Serialize)]
struct Parts {
    parts: Vec<Part>,
}

// 点开一个程序时：把它的进程拆成 它自己的窗口 / 页面 / 主程序 / 小帮手
#[tauri::command]
fn group_parts(state: tauri::State<AppState>, pids: Vec<u32>) -> Parts {
    parts_of(&mut state.0.lock().unwrap(), pids)
}

fn parts_of(w: &mut Watch, pids: Vec<u32>) -> Parts {
    let sys = &mut w.sys;
    let list: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&list), false, ProcessRefreshKind::nothing().with_memory().with_cmd(UpdateKind::OnlyIfNotSet));
    let mut parts: Vec<Part> = vec![];
    for pid in &list {
        let Some(p) = sys.process(*pid) else { continue };
        let ram = p.memory() as f64 / GB;
        let kind = if let Some(t) = p.cmd().iter().find_map(|a| a.to_string_lossy().strip_prefix("--type=").map(|v| v.to_string())) {
            if t != "renderer" {
                "helper"
            } else if p.cmd().iter().any(|a| a.to_string_lossy().starts_with("--desktop-")) {
                "win"
            } else {
                "page"
            }
        } else {
            "main"
        };
        match parts.iter_mut().find(|x| x.kind == kind) {
            Some(x) => {
                x.n += 1;
                x.ram += ram;
            }
            None => parts.push(Part { kind, n: 1, ram }),
        }
    }
    Parts { parts }
}

// ── 以对话为主：每个 AI 对话一行，底下列它开的东西（照 Process Explorer 的进程树：谁开的就挂在谁下面）──
#[derive(Clone, Default)]
struct Who {
    start: u64,
    // 这个进程本身是某家 AI 的一个对话
    conv: Option<String>,
    // 身上的标记：哪家 AI、哪个对话编号
    env: Option<(String, Option<String>)>,
    sess: Option<String>,
    links: Vec<String>,
}

fn who_of(cache: &mut HashMap<u32, Who>, pid: Pid, p: &sysinfo::Process) -> Who {
    let st = p.start_time();
    if let Some(w) = cache.get(&pid.as_u32()).filter(|w| w.start == st) {
        return w.clone();
    }
    let conv = conv_agent(p).map(|a| a.name);
    let itself = conv.is_some() || is_agent_itself(p);
    let t = if itself { None } else { env_tag(p) };
    let sess = t.as_ref().and_then(|t| t.sess.clone());
    let w = Who { start: st, conv, env: t.map(|t| (t.agent, t.thread)), sess, links: links_of(p) };
    cache.insert(pid.as_u32(), w.clone());
    w
}

#[derive(Serialize)]
struct Item {
    kind: &'static str,
    what: String,
    // tool = 对话自己的工具（对话还要用）；opened = AI 动手开的（窗口、预览……能单独关）
    role: &'static str,
    n: usize,
    ram: f64,
    pids: Vec<u32>,
    // 它是什么：窗口标题（Edge 就是正在看的网页）、网页预览自己的标题
    title: Option<String>,
    port: Option<u16>,
    win: bool,
}

#[derive(Serialize)]
struct Chat {
    id: String,
    agent: String,
    name: Option<String>,
    dir: Option<String>,
    // 对话本身在共用的后台里（Codex 桌面版）：只能关它开的东西
    shared: bool,
    busy_seen: bool,
    since: u64,
    idle_min: u64,
    watched_min: u64,
    own: f64,
    ram: f64,
    all: Vec<u32>,
    end: Vec<u32>,
    items: Vec<Item>,
    // 点一下跳到这个对话（各家 App 自己的链接；没有就把它所在的窗口叫到前面）
    link: Option<String>,
    note: Option<String>,
    note_en: Option<String>,
}

fn is_shell(p: &sysinfo::Process) -> bool {
    SHELL.contains(&stem(&p.name().to_string_lossy()).as_str())
}

fn is_browser(s: &str) -> bool {
    s == "msedge" || s == "chrome" || s == "firefox" || s.contains("chromium") || s.contains("headless_shell")
}

// 一个进程归哪一样：碰到不是命令行壳的就开新的一样，它下面的都归它；浏览器另开一样（好单独关）
fn place(sys: &System, kids: &HashMap<Pid, Vec<Pid>>, win: &HashSet<u32>, c: Pid, items: &mut Vec<Vec<Pid>>, own: &mut Vec<Pid>, cur: Option<usize>) {
    let Some(p) = sys.process(c) else { return };
    let s = stem(&p.name().to_string_lossy());
    let parent_same = p.parent().and_then(|x| sys.process(x)).map(|x| stem(&x.name().to_string_lossy()) == s).unwrap_or(false);
    let idx = match cur {
        None if is_shell(p) => {
            own.push(c);
            None
        }
        Some(i) if parent_same || !(is_browser(&s) || win.contains(&c.as_u32())) || sys.process(items[i][0]).map(|r| conv_agent(r).is_some()).unwrap_or(false) => {
            items[i].push(c);
            Some(i)
        }
        _ => {
            items.push(vec![c]);
            Some(items.len() - 1)
        }
    };
    for k in kids.get(&c).cloned().unwrap_or_default() {
        place(sys, kids, win, k, items, own, idx);
    }
}

// 程序自己登记的名字（任务管理器显示的就是这个：PhoneExperienceHost → 手机连接）；同一个文件只读一次
static DESC: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

#[cfg(windows)]
fn file_desc(path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let key = path.to_string_lossy().to_lowercase();
    if let Some(v) = DESC.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return v.clone();
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let got = unsafe {
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            None
        } else {
            let mut buf = vec![0u8; size as usize];
            if GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
                None
            } else {
                let q = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                let mut n = 0u32;
                let mut langs = vec![];
                if VerQueryValueW(buf.as_ptr() as *const _, q("\\VarFileInfo\\Translation").as_ptr(), &mut p, &mut n) != 0 && n >= 4 {
                    let t = std::slice::from_raw_parts(p as *const u16, (n / 2) as usize);
                    for c in t.chunks(2) {
                        if c.len() == 2 {
                            langs.push(format!("{:04x}{:04x}", c[0], c[1]));
                        }
                    }
                }
                langs.extend(["040904b0".to_string(), "080404b0".to_string()]);
                langs.into_iter().find_map(|l| {
                    let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                    let mut n = 0u32;
                    if VerQueryValueW(buf.as_ptr() as *const _, q(&format!("\\StringFileInfo\\{l}\\FileDescription")).as_ptr(), &mut p, &mut n) != 0 && n > 1 {
                        let v = String::from_utf16_lossy(std::slice::from_raw_parts(p as *const u16, n as usize - 1)).trim().to_string();
                        (!v.is_empty()).then_some(v)
                    } else {
                        None
                    }
                })
            }
        }
    };
    DESC.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, got.clone());
    got
}
#[cfg(not(windows))]
fn file_desc(_: &std::path::Path) -> Option<String> {
    None
}

// 屏幕上看得见的窗口：哪个程序的、标题是什么
type Wins = HashMap<u32, Vec<(usize, String)>>;

#[cfg(windows)]
fn win_list() -> Wins {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> i32 {
        unsafe {
            if IsWindowVisible(h) != 0 {
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, &mut pid);
                let mut buf = [0u16; 512];
                let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
                (*(l as *mut Wins)).entry(pid).or_default().push((h as usize, String::from_utf16_lossy(&buf[..n])));
            }
        }
        1
    }
    let mut map: Wins = HashMap::new();
    unsafe {
        EnumWindows(Some(cb), &mut map as *mut _ as LPARAM);
    }
    map
}
#[cfg(not(windows))]
fn win_list() -> Wins {
    HashMap::new()
}

// 一个程序最像“主窗口”的那个：标题最长的
fn best_win(wins: &Wins, pid: u32) -> Option<(usize, String)> {
    wins.get(&pid)?.iter().max_by_key(|w| w.1.chars().count()).cloned()
}

// 窗口标题去掉末尾的程序名（“Google - 个人 - Microsoft Edge” → “Google - 个人”）
fn win_title(wins: &Wins, sys: &System, pids: &[Pid]) -> Option<String> {
    pids.iter().find_map(|pid| {
        let (_, t) = best_win(wins, pid.as_u32())?;
        let t: String = t.chars().filter(|c| !matches!(c, '\u{200b}' | '\u{200e}' | '\u{200f}')).collect();
        let app = sys.process(*pid).and_then(|p| p.exe()).and_then(file_desc).unwrap_or_default();
        let t = [" - ", " — ", " | "].iter().find_map(|sep| t.strip_suffix(&format!("{sep}{app}"))).map(|x| x.to_string()).unwrap_or(t);
        let t = t.trim().to_string();
        (!t.is_empty() && t != app).then_some(t)
    })
}

// 网页预览自己的标题（<title>）：只问本机这个端口一次，问不到一分钟后再问
static PAGES: Mutex<Option<HashMap<(u32, u16), (Instant, Option<String>)>>> = Mutex::new(None);

fn page_title(pid: u32, port: u16) -> Option<String> {
    if let Some((t, v)) = PAGES.lock().unwrap().get_or_insert_with(HashMap::new).get(&(pid, port)) {
        if v.is_some() || t.elapsed().as_secs() < 60 {
            return v.clone();
        }
    }
    let got = (|| {
        use std::io::{Read, Write};
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(150)).ok()?;
        s.set_read_timeout(Some(Duration::from_millis(300))).ok()?;
        s.write_all(format!("GET / HTTP/1.0\r\nHost: localhost:{port}\r\nAccept: text/html\r\n\r\n").as_bytes()).ok()?;
        let mut buf = vec![];
        let _ = s.take(128 * 1024).read_to_end(&mut buf);
        let html = String::from_utf8_lossy(&buf);
        let low = html.to_lowercase();
        let a = low.find("<title")?;
        let b = a + low[a..].find('>')? + 1;
        let e = b + low[b..].find("</title")?;
        let t = html[b..e].replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        (!t.is_empty() && !t.starts_with("Directory listing for")).then(|| t.chars().take(60).collect())
    })();
    PAGES.lock().unwrap().get_or_insert_with(HashMap::new).insert((pid, port), (Instant::now(), got.clone()));
    got
}

// 点一下“去看它”：有链接就打开链接（跳到对话 / 用浏览器打开网页预览），不然把它的窗口叫到前面；
// 它自己没窗口（终端里跑的对话），就往上找它所在的那个窗口（终端、VS Code……）
#[tauri::command]
fn look(state: tauri::State<AppState>, pids: Vec<u32>, link: Option<String>, port: Option<u16>, up: Option<bool>) -> bool {
    if let Some(l) = link {
        return allowed_link(&l) && tauri_plugin_opener::open_url(l, None::<&str>).is_ok();
    }
    if let Some(pt) = port {
        return tauri_plugin_opener::open_url(format!("http://localhost:{pt}/"), None::<&str>).is_ok();
    }
    let w = state.0.lock().unwrap();
    pick_win(&w.sys, &win_list(), &pids, up.unwrap_or(false)).map(|(h, _)| focus(h)).unwrap_or(false)
}

// up = 是个对话：自己没窗口就往上找它所在的窗口；别的东西（后台开的浏览器）没窗口就是没有
fn pick_win(sys: &System, wins: &Wins, pids: &[u32], up: bool) -> Option<(usize, String)> {
    let mut cand: Vec<u32> = vec![];
    for p in pids {
        cand.push(*p);
        cand.extend(descendants(sys, Pid::from_u32(*p)).iter().map(|x| x.as_u32()));
    }
    if let Some(w) = cand.iter().find_map(|p| best_win(wins, *p)) {
        return Some(w);
    }
    if !up {
        return None;
    }
    for p in pids {
        let mut cur = Pid::from_u32(*p);
        for _ in 0..12 {
            let Some(pr) = sys.process(cur) else { break };
            let Some(par) = pr.parent() else { break };
            let Some(pp) = sys.process(par) else { break };
            if pp.start_time() > pr.start_time() || ROOT_STOP.contains(&stem(&pp.name().to_string_lossy()).as_str()) {
                break;
            }
            if let Some(w) = best_win(wins, par.as_u32()) {
                return Some(w);
            }
            cur = par;
        }
    }
    None
}

#[cfg(windows)]
fn focus(h: usize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};
    unsafe {
        let h = h as windows_sys::Win32::Foundation::HWND;
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        }
        SetForegroundWindow(h) != 0
    }
}
#[cfg(not(windows))]
fn focus(_: usize) -> bool {
    false
}

// 一路往上找“开它的那个程序”，到系统程序为止；找到的最上面那个就是它属于的 App
const ROOT_STOP: &[&str] = &[
    "explorer", "svchost", "services", "wininit", "winlogon", "system", "smss", "csrss", "userinit", "sihost", "taskhostw",
    "dllhost", "runtimebroker", "applicationframehost", "startmenuexperiencehost", "searchhost", "launchd", "systemd", "init",
];

// 第二个值：往上找时停在了哪个系统程序上（Windows「搜索」开的网页小窗就归 Windows 自己）
fn root_info(sys: &System, pid: Pid) -> (String, Option<String>) {
    let mut cur = pid;
    let mut stop = None;
    for _ in 0..16 {
        let Some(p) = sys.process(cur) else { break };
        let Some(par) = p.parent() else { break };
        let Some(pp) = sys.process(par) else { break };
        let ps = stem(&pp.name().to_string_lossy());
        if pp.start_time() > p.start_time() || ROOT_STOP.contains(&ps.as_str()) || SHELL.contains(&ps.as_str()) || DEV.contains(&ps.as_str()) || ps == "timeout" {
            if ps != "explorer" && SYSTEM.contains(&ps.as_str()) {
                stop = Some(ps);
            }
            break;
        }
        cur = par;
    }
    (sys.process(cur).map(|p| stem(&p.name().to_string_lossy())).unwrap_or_default(), stop)
}

// 属于哪个 AI 工具的 App：自己是 App，或往上找到 App（App 开的后台、小帮手都算 App 本身）
fn app_of(sys: &System, pid: Pid) -> Option<Agent> {
    let agents = AGENTS.read().unwrap();
    let mut cur = pid;
    for _ in 0..16 {
        let p = sys.process(cur)?;
        if let Some(a) = agents.iter().find(|a| !a.app_name.is_empty() && app_hit(&a.app, p)) {
            return Some(a.clone());
        }
        let par = p.parent()?;
        let pp = sys.process(par)?;
        if pp.start_time() > p.start_time() {
            return None;
        }
        cur = par;
    }
    None
}

// 这一样叫什么：用看得懂的名字（网页自动化、Blender 工具、Edge 浏览器、网页预览 :5173）
fn item_info(p: &sysinfo::Process, port: Option<u16>) -> (&'static str, String, &'static str) {
    let s = stem(&p.name().to_string_lossy());
    let g = sig(p);
    let all = format!("{s} {}", g.to_lowercase());
    let harness = env_val(p, "AI_AGENT").map(|v| v.ends_with("_harness")).unwrap_or(false);
    if let Some(a) = conv_agent(p) {
        return ("agent", a.name, "opened");
    }
    if is_browser(&s) {
        return ("browser", pretty(&s), "opened");
    }
    if let (Some(pt), false) = (port, harness) {
        return ("preview", format!(":{pt}"), "opened");
    }
    if all.contains("playwright") {
        return ("mcp", "playwright".into(), "tool");
    }
    if all.contains("blender") && all.contains("mcp") {
        return ("mcp", "blender".into(), "tool");
    }
    if s == "blender" {
        return ("app", "Blender".into(), "opened");
    }
    let label = if g.is_empty() { pretty(&s) } else { format!("{} · {g}", pretty(&s)) };
    if harness || all.contains("mcp") {
        return ("mcp", if g.is_empty() { pretty(&s) } else { g }, "tool");
    }
    if !DEV.contains(&s.as_str()) {
        return ("app", pretty(&s), "opened");
    }
    ("other", label, "opened")
}

fn make_items(sys: &System, listen: &HashMap<u32, Vec<u16>>, win: &HashSet<u32>, wins: &Wins, groups: Vec<Vec<Pid>>) -> Vec<Item> {
    let mut out = vec![];
    for g in groups {
        let Some(root) = sys.process(g[0]) else { continue };
        let port = g.iter().find_map(|x| listen.get(&x.as_u32()).and_then(|v| v.iter().min().copied()));
        let (mut kind, what, role) = item_info(root, port);
        if kind == "app" && !g.iter().any(|x| win.contains(&x.as_u32())) {
            kind = "other"; // 没有窗口的小程序（git、grep……）不列出来
        }
        let ram = g.iter().filter_map(|x| sys.process(*x)).map(|p| p.memory() as f64 / GB).sum();
        let port = if kind == "preview" { port } else { None };
        let title = match (kind, port) {
            ("preview", Some(pt)) => page_title(g[0].as_u32(), pt),
            ("browser" | "app" | "agent", _) => win_title(wins, sys, &g),
            _ => None,
        };
        let has_win = g.iter().any(|x| win.contains(&x.as_u32()));
        out.push(Item { kind, what, role, n: g.len(), ram, pids: vec![g[0].as_u32()], title, port, win: has_win });
    }
    out.sort_by(|a, b| b.ram.partial_cmp(&a.ram).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn sig(p: &sysinfo::Process) -> String {
    let args: Vec<String> = p.cmd().iter().skip(1).map(|a| a.to_string_lossy().replace('\\', "/")).collect();
    for (i, a) in args.iter().enumerate() {
        if a == "-m" {
            if let Some(m) = args.get(i + 1) {
                return m.clone();
            }
        }
    }
    for a in &args {
        if let Some(i) = a.find('@') {
            let core = &a[i + 1..];
            if core.contains('/') && !a.contains("://") {
                let pkg = core.split('@').next().unwrap_or(core);
                return format!("@{}", pkg.split('/').take(2).collect::<Vec<_>>().join("/"));
            }
        }
    }
    for a in &args {
        if let Some(i) = a.rfind("node_modules/") {
            let mut it = a[i + 13..].split('/');
            let first = it.next().unwrap_or("");
            let name = if first.starts_with('@') { format!("{}/{}", first, it.next().unwrap_or("")) } else { first.to_string() };
            if !name.is_empty() && name != ".bin" && name != "npm" {
                return name;
            }
        }
    }
    for a in &args {
        let l = a.to_lowercase();
        if [".py", ".js", ".mjs", ".cjs", ".ts"].iter().any(|e| l.ends_with(e)) {
            if let Some(f) = std::path::Path::new(a).file_stem() {
                let f = f.to_string_lossy().to_string();
                if f != "npx-cli" && f != "cli" {
                    return f;
                }
            }
        }
    }
    String::new()
}

// 往上找主人（跳过命令行窗口、启动壳），看主人是不是某家 AI 的一个对话
fn owner_is_conv(sys: &System, pid: Pid) -> bool {
    let mut cur = pid;
    for _ in 0..8 {
        let Some(p) = sys.process(cur) else { return false };
        let Some(par) = p.parent() else { return false };
        let Some(pp) = sys.process(par) else { return false };
        if conv_agent(pp).is_some() {
            return true;
        }
        let s = stem(&pp.name().to_string_lossy());
        if !DEV.contains(&s.as_str()) && !SHELL.contains(&s.as_str()) {
            return false;
        }
        cur = par;
    }
    false
}

// 某个程序开出来的所有子孙（开始时间比上一级晚，防止 PID 被重用认错）
fn descendants(sys: &System, root: Pid) -> Vec<Pid> {
    let mut out = vec![];
    let mut todo = vec![root];
    while let Some(x) = todo.pop() {
        let st = sys.process(x).map(|p| p.start_time()).unwrap_or(0);
        for (cp, c) in sys.processes() {
            if c.parent() == Some(x) && c.start_time() >= st && *cp != root && !out.contains(cp) {
                out.push(*cp);
                todo.push(*cp);
            }
        }
    }
    out
}

// 结束一个 AI 对话（或共用后台的对话开的那几个程序）：连它们开的帮手一起关（Windows 关掉上级不会自动关下级）
#[tauri::command]
fn kill_tree(state: tauri::State<AppState>, pids: Vec<u32>) -> usize {
    let guard = state.0.lock().unwrap();
    let mut all = vec![];
    for pid in pids {
        let root = Pid::from_u32(pid);
        all.extend(descendants(&guard.sys, root));
        all.push(root);
    }
    let me = std::process::id();
    let mut n = 0;
    for x in all {
        if x.as_u32() == me || x.as_u32() <= 4 {
            continue;
        }
        if let Some(p) = guard.sys.process(x) {
            if SYSTEM.contains(&stem(&p.name().to_string_lossy()).as_str()) {
                continue;
            }
            if p.kill() {
                n += 1;
            }
        }
    }
    n
}

// 浏览器的主进程：一路往上找同名的上级
fn browser_root(sys: &System, pid: Pid) -> Pid {
    let mut cur = pid;
    for _ in 0..8 {
        let Some(p) = sys.process(cur) else { break };
        let Some(par) = p.parent() else { break };
        let Some(pp) = sys.process(par) else { break };
        if pp.name() != p.name() || pp.start_time() > p.start_time() {
            break;
        }
        cur = par;
    }
    cur
}

fn stem(name: &str) -> String {
    let l = name.to_lowercase();
    l.strip_suffix(".exe").unwrap_or(&l).to_string()
}

fn pretty(s: &str) -> String {
    let known = match s {
        "claude" => "Claude",
        "chatgpt" => "ChatGPT",
        "codex" => "Codex",
        "msedge" => "Edge",
        "msedgewebview2" => "Edge WebView",
        "chrome" => "Chrome",
        "code" => "VS Code",
        "node" => "Node.js",
        "python" | "pythonw" => "Python",
        "msmpeng" => "Defender",
        "explorer" => "Explorer",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// 端口：谁在“开着门等人连”、谁此刻有人连着
fn sockets() -> (HashMap<u32, Vec<u16>>, HashSet<u32>) {
    let mut listen: HashMap<u32, Vec<u16>> = HashMap::new();
    let mut conn = HashSet::new();
    if let Ok(list) = get_sockets_info(AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6, ProtocolFlags::TCP) {
        for si in list {
            if let ProtocolSocketInfo::Tcp(t) = &si.protocol_socket_info {
                for pid in &si.associated_pids {
                    match t.state {
                        TcpState::Listen => listen.entry(*pid).or_default().push(t.local_port),
                        TcpState::Established => {
                            conn.insert(*pid);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    (listen, conn)
}

// 往上一路找“主人”：跳过同类启动壳；链子断了（上级没了，或 PID 被新程序占了）= 主人已经没了
fn owner(sys: &System, pid: Pid) -> (bool, Vec<String>, Option<String>) {
    let mut cur = pid;
    let mut chain = vec![];
    for _ in 0..12 {
        let Some(p) = sys.process(cur) else { return (true, chain, None) };
        let Some(par) = p.parent() else { return (true, chain, None) };
        let Some(pp) = sys.process(par) else { return (true, chain, None) };
        if pp.start_time() > p.start_time() {
            return (true, chain, None);
        }
        let n = pp.name().to_string_lossy().to_string();
        let s = stem(&n);
        if !DEV.contains(&s.as_str()) && !SHELL.contains(&s.as_str()) {
            return (false, chain, Some(n));
        }
        chain.push(n);
        cur = par;
    }
    (false, chain, None)
}

#[cfg(windows)]
fn commit() -> Option<(f64, f64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            let lim = m.ullTotalPageFile as f64 / GB;
            return Some((lim - m.ullAvailPageFile as f64 / GB, lim));
        }
    }
    None
}
#[cfg(not(windows))]
fn commit() -> Option<(f64, f64)> {
    None
}

#[tauri::command]
fn snapshot(state: tauri::State<AppState>) -> Snap {
    snap_of(&mut state.0.lock().unwrap())
}

fn snap_of(w: &mut Watch) -> Snap {
    let Watch { sys, first_seen, last_conn, cc_busy, who } = w;
    sys.refresh_memory();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_environ(UpdateKind::OnlyIfNotSet),
    );
    let now = Instant::now();
    let mut whos: HashMap<Pid, Who> = HashMap::new();
    for (pid, p) in sys.processes() {
        whos.insert(*pid, who_of(who, *pid, p));
    }
    who.retain(|k, _| whos.contains_key(&Pid::from_u32(*k)));
    let cc: Vec<Pid> = whos.iter().filter(|(_, w)| w.conv.is_some()).map(|(pid, _)| *pid).collect();
    if !cc.is_empty() {
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&cc), false, ProcessRefreshKind::nothing().with_cpu().with_cwd(UpdateKind::OnlyIfNotSet));
        for pid in &cc {
            if let Some(p) = sys.process(*pid) {
                let acc = p.accumulated_cpu_time();
                let e = cc_busy.entry(pid.as_u32()).or_insert((acc, now, false));
                let secs = now.duration_since(e.1).as_secs_f64().max(1.0);
                if acc.saturating_sub(e.0) as f64 > 100.0 * secs.min(4.0) {
                    e.1 = now;
                    e.2 = true;
                }
                e.0 = acc;
            }
        }
        cc_busy.retain(|k, _| cc.iter().any(|p| p.as_u32() == *k));
    }
    let (listen, conn) = sockets();
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    for pid in &conn {
        last_conn.insert(*pid, now);
    }
    let me = std::process::id();
    // 谁是谁开的（开始时间比上一级晚，防止 PID 被重用认错）
    let mut kids: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(pp) = p.parent().and_then(|x| sys.process(x).map(|pp| (x, pp))) {
            if pp.1.start_time() <= p.start_time() {
                kids.entry(pp.0).or_default().push(*pid);
            }
        }
    }
    let wins = win_list();
    let win: HashSet<u32> = wins.keys().copied().collect();
    // 小方块自己（包括另外开着的小方块）和它的网页内核都不列
    let my_exe = std::env::current_exe().ok();
    let mut mine: HashSet<Pid> = HashSet::new();
    for (pid, p) in sys.processes() {
        if pid.as_u32() == me || (my_exe.is_some() && p.exe() == my_exe.as_deref()) {
            mine.insert(*pid);
            mine.extend(descendants(sys, *pid));
        }
    }
    let mut claimed: HashSet<Pid> = HashSet::new();
    let mut chats = vec![];
    let cc_set: HashSet<Pid> = cc.iter().copied().collect();
    let inside_conv = |pid: Pid| {
        let mut cur = pid;
        for _ in 0..24 {
            let Some(p) = sys.process(cur) else { return false };
            let Some(par) = p.parent() else { return false };
            let Some(pp) = sys.process(par) else { return false };
            if pp.start_time() > p.start_time() {
                return false;
            }
            if cc_set.contains(&par) {
                return true;
            }
            cur = par;
        }
        false
    };
    let own_link = |p: &sysinfo::Process| conv_agent(p).and_then(|a| if a.link_env.is_empty() { None } else { env_val(p, &a.link_env).and_then(|v| link_for(&a, &v)) });
    let mut nested: HashSet<Pid> = cc.iter().copied().filter(|c| inside_conv(*c)).collect();
    let live_links: HashSet<String> = cc.iter().filter(|c| !nested.contains(c)).filter_map(|c| sys.process(*c)).filter_map(own_link).collect();
    for c in &cc {
        let mine_l = sys.process(*c).and_then(own_link);
        if whos[c].links.iter().any(|l| live_links.contains(l) && Some(l) != mine_l.as_ref()) {
            nested.insert(*c);
        }
    }
    for c in &cc {
        if nested.contains(c) {
            continue;
        }
        let Some(p) = sys.process(*c) else { continue };
        let (mut items, mut own) = (vec![], vec![]);
        for k in kids.get(c).cloned().unwrap_or_default() {
            place(sys, &kids, &win, k, &mut items, &mut own, None);
        }
        let mut all: Vec<Pid> = vec![*c];
        all.extend(own.iter().copied());
        all.extend(items.iter().flatten().copied());
        let ram_of = |v: &[Pid]| v.iter().filter_map(|x| sys.process(*x)).map(|p| p.memory() as f64 / GB).sum::<f64>();
        let own_ram = p.memory() as f64 / GB + ram_of(&own);
        let pu = c.as_u32();
        let seen = *first_seen.entry(pu).or_insert(now);
        let last = cc_busy.get(&pu).filter(|x| x.2).map(|x| x.1).unwrap_or(seen);
        let busy_seen = cc_busy.get(&pu).map(|x| x.2).unwrap_or(false);
        chats.push(Chat {
            id: pu.to_string(),
            busy_seen,
            agent: whos[c].conv.clone().unwrap_or_default(),
            name: conv_name(p),
            dir: folder(p),
            shared: false,
            since: p.start_time(),
            idle_min: now.duration_since(last).as_secs() / 60,
            watched_min: now.duration_since(seen).as_secs() / 60,
            own: own_ram,
            ram: ram_of(&all),
            all: all.iter().map(|x| x.as_u32()).collect(),
            end: vec![pu],
            items: make_items(sys, &listen, &win, &wins, items),
            link: own_link(p),
            note: None,
            note_en: None,
        });
        claimed.extend(all);
    }
    // 和一个开着的对话同一个编号、却跑到外面去了的（开它的那一层先退出了）：还是那个对话的
    let by_link: HashMap<String, usize> = chats.iter().enumerate().filter_map(|(i, c)| c.link.clone().map(|l| (l, i))).collect();
    let mut adopt: HashMap<usize, Vec<Pid>> = HashMap::new();
    for (pid, w) in &whos {
        if claimed.contains(pid) || mine.contains(pid) || pid.as_u32() == me {
            continue;
        }
        if let Some(i) = w.links.iter().find_map(|l| by_link.get(l)) {
            adopt.entry(*i).or_default().push(*pid);
        }
    }
    for (i, ps) in adopt {
        let set: HashSet<Pid> = ps.iter().copied().collect();
        let roots: Vec<Pid> = ps.iter().copied().filter(|x| sys.process(*x).and_then(|p| p.parent()).map(|par| !set.contains(&par)).unwrap_or(true)).collect();
        let (mut items, mut own) = (vec![], vec![]);
        for r in &roots {
            place(sys, &kids, &win, *r, &mut items, &mut own, None);
        }
        let mut all: Vec<Pid> = own.clone();
        all.extend(items.iter().flatten().copied());
        let ram: f64 = all.iter().filter_map(|x| sys.process(*x)).map(|p| p.memory() as f64 / GB).sum();
        let own_ram: f64 = own.iter().filter_map(|x| sys.process(*x)).map(|p| p.memory() as f64 / GB).sum();
        let new_items = make_items(sys, &listen, &win, &wins, items);
        let c = &mut chats[i];
        c.items.extend(new_items);
        c.items.sort_by(|a, b| b.ram.partial_cmp(&a.ram).unwrap_or(std::cmp::Ordering::Equal));
        c.ram += ram;
        c.own += own_ram;
        c.all.extend(all.iter().map(|x| x.as_u32()));
        c.end.extend(roots.iter().map(|x| x.as_u32()));
        claimed.extend(all);
    }
    // 共用后台的 AI（Codex 桌面版）：对话跑的命令身上带着对话编号，按编号归到一起
    let mut threads: HashMap<(String, String), Vec<Pid>> = HashMap::new();
    for (pid, w) in &whos {
        if let Some((agent, Some(t))) = &w.env {
            if !claimed.contains(pid) {
                threads.entry((agent.clone(), t.clone())).or_default().push(*pid);
            }
        }
    }
    for ((agent, thread), ps) in threads {
        let set: HashSet<Pid> = ps.iter().copied().collect();
        let mut roots: Vec<Pid> = ps.iter().copied().filter(|x| sys.process(*x).and_then(|p| p.parent()).map(|par| !set.contains(&par)).unwrap_or(true)).collect();
        roots.sort_by_key(|x| sys.process(*x).map(|p| p.start_time()).unwrap_or(0));
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&roots), false, ProcessRefreshKind::nothing().with_cwd(UpdateKind::OnlyIfNotSet));
        let (mut items, mut own) = (vec![], vec![]);
        for r in &roots {
            place(sys, &kids, &win, *r, &mut items, &mut own, None);
        }
        let mut all: Vec<Pid> = own.clone();
        all.extend(items.iter().flatten().copied());
        let ram_of = |v: &[Pid]| v.iter().filter_map(|x| sys.process(*x)).map(|p| p.memory() as f64 / GB).sum::<f64>();
        chats.push(Chat {
            id: format!("t:{thread}"),
            busy_seen: false,
            name: thread_name(&agent, &thread),
            dir: roots.first().and_then(|r| sys.process(*r)).and_then(folder),
            link: AGENTS.read().unwrap().iter().find(|a| a.name == agent && a.link_env == a.thread_env).and_then(|a| link_for(a, &thread)),
            agent,
            shared: true,
            since: roots.first().and_then(|r| sys.process(*r)).map(|p| p.start_time()).unwrap_or(0),
            idle_min: 0,
            watched_min: 0,
            own: ram_of(&own),
            ram: ram_of(&all),
            all: all.iter().map(|x| x.as_u32()).collect(),
            end: roots.iter().map(|x| x.as_u32()).collect(),
            items: make_items(sys, &listen, &win, &wins, items),
            note: None,
            note_en: None,
        });
        claimed.extend(all);
    }
    // 结束之后会怎样：按这家 AI 的表，App 里的和终端里的说法不一样
    for c in chats.iter_mut() {
        if let Some(a) = AGENTS.read().unwrap().iter().find(|a| a.name == c.agent) {
            // 终端那句只给“在命令行里直接打命令开的、看得见窗口”的对话；别的程序替它开的（上一层不是命令行），拿不准就不写
            let first = c.end.first().map(|x| Pid::from_u32(*x));
            let from_shell = first.and_then(|x| sys.process(x)).and_then(|p| p.parent()).and_then(|x| sys.process(x)).map(is_shell).unwrap_or(false);
            let in_win = c.link.is_none() && from_shell && pick_win(sys, &wins, &c.end, true).is_some();
            let (z, e) = if c.link.is_some() { (&a.end_note, &a.end_note_en) } else if in_win { (&a.cli_note, &a.cli_note_en) } else { continue };
            c.note = (!z.is_empty()).then(|| z.clone());
            c.note_en = (!e.is_empty()).then(|| e.clone());
        }
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    for (pid, p) in sys.processes() {
        let pu = pid.as_u32();
        if pu == me || claimed.contains(pid) || mine.contains(pid) {
            continue;
        }
        let name = p.name().to_string_lossy().to_string();
        let s = stem(&name);
        let ports = listen.get(&pu);
        let is_dev = DEV.contains(&s.as_str());
        // 开着端口的开发程序各自一行（每个都是一个单独的网页服务）；其余按程序名合并
        let parent_cc = owner_is_conv(sys, *pid);
        let what = if is_dev && ports.is_none() { sig(p) } else { String::new() };
        let orphan = is_dev && ports.is_none() && owner(sys, *pid).0;
        let is_browser = s == "msedge" || s == "chrome";
        let broot = if is_browser { browser_root(sys, *pid) } else { *pid };
        // 浏览器：是哪家 AI、哪个对话开的（找得到对话 = 有人管；只剩标记、主人没了、还开着遥控口 = 没人管）
        let btag = if is_browser { tag_of(sys, broot) } else { None };
        let bai = btag.as_ref().filter(|t| t.conv.is_some() || t.thread.is_some());
        let bremote = is_browser
            && sys.process(broot).map(|r| r.cmd().iter().any(|a| a.to_string_lossy().starts_with("--remote-debugging"))).unwrap_or(false);
        let borphan = is_browser && bai.is_none() && bremote && owner(sys, broot).0;
        // AI 工具的 App 本身（比如 Codex 桌面版是个叫 ChatGPT.exe 的程序）单独一行、用它的名字
        // 开着端口的网页服务、主人没了的开发程序：各自一行（App 开的也一样），不并进 App 本身
        let app = if is_dev && (ports.is_some() || orphan) { None } else { app_of(sys, *pid) };
        let key = if let Some(a) = &app {
            format!("app:{}", a.id)
        } else if bai.is_some() || borphan {
            format!("{s}@{}", broot.as_u32())
        } else if is_dev && ports.is_some() || orphan {
            format!("{s}:{pu}")
        } else if !what.is_empty() {
            format!("{s}#{what}")
        } else if SYSTEM.contains(&s.as_str()) || is_dev || SHELL.contains(&s.as_str()) || whos.get(pid).map(|w| w.env.is_some()).unwrap_or(false) {
            s.clone()
        } else {
            match root_info(sys, *pid) {
                (r, Some(sp)) if r == "msedgewebview2" => sp,
                (r, _) => r,
            }
        };
        let g = groups.entry(key.clone()).or_insert_with(|| Group {
            id: key.clone(),
            name: if key.contains([':', '#', '@']) { pretty(&s) } else { pretty(&key) },
            name_en: None,
            n: 0,
            ram: 0.0,
            vm: 0.0,
            pids: vec![],
            exe: p.exe().map(|e| e.display().to_string()),
            since: p.start_time(),
            sys: SYSTEM.contains(&s.as_str()) || SYSTEM.contains(&key.as_str()) || pu <= 4,
            score: 0,
            why: vec![],
            conv: None,
            link: None,
            title: None,
            port: None,
            cc_kids: 0,
        });
        if g.n == 0 && !what.is_empty() {
            g.name = format!("{} · {}", pretty(&s), what);
        }
        if !key.contains([':', '#', '@']) && stem(&p.name().to_string_lossy()) == key {
            if let Some(d) = p.exe().and_then(file_desc) {
                g.name = d;
            }
        }
        if g.n == 0 {
            if let Some(a) = &app {
                g.name = a.app_name.clone();
                g.name_en = (!a.app_name_en.is_empty()).then(|| a.app_name_en.clone());
                g.why.push(Why { k: "appOf", v: a.name.clone() });
            }
        }
        if g.n == 0 && (bai.is_some() || borphan) {
            let agent = btag.as_ref().map(|t| t.agent.clone()).unwrap_or_default();
            g.name = if agent.is_empty() { format!("{}（AI 开的）", pretty(&s)) } else { format!("{}（{agent} 开的）", pretty(&s)) };
            g.why.push(Why { k: "agent", v: agent });
            if let Some(t) = bai {
                let label = tag_label(sys, t)
                    .or_else(|| t.conv.and_then(|c| sys.process(c)).and_then(folder).map(|d| format!("dir:{d}")))
                    .unwrap_or_default();
                g.why.push(Why { k: "openedBy", v: label });
            } else {
                g.score = 3;
                g.why.push(Why { k: "ownerGone", v: String::new() });
                g.why.push(Why { k: "aiBrowser", v: String::new() });
            }
        }
        if parent_cc {
            g.cc_kids += 1;
        }
        if let Some(w) = whos.get(pid) {
            if let Some((agent, _)) = &w.env {
                if !g.why.iter().any(|x| x.k == "byAgent") {
                    g.why.push(Why { k: "byAgent", v: agent.clone() });
                }
                if g.conv.is_none() {
                    g.conv = w.sess.as_ref().and_then(|s| sess_title(agent, s, p));
                }
            }
            if g.link.is_none() {
                g.link = w.links.first().cloned();
            }
        }
        g.n += 1;
        g.ram += p.memory() as f64 / GB;
        g.vm += p.virtual_memory() as f64 / GB;
        g.pids.push(pu);
        g.since = g.since.min(p.start_time());
        if let (true, Some(ps)) = (is_dev, ports) {
            let mut ps = ps.clone();
            ps.sort();
            ps.dedup();
            if g.port.is_none() {
                g.port = ps.first().copied();
                g.title = g.port.and_then(|pt| page_title(pu, pt));
            }
            let port = ps.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
            g.name = format!("{} :{}", pretty(&s), port);
            g.score += 1;
            g.why.push(Why { k: "port", v: port });
            let (broken, chain, own) = owner(sys, *pid);
            if broken {
                g.score += 2;
                g.why.push(Why { k: "ownerGone", v: chain.join("→") });
            } else if let Some(o) = own {
                g.why.push(Why { k: "owner", v: o });
            }
            let age_h = epoch.saturating_sub(p.start_time()) as f64 / 3600.0;
            if age_h >= 6.0 {
                g.score += 1;
            }
            g.why.push(Why { k: "age", v: format!("{age_h:.1}") });
            let seen = *first_seen.entry(pu).or_insert(now);
            let last = last_conn.get(&pu).copied().unwrap_or(seen);
            let idle = now.duration_since(last).as_secs() / 60;
            if idle >= 60 {
                g.score += 1;
                g.why.push(Why { k: "idle", v: idle.to_string() });
            }
        } else if orphan {
            // 没开端口、但主人没了：只算“可能没人管”（像桌宠这种是故意开着的）
            g.score = 2;
            if g.why.is_empty() {
                g.why.push(Why { k: "ownerGone", v: String::new() });
            }
        }
    }
    let mut out: Vec<Group> = groups.into_values().collect();
    // AI 开的窗口（浏览器、软件）：写上它正在显示什么
    for g in out.iter_mut() {
        if g.title.is_none() && g.why.iter().any(|x| x.k == "agent" || x.k == "byAgent") {
            let ps: Vec<Pid> = g.pids.iter().map(|x| Pid::from_u32(*x)).collect();
            g.title = win_title(&wins, sys, &ps);
        }
    }
    let sys_ram: f64 = out.iter().filter(|g| g.sys).map(|g| g.ram).sum();
    // 同一个帮手开了好几份、而且都是 Claude Code 对话开的 → 标出“每个对话各开一份”
    for g in out.iter_mut() {
        let shell = g.id == "conhost" || SHELL.contains(&g.id.as_str());
        if g.cc_kids >= 2 && !shell {
            g.why.push(Why { k: "perConv", v: g.cc_kids.to_string() });
        }
    }
    out.sort_by(|a, b| b.ram.partial_cmp(&a.ram).unwrap_or(std::cmp::Ordering::Equal));
    let mut keep = vec![];
    for (i, g) in out.into_iter().enumerate() {
        if i < 12 || g.score >= 2 || g.why.iter().any(|x| x.k == "perConv") {
            keep.push(g);
        }
    }
    let total = sys.total_memory() as f64 / GB;
    let used = sys.used_memory() as f64 / GB;
    let (commit_used, commit_limit) = commit().unwrap_or((
        used + sys.used_swap() as f64 / GB,
        total + sys.total_swap() as f64 / GB,
    ));
    Snap { total, used, commit_used, commit_limit, sys_ram, groups: keep, chats }
}

// 结束：只结束列出来的这些 PID，系统程序和自己一律不碰
#[tauri::command]
fn kill(state: tauri::State<AppState>, pids: Vec<u32>) -> usize {
    let guard = state.0.lock().unwrap();
    let me = std::process::id();
    let mut n = 0;
    for pu in pids {
        if pu == me || pu <= 4 {
            continue;
        }
        if let Some(p) = guard.sys.process(Pid::from_u32(pu)) {
            let s = stem(&p.name().to_string_lossy());
            if SYSTEM.contains(&s.as_str()) {
                continue;
            }
            if p.kill() {
                n += 1;
            }
        }
    }
    n
}

// ── 借电脑上已登录的 Codex 问一句（不用 Key）：在空的临时文件夹里只读跑，问题从标准输入进，回答从 -o 文件读 ──
static ASKING: Mutex<Option<u32>> = Mutex::new(None);

fn find_codex() -> Option<std::path::PathBuf> {
    let mut found: Vec<(SystemTime, std::path::PathBuf)> = vec![];
    // Codex 桌面版自带的命令（那串字符升级会变，挑最新的）
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let bin = std::path::Path::new(&local).join("OpenAI").join("Codex").join("bin");
        if let Ok(rd) = std::fs::read_dir(&bin) {
            for e in rd.flatten() {
                let exe = e.path().join("codex.exe");
                if let Ok(m) = exe.metadata() {
                    found.push((m.modified().unwrap_or(UNIX_EPOCH), exe));
                }
            }
        }
    }
    // npm 装的 Codex
    if let Ok(roaming) = std::env::var("APPDATA") {
        let exe = std::path::Path::new(&roaming)
            .join("npm/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
        if let Ok(m) = exe.metadata() {
            found.push((m.modified().unwrap_or(UNIX_EPOCH), exe));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    if let Some((_, p)) = found.into_iter().next() {
        return Some(p);
    }
    // Mac / Linux：在 PATH 里找
    #[cfg(not(windows))]
    for dir in std::env::var("PATH").unwrap_or_default().split(':').chain(["/opt/homebrew/bin", "/usr/local/bin"]) {
        let exe = std::path::Path::new(dir).join("codex");
        if exe.exists() {
            return Some(exe);
        }
    }
    None
}

#[tauri::command]
fn codex_status() -> Option<String> {
    find_codex().map(|p| p.display().to_string())
}

fn run_codex(prompt: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let exe = find_codex().ok_or("notfound")?;
    let dir = std::env::temp_dir().join("rameow-ask");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let out = dir.join(format!("answer-{stamp}.txt"));
    let mut cmd = Command::new(&exe);
    cmd.args(["exec", "--skip-git-repo-check", "-s", "read-only", "-C"])
        .arg(&dir)
        .arg("-o")
        .arg(&out)
        .args(["-c", "model_reasoning_effort=\"low\"", "-"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // 不弹黑色命令行窗口
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    *ASKING.lock().unwrap() = Some(child.id());
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(prompt.as_bytes());
    }
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > Duration::from_secs(180) => {
                let _ = child.kill();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break None,
        }
    };
    let cancelled = ASKING.lock().unwrap().take().is_none();
    let text = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    if cancelled {
        return Err("cancelled".into());
    }
    match status {
        None => Err("timeout".into()),
        Some(_) if text.trim().is_empty() => Err("noanswer".into()),
        Some(_) => Ok(text),
    }
}

// 借电脑上已经登录的 Claude Code：先找官方安装的 claude.exe，再找 npm 装的 claude.cmd，再在 PATH 里找
fn find_claude() -> Option<std::path::PathBuf> {
    let mut cands: Vec<std::path::PathBuf> = vec![];
    if let Ok(h) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        cands.push(std::path::Path::new(&h).join(".local").join("bin").join(if cfg!(windows) { "claude.exe" } else { "claude" }));
    }
    if let Ok(r) = std::env::var("APPDATA") {
        cands.push(std::path::Path::new(&r).join("npm").join("claude.cmd"));
    }
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for n in names {
            cands.push(dir.join(n));
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

#[tauri::command]
fn claude_status() -> Option<String> {
    find_claude().map(|p| p.display().to_string())
}

fn run_claude(prompt: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let exe = find_claude().ok_or("notfound")?;
    let dir = std::env::temp_dir().join("rameow-ask");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.args(["-p", "--output-format", "text", "--no-session-persistence", "--tools", "", "--strict-mcp-config"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // 小方块要是从某个 AI 对话里开的，别让 Claude Code 以为自己是那个对话的帮手
    for k in ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_HOST_SESSION_ID", "AI_AGENT"] {
        cmd.env_remove(k);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // 不弹黑色命令行窗口
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    *ASKING.lock().unwrap() = Some(child.id());
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(prompt.as_bytes());
    }
    let mut so = child.stdout.take().ok_or("noanswer")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > Duration::from_secs(180) => {
                let _ = child.kill();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break None,
        }
    };
    let cancelled = ASKING.lock().unwrap().take().is_none();
    let text = reader.join().unwrap_or_default();
    if cancelled {
        return Err("cancelled".into());
    }
    match status {
        None => Err("timeout".into()),
        Some(_) if text.trim().is_empty() => Err("noanswer".into()),
        Some(_) => Ok(text),
    }
}

#[tauri::command]
async fn ask_claude(prompt: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || run_claude(&prompt)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn ask_codex(prompt: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || run_codex(&prompt)).await.map_err(|e| e.to_string())?
}

// 中途不想等：把正在跑的 Codex 关掉
#[tauri::command]
fn cancel_codex() {
    if let Some(pid) = ASKING.lock().unwrap().take() {
        let sys = System::new_all();
        let root = Pid::from_u32(pid);
        for x in descendants(&sys, root).into_iter().chain(Some(root)) {
            if let Some(p) = sys.process(x) {
                let _ = p.kill();
            }
        }
    }
}

fn toggle_win(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 测试用：rameow.exe --dump 文件  → 把看到的数据（含每个对话开了什么）写进文件就退出，不开窗口
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--dump") {
        let mut w = Watch { sys: System::new_all(), first_seen: HashMap::new(), last_conn: HashMap::new(), cc_busy: HashMap::new(), who: HashMap::new() };
        snap_of(&mut w);
        std::thread::sleep(Duration::from_millis(600));
        let snap = snap_of(&mut w);
        let _ = std::fs::write(args.get(i + 1).map(|s| s.as_str()).unwrap_or("dump.json"), serde_json::to_string_pretty(&snap).unwrap_or_default());
        return;
    }
    // 测试用：--ask-claude 文件 → 借 Claude Code 问一句很短的话，回答写进文件
    if let Some(i) = args.iter().position(|a| a == "--ask-claude") {
        let got = run_claude("Reply with exactly: {\"ok\":true}").unwrap_or_else(|e| format!("ERR {e}"));
        let _ = std::fs::write(args.get(i + 1).map(|s| s.as_str()).unwrap_or("ask.txt"), got);
        return;
    }
    // 测试用：--look 进程号,进程号 文件 [up] → 写下“点一下”会叫到前面的是哪个窗口（只看不动）
    if let Some(i) = args.iter().position(|a| a == "--look") {
        let mut sys = System::new_all();
        sys.refresh_all();
        let pids: Vec<u32> = args.get(i + 1).map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_default();
        let got = pick_win(&sys, &win_list(), &pids, args.get(i + 3).map(|v| v == "up").unwrap_or(false)).map(|(_, t)| t).unwrap_or_else(|| "（没有窗口）".into());
        let _ = std::fs::write(args.get(i + 2).map(|s| s.as_str()).unwrap_or("look.txt"), got);
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_shortcuts(["ctrl+shift+m"])
                .expect("shortcut")
                .with_handler(|app, _sc, ev| {
                    if ev.state == ShortcutState::Pressed {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.emit("toggle-panel", ());
                        }
                    }
                })
                .build(),
        )
        .manage(AppState(Mutex::new(Watch {
            sys: System::new(),
            first_seen: HashMap::new(),
            last_conn: HashMap::new(),
            cc_busy: HashMap::new(),
            who: HashMap::new(),
        })))
        .manage(Hit(Mutex::new((150.0, 10.0, 100.0, 44.0))))
        .invoke_handler(tauri::generate_handler![sys_lang, snapshot, kill, kill_tree, set_hit, codex_status, ask_codex, claude_status, ask_claude, cancel_codex, group_parts, set_guard, look])
        .setup(|app| {
            // 正式版第一次打开时默认开机启动（托盘里能关）；开发版不登记，免得开机去开调试版
            #[cfg(not(debug_assertions))]
            {
                let dir = app.path().app_config_dir()?;
                let mark = dir.join("autostart-set");
                if !mark.exists() {
                    let _ = app.autolaunch().enable();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(&mark, b"1");
                }
            }
            let auto_on = app.autolaunch().is_enabled().unwrap_or(false);
            let toggle = MenuItem::with_id(app, "toggle", "显示 / 隐藏  Show / Hide", true, None::<&str>)?;
            let auto = CheckMenuItem::with_id(app, "auto", "开机启动  Start at login", true, auto_on, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出  Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&toggle, &auto, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Memory")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id().as_ref() {
                    "toggle" => toggle_win(app),
                    "auto" => {
                        let al = app.autolaunch();
                        if al.is_enabled().unwrap_or(false) {
                            let _ = al.disable();
                        } else {
                            let _ = al.enable();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        toggle_win(tray.app_handle());
                    }
                })
                .build(app)?;
            // 每 0.1 秒看一眼鼠标：在机器上就接住点击，在透明处就让它点穿到下面的窗口
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut ign = false;
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let Some(w) = handle.get_webview_window("main") else { continue };
                    if !w.is_visible().unwrap_or(false) {
                        continue;
                    }
                    let (Ok(c), Ok(p), Ok(sf)) = (w.cursor_position(), w.outer_position(), w.scale_factor()) else { continue };
                    let r = *handle.state::<Hit>().0.lock().unwrap();
                    let x = (c.x - p.x as f64) / sf;
                    let y = (c.y - p.y as f64) / sf;
                    let inside = x >= r.0 - 4.0 && x <= r.0 + r.2 + 4.0 && y >= r.1 - 4.0 && y <= r.1 + r.3 + 4.0;
                    if inside == ign {
                        ign = !inside;
                        let _ = w.set_ignore_cursor_events(ign);
                    }
                }
            });
            if let Ok(dir) = app.path().app_config_dir() {
                load_user_agents(&dir.join("agents.json"));
            }
            let gh = app.handle().clone();
            std::thread::spawn(move || guard_loop(gh));
            // 默认放在主屏幕顶上正中间；之后前端会换成上次拖到的位置
            if let Some(w) = app.get_webview_window("main") {
                if let Ok(Some(m)) = w.primary_monitor() {
                    let size = m.size().to_logical::<f64>(m.scale_factor());
                    let _ = w.set_position(LogicalPosition::new(size.width / 2.0 - 200.0, 4.0));
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    // 真的借本机 Codex 问一句（要几秒、会用一点额度），平时不跑：cargo test -- --ignored
    #[test]
    #[ignore]
    fn codex_answers() {
        let t = super::run_codex("Reply with exactly: {\"ok\":true}").expect("codex");
        assert!(t.contains("ok"), "{t}");
    }

    // 每个 AI 对话在哪个文件夹干活、叫什么：cargo test -- --ignored conv_where --nocapture
    #[test]
    #[ignore]
    fn conv_where() {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::everything());
        for (pid, p) in sys.processes() {
            if let Some(a) = super::conv_agent(p) {
                println!("{} {} 文件夹 {:?} 对话名 {:?}", a.name, pid, super::folder(p), super::conv_name(p));
            }
        }
    }

    // 以对话为主：每个 AI 对话底下开了什么（AI_AGENT 结尾 harness = 工具帮手，agent = AI 动手开的）：cargo test -- --ignored conv_tree --nocapture
    #[test]
    #[ignore]
    fn conv_tree() {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::everything());
        let (listen, _) = super::sockets();
        let skip = ["conhost", "cmd", "bash", "sh", "powershell", "pwsh", "uv", "uvx"];
        let mut orphans = vec![];
        for (pid, p) in sys.processes() {
            if let Some(t) = super::tag_of(&sys, *pid) {
                if t.conv.is_none() && !skip.contains(&super::stem(&p.name().to_string_lossy()).as_str()) {
                    orphans.push(format!("    {} {} {} {:.0}MB", pid, p.name().to_string_lossy(), super::sig(p), p.memory() as f64 / 1048576.0));
                }
            }
            let Some(a) = super::conv_agent(p) else { continue };
            let kids = super::descendants(&sys, *pid);
            let all: f64 = kids.iter().filter_map(|k| sys.process(*k)).map(|k| k.memory() as f64).sum::<f64>() + p.memory() as f64;
            println!("== {}「{}」 {:?} 一共 {:.0}MB（对话本身 {:.0}MB）", a.name, super::conv_name(p).unwrap_or_default(), super::folder(p), all / 1048576.0, p.memory() as f64 / 1048576.0);
            for k in kids {
                let Some(c) = sys.process(k) else { continue };
                let s = super::stem(&c.name().to_string_lossy());
                if skip.contains(&s.as_str()) {
                    continue;
                }
                let role = super::env_val(c, "AI_AGENT").map(|v| if v.ends_with("_harness") { "工具" } else { "动手开的" }).unwrap_or("?");
                let port = listen.get(&k.as_u32()).map(|v| format!(" :{v:?}")).unwrap_or_default();
                println!("    [{role}] {} {} {}{} {:.0}MB 父 {:?}", k, c.name().to_string_lossy(), super::sig(c), port, c.memory() as f64 / 1048576.0, c.parent().and_then(|x| sys.process(x)).map(|x| x.name().to_string_lossy().to_string()));
            }
        }
        println!("== 开它的对话没了（只剩标记）");
        for o in orphans {
            println!("{o}");
        }
    }

    // 每个程序是谁开的（只列 AI 开的）：cargo test -- --ignored who_opened --nocapture
    #[test]
    #[ignore]
    fn who_opened() {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::everything());
        let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
        for (pid, p) in sys.processes() {
            if let Some(t) = super::tag_of(&sys, *pid) {
                let how = if t.conv.is_some() { "找到对话" } else if t.thread.is_some() { "对话编号" } else { "只有标记" };
                *tally.entry(format!("{} · {how}", t.agent)).or_default() += 1;
                if t.conv.is_none() {
                    println!("{} {} → {} {how} {:?}", pid, p.name().to_string_lossy(), t.agent, super::tag_label(&sys, &t));
                }
            }
        }
        for (k, n) in tally {
            println!("== {k}: {n}");
        }
    }

    // 各家 AI 工具会给自己开的程序留标记（环境变量，子孙都继承）：只看白名单里的名字，值只打印不敏感的几个
    // cargo test -- --ignored env_markers --nocapture
    #[test]
    #[ignore]
    fn env_markers() {
        const KEYS: &[&str] = &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SESSION_NAME", "CODEX_THREAD_ID", "CODEX_SANDBOX", "CODEX_CI", "CODEX_MANAGED_BY_NPM", "GEMINI_CLI", "CURSOR_AGENT", "CURSOR_TRACE_ID", "OPENCODE_CLIENT", "AI_AGENT", "AUGMENT_AGENT", "ANTIGRAVITY_AGENT", "COPILOT_MODEL", "COPILOT_AGENT", "AIDER_MODEL", "CLINE_ACTIVE", "TERM_PROGRAM"];
        const SHOW: &[&str] = &["CLAUDE_CODE_ENTRYPOINT", "CODEX_THREAD_ID", "AI_AGENT", "TERM_PROGRAM", "CURSOR_AGENT", "GEMINI_CLI", "OPENCODE_CLIENT"];
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::everything());
        let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
        for (pid, p) in sys.processes() {
            let mut hit = vec![];
            for e in p.environ() {
                let e = e.to_string_lossy();
                let Some((k, v)) = e.split_once('=') else { continue };
                if KEYS.contains(&k) {
                    hit.push(if SHOW.contains(&k) { format!("{k}={v}") } else { k.to_string() });
                }
            }
            if !hit.is_empty() {
                hit.sort();
                *tally.entry(hit.iter().map(|h| h.split('=').next().unwrap()).collect::<Vec<_>>().join("+")).or_default() += 1;
                println!("{} {} {}", pid, p.name().to_string_lossy(), hit.join(" "));
            }
        }
        for (k, n) in tally {
            println!("== {k}: {n}");
        }
    }

    // 用这台电脑上真的某个程序拆一次（默认 claude.exe，MEMO_TEST_EXE=codex.exe 换别的）：cargo test -- --ignored claude_parts --nocapture
    #[test]
    #[ignore]
    fn claude_parts() {
        let mut w = super::Watch { sys: sysinfo::System::new_all(), first_seen: Default::default(), last_conn: Default::default(), cc_busy: Default::default(), who: Default::default() };
        let pids: Vec<u32> = w.sys.processes().iter().filter(|(_, p)| p.name().to_string_lossy().eq_ignore_ascii_case(&std::env::var("MEMO_TEST_EXE").unwrap_or("claude.exe".into()))).map(|(k, _)| k.as_u32()).collect();
        super::snap_of(&mut w);
        let r = super::parts_of(&mut w, pids);
        for p in &r.parts {
            println!("{} ×{} {:.2} GB", p.kind, p.n, p.ram);
        }
        let snap = super::snap_of(&mut w);
        for c in &snap.chats {
            println!("== {}「{}」{:?} 闲 {} 分 一共 {:.0} MB（对话本身 {:.0}）结束={:?}", c.agent, c.name.clone().unwrap_or_default(), c.dir, c.idle_min, c.ram * 1024.0, c.own * 1024.0, c.end);
            for it in &c.items {
                println!("    [{}] {} {} ×{} {:.0} MB", it.role, it.kind, it.what, it.n, it.ram * 1024.0);
            }
        }
        println!("系统程序合计 {:.2} GB", snap.sys_ram);
        for g in snap.groups.iter().filter(|g| g.score >= 2 || g.why.iter().any(|x| x.k == "perConv")) {
            let why: Vec<String> = g.why.iter().map(|x| format!("{}={}", x.k, x.v)).collect();
            println!("[{}] {} ×{} {:.0} MB  {}", g.score, g.name, g.n, g.ram * 1024.0, why.join(" "));
        }
        assert!(!r.parts.is_empty());
    }
}
