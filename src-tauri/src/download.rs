//! 下载层：HLS 离线下载 + 本地管理。
//!
//! 口径（与上游 README 的使用须知一致）：个人离线自用；全局只跑 1 个任务、
//! 任务内分片顺序下载（并发 1），不做全站抓取、不高频轮询。
//!
//! 流水线：入队 → 开始时才调 `api_play` 取新鲜地址（`auth_key` 会过期，旧地址不存）
//! → 解析 m3u8（主/媒体列表、AES-128 key、EXT-X-MAP）→ 逐片下载解密存 `.part/`
//! （已存分片跳过 = 断点续传）→ 拼接为成品文件（TS → `.ts`，fMP4 → `.mp4`）。
//!
//! 命名：目录按作品名，文件名为「作品名 - 第NN集[.集标题].ts」，见 [`file_stem`]。
//! 索引 `downloads/downloads.json` 照 `session.rs` 的 JSON 落盘模式，不引数据库。

use crate::api::Api;
use crate::net::{edge, fetch_bytes};
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, State};

/* ───────────────────────── 数据模型 ───────────────────────── */

pub const ST_QUEUED: &str = "queued";
pub const ST_DOWNLOADING: &str = "downloading";
pub const ST_PAUSED: &str = "paused";
pub const ST_DONE: &str = "done";
pub const ST_FAILED: &str = "failed";
pub const ST_CANCELED: &str = "canceled";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DlTask {
    /// `{video_id}_{ep}`
    pub id: String,
    pub video_id: i64,
    pub ep: u32,
    /// 作品名（入队时快照，离线/下架后仍可显示与命名）
    pub title: String,
    #[serde(default)]
    pub ep_title: String,
    #[serde(default)]
    pub total_episodes: u32,
    /// 本集时长（秒），来自详情/播放信息；0 = 未知
    #[serde(default)]
    pub duration: u64,
    /// 封面原图地址（快照）；本地解密封面在作品目录 cover.jpg
    #[serde(default)]
    pub cover: String,
    pub status: String,
    /// 成品文件名（含扩展名），完成后才有
    #[serde(default)]
    pub file_name: Option<String>,
    /// 成品相对 downloads 根目录的路径，完成后才有
    #[serde(default)]
    pub rel_path: Option<String>,
    /// `ts` | `mp4`
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub segs_done: u32,
    #[serde(default)]
    pub segs_total: u32,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub finished_at: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn task_id(video_id: i64, ep: u32) -> String {
    format!("{video_id}_{ep}")
}

/* ───────────────────────── 命名 ───────────────────────── */

/// 文件名清洗：非法字符/控制字符 → 空格，合并空白，去首尾点与空格，限长（按字节、保 UTF-8 边界）。
pub fn sanitize_name(raw: &str, max_bytes: usize) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim().trim_matches('.').trim().to_string();
    if s.len() <= max_bytes {
        return s;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].trim().to_string()
}

/// 集号标签：`第01集`（总集数 ≥100 时 3 位）。
pub fn ep_label(ep: u32, total: u32) -> String {
    let width = if total >= 100 { 3 } else { 2 };
    format!("第{ep:0width$}集", width = width)
}

/// 集标题若以「第N集」开头且 N 就是本集集号，剥掉这个前缀（避免「第03集 第3集 初遇」式重复）；
/// 剩下的部分才是真正的集标题。N 与本集不符时原样保留。
fn strip_ep_prefix(ep_title: &str, ep: u32) -> String {
    let t = ep_title.trim();
    let Some(rest) = t.strip_prefix('第') else { return t.to_string() };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return t.to_string();
    }
    let Some(after) = rest[digits.len()..].strip_prefix('集') else {
        return t.to_string();
    };
    if digits.parse::<u32>() == Ok(ep) {
        after.trim().to_string()
    } else {
        t.to_string()
    }
}

/// 成品文件主名：「作品名 - 第NN集」+ 可选集标题（集标题自带集号前缀时先剥掉，不重复叠加）。
pub fn file_stem(title: &str, ep: u32, total: u32, ep_title: &str) -> String {
    let name = sanitize_name(title, 80);
    let name = if name.is_empty() {
        "未命名".to_string()
    } else {
        name
    };
    let label = ep_label(ep, total);
    let mut stem = format!("{name} - {label}");
    let et = sanitize_name(&strip_ep_prefix(ep_title, ep), 40);
    if !et.is_empty() {
        stem.push(' ');
        stem.push_str(&et);
    }
    stem
}

/// 作品目录名（根目录下一级）。
pub fn work_dir_name(title: &str, video_id: i64) -> String {
    let name = sanitize_name(title, 80);
    if name.is_empty() {
        format!("video_{video_id}")
    } else {
        name
    }
}

/* ───────────────────────── 管理器 ───────────────────────── */

const STOP_RUN: u8 = 0;
const STOP_PAUSE: u8 = 1;
const STOP_CANCEL: u8 = 2;

pub struct DownloadManager {
    /// downloads 根目录（数据目录/downloads），bind 前为 None
    root: Mutex<Option<PathBuf>>,
    /// 数据目录迁移前的旧 downloads 根目录：索引/成品的回退来源，bind 时按需设置
    legacy_root: Mutex<Option<PathBuf>>,
    tasks: Mutex<Vec<DlTask>>,
    flags: Mutex<HashMap<String, Arc<AtomicU8>>>,
    worker_alive: Mutex<bool>,
    app: Mutex<Option<AppHandle>>,
    /// 下载完成后自动无损换封装为 MP4（仅 TS 源），持久化在 downloads/prefs.json
    remux_pref: Mutex<bool>,
    /// 正在转换中的任务 id（防重复触发）
    remuxing: Mutex<std::collections::HashSet<String>>,
}

impl DownloadManager {
    pub fn new() -> Self {
        Self {
            root: Mutex::new(None),
            legacy_root: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            flags: Mutex::new(HashMap::new()),
            worker_alive: Mutex::new(false),
            app: Mutex::new(None),
            remux_pref: Mutex::new(false),
            remuxing: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// 启动时绑定：读回索引；上次被杀掉时停在 downloading/queued 的降级为 paused（可续传）。
    /// legacy_data_dir：数据目录迁移前的旧数据目录；主目录还没有索引时先读它的索引，
    /// 成品文件按「主目录 → 旧目录」顺序解析，旧文件不搬也能播、能删。
    pub fn bind(&self, app: AppHandle, data_dir: PathBuf, legacy_data_dir: Option<PathBuf>) {
        let root = data_dir.join("downloads");
        let _ = std::fs::create_dir_all(&root);
        let legacy_dl = legacy_data_dir
            .map(|d| d.join("downloads"))
            .filter(|p| p.is_dir() && *p != root);
        let index = root.join("downloads.json");
        let index_text = std::fs::read_to_string(&index).ok().or_else(|| {
            legacy_dl
                .as_ref()
                .and_then(|d| std::fs::read_to_string(d.join("downloads.json")).ok())
        });
        let mut loaded: Vec<DlTask> = Vec::new();
        if let Some(text) = index_text {
            if let Ok(mut v) = serde_json::from_str::<Vec<DlTask>>(&text) {
                for t in v.iter_mut() {
                    if t.status == ST_DOWNLOADING || t.status == ST_QUEUED {
                        t.status = ST_PAUSED.to_string();
                    }
                }
                loaded = v;
            }
        }
        *self.tasks.lock().unwrap() = loaded;
        // 换封装偏好（与索引同目录的 prefs.json）
        let pref_on = std::fs::read_to_string(root.join("prefs.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("remux_mp4").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        *self.remux_pref.lock().unwrap() = pref_on;
        *self.root.lock().unwrap() = Some(root);
        *self.legacy_root.lock().unwrap() = legacy_dl;
        *self.app.lock().unwrap() = Some(app);
        self.persist();
    }

    pub fn remux_pref(&self) -> bool {
        *self.remux_pref.lock().unwrap()
    }

    pub fn set_remux_pref(&self, on: bool) {
        *self.remux_pref.lock().unwrap() = on;
        if let Some(root) = self.root() {
            let _ = std::fs::write(root.join("prefs.json"), json!({ "remux_mp4": on }).to_string());
        }
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.root.lock().unwrap().clone()
    }

    pub fn legacy_root(&self) -> Option<PathBuf> {
        self.legacy_root.lock().unwrap().clone()
    }

    /// 主目录 + 旧目录（删除/清理时逐个尝试）。
    fn all_roots(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(r) = self.root() {
            v.push(r);
        }
        if let Some(r) = self.legacy_root() {
            v.push(r);
        }
        v
    }

    /// 成品相对路径 → 绝对路径：主目录有就用主目录，否则回退旧目录。
    fn resolve_file(&self, rel: &str) -> Option<PathBuf> {
        let primary = self.root()?.join(rel);
        if primary.is_file() {
            return Some(primary);
        }
        if let Some(legacy) = self.legacy_root() {
            let p = legacy.join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
        Some(primary)
    }

    pub fn list(&self) -> Vec<DlTask> {
        self.tasks.lock().unwrap().clone()
    }

    fn flag(&self, id: &str) -> Arc<AtomicU8> {
        self.flags
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(AtomicU8::new(STOP_RUN)))
            .clone()
    }

    fn stop_mode(&self, id: &str) -> u8 {
        self.flag(id).load(Ordering::SeqCst)
    }

    fn set_stop(&self, id: &str, mode: u8) {
        self.flag(id).store(mode, Ordering::SeqCst);
    }

    fn update<F: FnOnce(&mut DlTask)>(&self, id: &str, f: F) {
        let mut changed = false;
        {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                f(t);
                changed = true;
            }
        }
        if changed {
            self.persist();
            self.emit_changed();
        }
    }

    fn persist(&self) {
        let Some(root) = self.root() else { return };
        let tasks = self.tasks.lock().unwrap().clone();
        if let Ok(text) = serde_json::to_string_pretty(&tasks) {
            let _ = std::fs::write(root.join("downloads.json"), text);
        }
    }

    fn emit_changed(&self) {
        if let Some(app) = self.app.lock().unwrap().clone() {
            let _ = app.emit("download-changed", self.list());
        }
    }

    /// 已完成任务的成品绝对路径（离线播放用）。
    pub fn done_file(&self, video_id: i64, ep: u32) -> Option<(PathBuf, String, u64)> {
        let tasks = self.tasks.lock().unwrap();
        let t = tasks.iter().find(|t| t.video_id == video_id && t.ep == ep && t.status == ST_DONE)?;
        let rel = t.rel_path.as_ref()?;
        let path = self.resolve_file(rel)?;
        if path.is_file() {
            Some((path, t.format.clone().unwrap_or_else(|| "ts".into()), t.duration))
        } else {
            None
        }
    }

    /// 入队（已存在同任务：done 直接返回；failed/paused/canceled 重置为 queued 续传）。
    fn enqueue(&self, task: DlTask) -> DlTask {
        let id = task.id.clone();
        {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(exist) = tasks.iter_mut().find(|t| t.id == id) {
                if exist.status != ST_DONE {
                    exist.status = ST_QUEUED.to_string();
                    exist.error = None;
                    if exist.title.is_empty() {
                        exist.title = task.title.clone();
                    }
                }
                let out = exist.clone();
                drop(tasks);
                self.set_stop(&id, STOP_RUN);
                self.persist();
                self.emit_changed();
                self.kick();
                return out;
            }
            tasks.push(task.clone());
        }
        self.set_stop(&id, STOP_RUN);
        self.persist();
        self.emit_changed();
        self.kick();
        task
    }

    /// 全局顺序队列：同一时刻只跑一个任务。
    fn kick(&self) {
        {
            let mut alive = self.worker_alive.lock().unwrap();
            if *alive {
                return;
            }
            *alive = true;
        }
        let Some(app) = self.app.lock().unwrap().clone() else {
            *self.worker_alive.lock().unwrap() = false;
            return;
        };
        tauri::async_runtime::spawn(async move {
            loop {
                let mgr: State<DownloadManager> = app.state();
                let next = {
                    let tasks = mgr.tasks.lock().unwrap();
                    tasks.iter().find(|t| t.status == ST_QUEUED).map(|t| t.id.clone())
                };
                let Some(id) = next else { break };
                run_task(&app, &mgr, &id).await;
            }
            let mgr: State<DownloadManager> = app.state();
            *mgr.worker_alive.lock().unwrap() = false;
        });
    }
}

/* ───────────────────────── HLS 解析与解密 ───────────────────────── */

#[derive(Clone, Debug)]
struct KeySpec {
    method: String,
    uri: Option<String>,
    iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug)]
struct SegSpec {
    url: String,
    seq: u64,
    key: Option<KeySpec>,
}

#[derive(Debug)]
pub(crate) struct Playlist {
    /// 主列表的候选 (bandwidth, url)
    variants: Vec<(u64, String)>,
    segs: Vec<SegSpec>,
    map_uri: Option<String>,
    target_dur: f64,
}

fn parse_attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        rest = &rest[eq + 1..];
        let val = if let Some(stripped) = rest.strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => {
                    let v = stripped[..end].to_string();
                    rest = &stripped[end + 1..];
                    v
                }
                None => {
                    let v = stripped.to_string();
                    rest = "";
                    v
                }
            }
        } else {
            match rest.find(',') {
                Some(end) => {
                    let v = rest[..end].to_string();
                    rest = &rest[end + 1..];
                    v
                }
                None => {
                    let v = rest.to_string();
                    rest = "";
                    v
                }
            }
        };
        out.insert(key, val);
        rest = rest.strip_prefix(',').unwrap_or(rest);
    }
    out
}

fn parse_iv(s: &str) -> Option<[u8; 16]> {
    let hex = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let padded = format!("{hex:0>32}");
    let bytes = (0..16)
        .map(|i| u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&bytes);
    Some(iv)
}

fn seq_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

/// 解析 m3u8 文本（主列表 → variants；媒体列表 → segs）。相对地址按列表地址绝对化。
pub(crate) fn parse_playlist(text: &str, base_url: &str) -> Playlist {
    let mut pl = Playlist {
        variants: Vec::new(),
        segs: Vec::new(),
        map_uri: None,
        target_dur: 0.0,
    };
    let mut media_seq: u64 = 0;
    let mut cur_key: Option<KeySpec> = None;
    let mut pending_bw: Option<u64> = None;
    let mut have_inf = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(tag) = line.strip_prefix('#') {
            if let Some(v) = tag.strip_prefix("EXT-X-MEDIA-SEQUENCE:") {
                media_seq = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = tag.strip_prefix("EXT-X-TARGETDURATION:") {
                pl.target_dur = v.trim().parse().unwrap_or(0.0);
            } else if let Some(v) = tag.strip_prefix("EXT-X-KEY:") {
                let attrs = parse_attrs(v);
                let method = attrs.get("METHOD").cloned().unwrap_or_default();
                if method == "NONE" {
                    cur_key = None;
                } else {
                    cur_key = Some(KeySpec {
                        method,
                        uri: attrs
                            .get("URI")
                            .map(|u| crate::media::absolutize(base_url, u)),
                        iv: attrs.get("IV").and_then(|s| parse_iv(s)),
                    });
                }
            } else if let Some(v) = tag.strip_prefix("EXT-X-MAP:") {
                let attrs = parse_attrs(v);
                if let Some(u) = attrs.get("URI") {
                    pl.map_uri = Some(crate::media::absolutize(base_url, u));
                }
            } else if let Some(v) = tag.strip_prefix("EXT-X-STREAM-INF:") {
                let attrs = parse_attrs(v);
                pending_bw = Some(attrs.get("BANDWIDTH").and_then(|s| s.parse().ok()).unwrap_or(0));
            } else if tag.starts_with("EXTINF:") {
                have_inf = true;
            }
            continue;
        }
        // 非 # 行：variant 或分片地址
        let abs = crate::media::absolutize(base_url, line);
        if let Some(bw) = pending_bw.take() {
            pl.variants.push((bw, abs));
        } else if have_inf || pl.map_uri.is_some() || !pl.segs.is_empty() {
            let seq = media_seq + pl.segs.len() as u64;
            pl.segs.push(SegSpec {
                url: abs,
                seq,
                key: cur_key.clone(),
            });
        } else {
            // 没有 EXTINF 的裸分片行也收（个别站点的简化列表）
            let seq = media_seq + pl.segs.len() as u64;
            pl.segs.push(SegSpec {
                url: abs,
                seq,
                key: cur_key.clone(),
            });
        }
    }
    pl
}

/// AES-128-CBC 解密一个分片；尾部 PKCS#7 填充合法时剥掉，不合法则原样保留（与播放器行为对齐的容错）。
pub fn decrypt_segment(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 || key.len() != 16 {
        return None;
    }
    let cipher = Aes128::new_from_slice(key).ok()?;
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut ct = [0u8; 16];
        ct.copy_from_slice(chunk);
        let mut block = ct;
        cipher.decrypt_block((&mut block).into());
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev = ct;
    }
    // PKCS#7 剥填充
    if let Some(&pad) = out.last() {
        let pad = pad as usize;
        if pad > 0 && pad <= 16 && out.len() >= pad && out[out.len() - pad..].iter().all(|b| *b as usize == pad) {
            out.truncate(out.len() - pad);
        }
    }
    Some(out)
}

/* ───────────────────────── 下载执行 ───────────────────────── */

fn token_of(app: &AppHandle) -> Option<String> {
    let api: State<Api> = app.state();
    api.session.token()
}

async fn get_json(app: &AppHandle, path: &str) -> Result<Value, String> {
    let token = token_of(app);
    edge("GET", path, None, token.as_deref()).await
}

async fn post_json(app: &AppHandle, path: &str, body: Value) -> Result<Value, String> {
    let token = token_of(app);
    edge("POST", path, Some(body), token.as_deref()).await
}

/// 从播放信息里取可下载地址：旧字段 `video_url` 优先，其次新接口的 `streams.main/hevc`。
fn play_url_of(play: &Value) -> String {
    if let Some(u) = play.get("video_url").and_then(|v| v.as_str()) {
        if !u.is_empty() {
            return u.to_string();
        }
    }
    for key in ["main", "hevc"] {
        if let Some(u) = play
            .get("streams")
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
        {
            if !u.is_empty() {
                return u.to_string();
            }
        }
    }
    String::new()
}

/// 取播放信息：优先新接口（POST .../episodes/{ep}/playback，旧接口的 video_url 已恒为空），
/// 失败或没有可用地址时再退回旧接口。
async fn fetch_play_info(app: &AppHandle, video_id: i64, ep: u32) -> Result<Value, String> {
    let mut primary: Option<Value> = None;
    if let Ok(v) = post_json(
        app,
        &format!("/api/videos/{video_id}/episodes/{ep}/playback"),
        json!({}),
    )
    .await
    {
        if !play_url_of(&v).is_empty() {
            return Ok(v);
        }
        primary = Some(v);
    }
    match get_json(app, &format!("/api/videos/{video_id}/play?ep={ep}&page_texts=0")).await {
        Ok(v) => {
            if !play_url_of(&v).is_empty() || primary.is_none() {
                Ok(v)
            } else {
                Ok(primary.unwrap())
            }
        }
        Err(e) => match primary {
            Some(v) => Ok(v),
            None => Err(e),
        },
    }
}

/// 拉播放列表文本；主列表自动跟随最高码率一档（最多跟 2 层）。
async fn fetch_media_playlist(start_url: &str) -> Result<(String, Playlist), String> {
    let mut url = start_url.to_string();
    for _ in 0..3 {
        let (_, bytes) = fetch_bytes(&url).await?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        if !text.contains("#EXTM3U") {
            return Err("播放列表格式异常（不是 m3u8）".to_string());
        }
        let pl = parse_playlist(&text, &url);
        if pl.segs.is_empty() && !pl.variants.is_empty() {
            let best = pl
                .variants
                .iter()
                .max_by_key(|(bw, _)| *bw)
                .map(|(_, u)| u.clone())
                .unwrap();
            url = best;
            continue;
        }
        return Ok((url, pl));
    }
    Err("播放列表层级过深".to_string())
}

/// 跑一个任务（由 worker 顺序调用）。暂停/取消通过 stop flag 在分片间隙检查。
async fn run_task(app: &AppHandle, mgr: &DownloadManager, id: &str) {
    let Some(root) = mgr.root() else { return };
    let Some(mut task) = mgr.list().into_iter().find(|t| t.id == id) else {
        return;
    };
    mgr.set_stop(id, STOP_RUN);
    mgr.update(id, |t| {
        t.status = ST_DOWNLOADING.to_string();
        t.error = None;
    });

    let fail = |mgr: &DownloadManager, msg: String| {
        mgr.update(id, |t| {
            t.status = ST_FAILED.to_string();
            t.error = Some(msg);
        });
    };

    // 1) 元数据补全（入队时已尽量填；这里兜底）
    if task.title.is_empty() || task.duration == 0 {
        if let Ok(d) = get_json(app, &format!("/api/videos/{}", task.video_id)).await {
            let title = d.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let total = d.get("total_episodes").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let (ep_title, dur) = d
                .get("episodes")
                .and_then(|v| v.as_array())
                .and_then(|eps| eps.iter().find(|e| e.get("ep_num").and_then(|n| n.as_u64()) == Some(task.ep as u64)))
                .map(|e| {
                    (
                        e.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        e.get("duration").and_then(|v| v.as_u64()).unwrap_or(0),
                    )
                })
                .unwrap_or_default();
            mgr.update(id, |t| {
                if !title.is_empty() {
                    t.title = title.clone();
                }
                if total > 0 {
                    t.total_episodes = total;
                }
                if !ep_title.is_empty() {
                    t.ep_title = ep_title.clone();
                }
                if dur > 0 {
                    t.duration = dur;
                }
            });
            task = mgr.list().into_iter().find(|t| t.id == id).unwrap_or(task);
        }
    }

    let dir_name = work_dir_name(&task.title, task.video_id);
    let work_dir = root.join(&dir_name);
    if std::fs::create_dir_all(&work_dir).is_err() {
        fail(mgr, "无法创建下载目录".to_string());
        return;
    }

    // 封面快照（解密后的明图，失败不拦下载）
    let cover_file = work_dir.join("cover.jpg");
    if !task.cover.is_empty() && !cover_file.is_file() {
        if let Ok((_, bytes)) = crate::media::load_image(&task.cover).await {
            let _ = std::fs::write(&cover_file, bytes.as_ref());
        }
    }

    // 2) 取新鲜播放地址
    let play = match fetch_play_info(app, task.video_id, task.ep).await {
        Ok(v) => v,
        Err(e) => {
            fail(mgr, format!("取播放地址失败：{e}"));
            return;
        }
    };
    let video_url = play_url_of(&play);
    if video_url.is_empty() {
        fail(mgr, "暂无播放地址".to_string());
        return;
    }
    let dur = play.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
    if dur > 0 && task.duration == 0 {
        mgr.update(id, |t| t.duration = dur);
        task.duration = dur;
    }

    // 3) 解析播放列表
    let (_pl_url, pl) = match fetch_media_playlist(&video_url).await {
        Ok(v) => v,
        Err(e) => {
            fail(mgr, e);
            return;
        }
    };
    if pl.segs.is_empty() {
        fail(mgr, "播放列表没有分片".to_string());
        return;
    }
    let is_mp4 = pl.map_uri.is_some();
    let ext = if is_mp4 { "mp4" } else { "ts" };
    let stem = file_stem(&task.title, task.ep, task.total_episodes, &task.ep_title);
    let file_name = format!("{stem}.{ext}");
    let part_dir = work_dir.join(format!("{stem}.part"));
    if std::fs::create_dir_all(&part_dir).is_err() {
        fail(mgr, "无法创建临时目录".to_string());
        return;
    }
    mgr.update(id, |t| {
        t.segs_total = pl.segs.len() as u32;
        t.format = Some(ext.to_string());
    });

    // 4) init 段（fMP4）
    if let Some(map_url) = &pl.map_uri {
        let init_file = part_dir.join("init.bin");
        if !init_file.is_file() {
            match fetch_bytes(map_url).await {
                Ok((_, bytes)) => {
                    if std::fs::write(&init_file, &bytes).is_err() {
                        fail(mgr, "写入 init 段失败".to_string());
                        return;
                    }
                }
                Err(e) => {
                    fail(mgr, format!("下载 init 段失败：{e}"));
                    return;
                }
            }
        }
    }

    // 5) 逐片下载 + 解密（顺序、低并发；已存分片跳过 = 续传）
    let mut key_cache: HashMap<String, Vec<u8>> = HashMap::new();
    let mut refreshed = false;
    let mut idx = 0usize;
    let mut bytes_done: u64 = {
        // 续传时把已有分片字节计入进度
        let mut sum = 0u64;
        let mut done = 0u32;
        for i in 0..pl.segs.len() {
            let f = part_dir.join(format!("{i:05}.seg"));
            if let Ok(m) = std::fs::metadata(&f) {
                if m.len() > 0 {
                    sum += m.len();
                    done += 1;
                }
            }
        }
        mgr.update(id, |t| {
            t.segs_done = done;
            t.bytes_done = sum;
        });
        sum
    };

    let mut current_pl = pl;
    'outer: while idx < current_pl.segs.len() {
        // 暂停/取消检查点
        match mgr.stop_mode(id) {
            STOP_PAUSE => {
                mgr.update(id, |t| t.status = ST_PAUSED.to_string());
                return;
            }
            STOP_CANCEL => {
                let _ = std::fs::remove_dir_all(&part_dir);
                mgr.update(id, |t| {
                    t.status = ST_CANCELED.to_string();
                    t.segs_done = 0;
                    t.bytes_done = 0;
                });
                return;
            }
            _ => {}
        }

        let seg_file = part_dir.join(format!("{idx:05}.seg"));
        if seg_file.is_file() && std::fs::metadata(&seg_file).map(|m| m.len() > 0).unwrap_or(false) {
            idx += 1;
            continue;
        }
        let seg = current_pl.segs[idx].clone();

        let raw = match fetch_bytes(&seg.url).await {
            Ok((_, b)) => b,
            Err(e) => {
                // 地址过期（auth_key）时刷新一次播放地址续传
                if !refreshed && (e.contains("401") || e.contains("403") || e.contains("HTTP 404")) {
                    refreshed = true;
                    match fetch_play_info(app, task.video_id, task.ep).await {
                        Ok(p2) => {
                            let u2 = play_url_of(&p2);
                            if !u2.is_empty() {
                                if let Ok((_, pl2)) = fetch_media_playlist(&u2).await {
                                    if pl2.segs.len() == current_pl.segs.len() {
                                        current_pl = pl2;
                                        continue 'outer;
                                    }
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                fail(mgr, format!("第 {} 片下载失败：{e}", idx + 1));
                return;
            }
        };

        // 解密
        let plain = match &seg.key {
            None => raw,
            Some(k) if k.method == "NONE" || k.method.is_empty() => raw,
            Some(k) => {
                let Some(uri) = &k.uri else {
                    fail(mgr, "分片加密但缺少密钥地址".to_string());
                    return;
                };
                if !key_cache.contains_key(uri) {
                    match fetch_bytes(uri).await {
                        Ok((_, kb)) => {
                            key_cache.insert(uri.clone(), kb);
                        }
                        Err(e) => {
                            fail(mgr, format!("下载密钥失败：{e}"));
                            return;
                        }
                    }
                }
                let key = key_cache.get(uri).unwrap();
                let iv = k.iv.unwrap_or_else(|| seq_iv(seg.seq));
                match decrypt_segment(key, &iv, &raw) {
                    Some(p) => p,
                    None => {
                        fail(mgr, format!("第 {} 片解密失败", idx + 1));
                        return;
                    }
                }
            }
        };

        // 先写临时名再改名，避免半截文件被当成续传成果
        let tmp = part_dir.join(format!("{idx:05}.tmp"));
        if std::fs::write(&tmp, &plain).is_err() || std::fs::rename(&tmp, &seg_file).is_err() {
            fail(mgr, "写入分片失败（磁盘空间？）".to_string());
            return;
        }
        bytes_done += plain.len() as u64;
        idx += 1;
        let done_count = idx as u32;
        let bd = bytes_done;
        mgr.update(id, |t| {
            t.segs_done = done_count;
            t.bytes_done = bd;
        });
    }

    if mgr.stop_mode(id) == STOP_CANCEL {
        let _ = std::fs::remove_dir_all(&part_dir);
        mgr.update(id, |t| t.status = ST_CANCELED.to_string());
        return;
    }

    // 6) 拼接（init + 分片顺序）→ 临时成品 → 原子改名
    let final_path = work_dir.join(&file_name);
    let tmp_final = work_dir.join(format!("{stem}.tmp"));
    let merged = (|| -> Result<u64, String> {
        let mut out = std::fs::File::create(&tmp_final).map_err(|e| e.to_string())?;
        let mut total: u64 = 0;
        use std::io::Write;
        if is_mp4 {
            let init = std::fs::read(part_dir.join("init.bin")).map_err(|e| e.to_string())?;
            out.write_all(&init).map_err(|e| e.to_string())?;
            total += init.len() as u64;
        }
        for i in 0..current_pl.segs.len() {
            let data = std::fs::read(part_dir.join(format!("{i:05}.seg"))).map_err(|e| e.to_string())?;
            out.write_all(&data).map_err(|e| e.to_string())?;
            total += data.len() as u64;
        }
        out.flush().map_err(|e| e.to_string())?;
        Ok(total)
    })();
    let total_size = match merged {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_final);
            fail(mgr, format!("拼接失败：{e}"));
            return;
        }
    };
    if std::fs::rename(&tmp_final, &final_path).is_err() {
        fail(mgr, "成品改名失败".to_string());
        return;
    }
    let _ = std::fs::remove_dir_all(&part_dir);
    let rel = format!("{dir_name}/{file_name}");
    let done_at = now_ms();
    mgr.update(id, |t| {
        t.status = ST_DONE.to_string();
        t.file_name = Some(file_name.clone());
        t.rel_path = Some(rel);
        t.size = total_size;
        t.bytes_done = total_size;
        t.segs_done = t.segs_total;
        t.finished_at = done_at;
        t.error = None;
    });

    // 自动换封装：源为 TS 且设置开启时，把成品无损转成 MP4；失败保留 TS 并记诊断日志
    if !is_mp4 && mgr.remux_pref() {
        let app2 = app.clone();
        let id2 = id.to_string();
        let _ = tauri::async_runtime::spawn_blocking(move || {
            let dl = app2.state::<DownloadManager>();
            if let Err(e) = do_remux(&dl, &id2) {
                let dir = crate::data_root(app2.path().app_data_dir().ok());
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("debug.log")) {
                    let _ = writeln!(f, "auto remux {id2} failed: {e}");
                }
            }
        });
    }
}

/// 把已完成任务的 TS 成品无损换封装为 MP4（同目录同主名），成功后更新索引并删原 TS。
/// 转换在临时名上完成、校验通过才改名替换，任何失败都保留原始 TS。
fn do_remux(mgr: &DownloadManager, id: &str) -> Result<(), String> {
    let task = mgr
        .list()
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| "任务不存在".to_string())?;
    if task.status != ST_DONE {
        return Err("只有已完成的下载可以转换".into());
    }
    if task.format.as_deref() != Some("ts") {
        return Err("该集不是 TS 格式，无需转换".into());
    }
    let rel = task.rel_path.clone().ok_or_else(|| "缺少成品路径".to_string())?;
    let src = mgr.resolve_file(&rel).ok_or_else(|| "找不到成品文件".to_string())?;
    if !src.is_file() {
        return Err("成品文件不存在".into());
    }
    {
        let mut ing = mgr.remuxing.lock().unwrap();
        if !ing.insert(id.to_string()) {
            return Err("正在转换中".into());
        }
    }
    let result = (|| -> Result<u64, String> {
        let tmp = src.with_extension("mp4.tmp");
        let dst = src.with_extension("mp4");
        if let Err(e) = crate::remux::remux_ts_to_mp4(&src, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            let _ = std::fs::remove_file(&tmp);
            return Err("转换产物为空".into());
        }
        std::fs::rename(&tmp, &dst).map_err(|e| format!("成品改名失败：{e}"))?;
        let _ = std::fs::remove_file(&src);
        Ok(size)
    })();
    mgr.remuxing.lock().unwrap().remove(id);
    let size = result?;
    let new_name = task.file_name.clone().map(|n| {
        n.rsplit_once('.').map(|(stem, _)| format!("{stem}.mp4")).unwrap_or_else(|| format!("{n}.mp4"))
    });
    let new_rel = rel
        .rsplit_once('.')
        .map(|(stem, _)| format!("{stem}.mp4"))
        .unwrap_or_else(|| format!("{rel}.mp4"));
    mgr.update(id, |t| {
        t.file_name = new_name.clone();
        t.rel_path = Some(new_rel.clone());
        t.format = Some("mp4".to_string());
        t.size = size;
        t.bytes_done = size;
    });
    Ok(())
}

/* ───────────────────────── Tauri 命令 ───────────────────────── */

async fn build_task(app: &AppHandle, video_id: i64, ep: u32) -> Result<DlTask, String> {
    let d = get_json(app, &format!("/api/videos/{video_id}")).await?;
    let title = d.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let cover = d.get("cover").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let total = d.get("total_episodes").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let (ep_title, dur) = d
        .get("episodes")
        .and_then(|v| v.as_array())
        .and_then(|eps| eps.iter().find(|e| e.get("ep_num").and_then(|n| n.as_u64()) == Some(ep as u64)))
        .map(|e| {
            (
                e.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                e.get("duration").and_then(|v| v.as_u64()).unwrap_or(0),
            )
        })
        .unwrap_or_default();
    Ok(DlTask {
        id: task_id(video_id, ep),
        video_id,
        ep,
        title,
        ep_title,
        total_episodes: total,
        duration: dur,
        cover,
        status: ST_QUEUED.to_string(),
        file_name: None,
        rel_path: None,
        format: None,
        size: 0,
        segs_done: 0,
        segs_total: 0,
        bytes_done: 0,
        error: None,
        created_at: now_ms(),
        finished_at: 0,
    })
}

#[tauri::command]
pub async fn dl_enqueue(app: AppHandle, dl: State<'_, DownloadManager>, video_id: i64, ep: u32) -> Result<DlTask, String> {
    // 已有任务（哪怕元数据不全）直接走队列逻辑，避免为了入队先打一发详情
    if let Some(exist) = dl.list().into_iter().find(|t| t.video_id == video_id && t.ep == ep) {
        if exist.status == ST_DONE {
            return Ok(exist);
        }
        return Ok(dl.enqueue(exist));
    }
    let task = build_task(&app, video_id, ep).await?;
    Ok(dl.enqueue(task))
}

#[tauri::command]
pub async fn dl_enqueue_series(
    app: AppHandle,
    dl: State<'_, DownloadManager>,
    video_id: i64,
    eps: Option<Vec<u32>>,
) -> Result<Vec<DlTask>, String> {
    let want: Vec<u32> = match eps {
        Some(v) if !v.is_empty() => v,
        _ => {
            let d = get_json(&app, &format!("/api/videos/{video_id}")).await?;
            d.get("episodes")
                .and_then(|v| v.as_array())
                .map(|eps| {
                    eps.iter()
                        .filter_map(|e| e.get("ep_num").and_then(|n| n.as_u64()).map(|n| n as u32))
                        .collect()
                })
                .unwrap_or_default()
        }
    };
    if want.is_empty() {
        return Err("没有可下载的集数".to_string());
    }
    let mut out = Vec::new();
    // 首集走完整 build（带元数据快照），其余集复用同一份详情信息在 enqueue 里补全标题
    let first = build_task(&app, video_id, want[0]).await?;
    let (title, cover, total) = (first.title.clone(), first.cover.clone(), first.total_episodes);
    out.push(dl.enqueue(first));
    for ep in want.into_iter().skip(1) {
        if let Some(exist) = dl.list().into_iter().find(|t| t.video_id == video_id && t.ep == ep) {
            if exist.status != ST_DONE {
                out.push(dl.enqueue(exist));
            } else {
                out.push(exist);
            }
            continue;
        }
        let t = DlTask {
            id: task_id(video_id, ep),
            video_id,
            ep,
            title: title.clone(),
            ep_title: String::new(),
            total_episodes: total,
            duration: 0,
            cover: cover.clone(),
            status: ST_QUEUED.to_string(),
            file_name: None,
            rel_path: None,
            format: None,
            size: 0,
            segs_done: 0,
            segs_total: 0,
            bytes_done: 0,
            error: None,
            created_at: now_ms(),
            finished_at: 0,
        };
        out.push(dl.enqueue(t));
    }
    Ok(out)
}

#[tauri::command]
pub async fn dl_list(dl: State<'_, DownloadManager>) -> Result<Vec<DlTask>, String> {
    Ok(dl.list())
}

#[tauri::command]
pub async fn dl_pause(dl: State<'_, DownloadManager>, id: String) -> Result<(), String> {
    let st = dl.list().into_iter().find(|t| t.id == id).map(|t| t.status);
    match st.as_deref() {
        Some(ST_DOWNLOADING) => dl.set_stop(&id, STOP_PAUSE),
        Some(ST_QUEUED) => {
            dl.set_stop(&id, STOP_PAUSE);
            dl.update(&id, |t| t.status = ST_PAUSED.to_string());
        }
        _ => {}
    }
    Ok(())
}

#[tauri::command]
pub async fn dl_resume(dl: State<'_, DownloadManager>, id: String) -> Result<(), String> {
    if let Some(t) = dl.list().into_iter().find(|t| t.id == id) {
        if t.status != ST_DONE {
            dl.enqueue(t);
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn dl_cancel(dl: State<'_, DownloadManager>, id: String) -> Result<(), String> {
    dl.set_stop(&id, STOP_CANCEL);
    // 不在下载中的任务立即落定为 canceled 并清临时目录
    let task = dl.list().into_iter().find(|t| t.id == id);
    if let Some(t) = task {
        if t.status == ST_QUEUED || t.status == ST_PAUSED || t.status == ST_FAILED {
            {
                let stem = file_stem(&t.title, t.ep, t.total_episodes, &t.ep_title);
                for root in dl.all_roots() {
                    let part = root.join(work_dir_name(&t.title, t.video_id)).join(format!("{stem}.part"));
                    let _ = std::fs::remove_dir_all(part);
                }
            }
            dl.update(&id, |t| {
                t.status = ST_CANCELED.to_string();
                t.segs_done = 0;
                t.bytes_done = 0;
            });
        }
    }
    Ok(())
}

/// 删除一条记录：成品文件 + 临时目录 + 索引一起删（同命令内完成，不留半截状态）。
fn remove_task_files(dl: &DownloadManager, t: &DlTask) {
    // 成品可能在主目录或旧目录，逐个根目录清理
    for root in dl.all_roots() {
        if let Some(rel) = &t.rel_path {
            let _ = std::fs::remove_file(root.join(rel));
        }
        let stem = file_stem(&t.title, t.ep, t.total_episodes, &t.ep_title);
        let work = root.join(work_dir_name(&t.title, t.video_id));
        let _ = std::fs::remove_dir_all(work.join(format!("{stem}.part")));
        // 目录空了（只剩 cover 也算空）就一并清掉作品目录
        if let Ok(mut rd) = std::fs::read_dir(&work) {
            let entries: Vec<_> = rd.by_ref().collect();
            let only_cover = entries.len() == 1
                && entries[0].as_ref().map(|e| e.file_name().to_string_lossy() == "cover.jpg").unwrap_or(false);
            if entries.is_empty() || only_cover {
                let _ = std::fs::remove_dir_all(&work);
            }
        }
    }
}

#[tauri::command]
pub async fn dl_remove(dl: State<'_, DownloadManager>, id: String) -> Result<(), String> {
    dl.set_stop(&id, STOP_CANCEL);
    let task = dl.list().into_iter().find(|t| t.id == id);
    if let Some(t) = task {
        remove_task_files(&dl, &t);
        {
            let mut tasks = dl.tasks.lock().unwrap();
            tasks.retain(|x| x.id != id);
        }
        dl.persist();
        dl.emit_changed();
    }
    Ok(())
}

#[tauri::command]
pub async fn dl_remove_work(dl: State<'_, DownloadManager>, video_id: i64) -> Result<(), String> {
    let mine: Vec<DlTask> = dl.list().into_iter().filter(|t| t.video_id == video_id).collect();
    for t in &mine {
        dl.set_stop(&t.id, STOP_CANCEL);
        remove_task_files(&dl, t);
    }
    {
        let mut tasks = dl.tasks.lock().unwrap();
        tasks.retain(|x| x.video_id != video_id);
    }
    dl.persist();
    dl.emit_changed();
    Ok(())
}

#[tauri::command]
pub async fn dl_stats(dl: State<'_, DownloadManager>) -> Result<Value, String> {
    let tasks = dl.list();
    let done: Vec<&DlTask> = tasks.iter().filter(|t| t.status == ST_DONE).collect();
    Ok(json!({
        "total": tasks.len(),
        "done": done.len(),
        "active": tasks.iter().filter(|t| t.status == ST_DOWNLOADING || t.status == ST_QUEUED).count(),
        "failed": tasks.iter().filter(|t| t.status == ST_FAILED).count(),
        "bytes": done.iter().map(|t| t.size).sum::<u64>(),
        "root": dl.root().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
    }))
}

#[tauri::command]
pub async fn dl_open_dir(dl: State<'_, DownloadManager>) -> Result<(), String> {
    let root = dl.root().ok_or_else(|| "下载目录未就绪".to_string())?;
    let _ = std::fs::create_dir_all(&root);
    let path = root.to_string_lossy().to_string();
    open_path(&path)
}

#[tauri::command]
pub async fn dl_get_remux_pref(dl: State<'_, DownloadManager>) -> Result<bool, String> {
    Ok(dl.remux_pref())
}

#[tauri::command]
pub async fn dl_set_remux_pref(dl: State<'_, DownloadManager>, enabled: bool) -> Result<(), String> {
    dl.set_remux_pref(enabled);
    Ok(())
}

/// 手动把一集已下载的 TS 转成 MP4（无损换封装）。转换在阻塞线程池里跑，完成后返回最新任务。
#[tauri::command]
pub async fn dl_remux(app: AppHandle, dl: State<'_, DownloadManager>, id: String) -> Result<DlTask, String> {
    {
        let t = dl
            .list()
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| "任务不存在".to_string())?;
        if t.status != ST_DONE {
            return Err("只有已完成的下载可以转换".into());
        }
        if t.format.as_deref() != Some("ts") {
            return Err("该集不是 TS 格式，无需转换".into());
        }
    }
    let app2 = app.clone();
    let id2 = id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let dl = app2.state::<DownloadManager>();
        do_remux(&dl, &id2)
    })
    .await
    .map_err(|e| format!("转换任务异常：{e}"))??;
    dl.list()
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| "任务不存在".to_string())
}

#[cfg(target_os = "windows")]
fn open_path(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("explorer")
        .arg(path)
        .creation_flags(0x0800_0000)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
fn open_path(path: &str) -> Result<(), String> {
    std::process::Command::new("open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn open_path(path: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(target_os = "android")]
fn open_path(_path: &str) -> Result<(), String> {
    Err("安卓端下载在 App 内管理".to_string())
}

/* ───────────────────────── 本地供流（离线播放） ───────────────────────── */

fn local_respond(status: u16, ctype: &str, body: Vec<u8>, extra: &[(&str, String)]) -> Response<Vec<u8>> {
    let mut b = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header("Content-Type", ctype)
        .header("Access-Control-Allow-Origin", "*")
        .header("Accept-Ranges", "bytes");
    for (k, v) in extra {
        b = b.header(*k, v.clone());
    }
    b.body(body).unwrap_or_else(|_| Response::new(Vec::new()))
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|kv| kv.strip_prefix(&format!("{name}="))).map(|s| s.to_string())
}

/// `<LOCAL_ORIGIN>/local?v=&e=` → 单分片本地 HLS 列表（TS 走 hls.js 转封装播放）；
/// `<LOCAL_ORIGIN>/localfile?v=&e=` → 成品文件本体（支持 Range，mp4 直播/拖动用）。
pub async fn handle_local(
    mgr: &DownloadManager,
    path: &str,
    query: &str,
    origin: &str,
) -> Response<Vec<u8>> {
    let (Some(v), Some(e)) = (query_param(query, "v"), query_param(query, "e")) else {
        return local_respond(400, "text/plain", b"missing v/e".to_vec(), &[]);
    };
    let (Ok(video_id), Ok(ep)) = (v.parse::<i64>(), e.parse::<u32>()) else {
        return local_respond(400, "text/plain", b"bad v/e".to_vec(), &[]);
    };
    let Some((file, format, duration)) = mgr.done_file(video_id, ep) else {
        return local_respond(404, "text/plain", b"not downloaded".to_vec(), &[]);
    };

    match path {
        "/local" => {
            let dur = if duration > 0 { duration as f64 } else { 120.0 };
            let target = (dur.ceil() as u64).max(1);
            let body = format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:{dur:.3},\n{origin}/localfile?v={video_id}&e={ep}\n#EXT-X-ENDLIST\n"
            );
            local_respond(200, "application/vnd.apple.mpegurl", body.into_bytes(), &[("Cache-Control", "no-store".to_string())])
        }
        "/localfile" => {
            // Range 由协议层从请求头透传进来：handle_local 的调用方（lib.rs）把 Range 头拼进 query 的 r= 参数
            serve_file_range(&file, &format, query_param(query, "r").as_deref())
        }
        _ => local_respond(404, "text/plain", b"not found".to_vec(), &[]),
    }
}

/// 按 Range 读文件：`bytes=start-end` / `bytes=start-` / `bytes=-suffix`。
pub fn serve_file_range(file: &PathBuf, format: &str, range: Option<&str>) -> Response<Vec<u8>> {
    let ctype = if format == "mp4" { "video/mp4" } else { "video/mp2t" };
    let Ok(meta) = std::fs::metadata(file) else {
        return local_respond(404, "text/plain", b"gone".to_vec(), &[]);
    };
    let len = meta.len();
    let (start, end) = match range.and_then(|r| r.strip_prefix("bytes=")) {
        Some(spec) => {
            let spec = spec.split(',').next().unwrap_or("");
            let (a, b) = spec.split_once('-').unwrap_or(("", ""));
            if a.is_empty() {
                // 后缀段
                let suffix: u64 = b.parse().unwrap_or(0);
                if suffix == 0 || len == 0 {
                    (0, len.saturating_sub(1))
                } else {
                    (len.saturating_sub(suffix), len - 1)
                }
            } else {
                let s: u64 = a.parse().unwrap_or(0);
                let e: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse().unwrap_or(len - 1) };
                (s.min(len.saturating_sub(1)), e.min(len.saturating_sub(1)))
            }
        }
        None => (0, len.saturating_sub(1)),
    };
    if len == 0 || start > end {
        return local_respond(416, "text/plain", b"range not satisfiable".to_vec(), &[("Content-Range", format!("bytes */{len}"))]);
    }
    let want = (end - start + 1) as usize;
    let read = (|| -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(file)?;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; want];
        f.read_exact(&mut buf)?;
        Ok(buf)
    })();
    match read {
        Ok(buf) => {
            let status = if range.is_some() { 206 } else { 200 };
            local_respond(
                status,
                ctype,
                buf,
                &[
                    ("Content-Range", format!("bytes {start}-{end}/{len}")),
                    ("Content-Length", want.to_string()),
                ],
            )
        }
        Err(e) => local_respond(500, "text/plain", e.to_string().into_bytes(), &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_rules() {
        assert_eq!(file_stem("霸道总裁爱上我", 1, 24, ""), "霸道总裁爱上我 - 第01集");
        assert_eq!(file_stem("霸道总裁爱上我", 12, 120, ""), "霸道总裁爱上我 - 第012集");
        assert_eq!(file_stem("a/b:c*d", 3, 10, "第3集 初遇"), "a b c d - 第03集 初遇");
        assert_eq!(file_stem("剧名", 3, 10, "第03集"), "剧名 - 第03集");
        assert_eq!(sanitize_name("  a\u{0007}b  ", 80), "a b");
        assert_eq!(work_dir_name("a/b", 5), "a b");
    }

    /* 测试用加密端：AES-128-CBC + PKCS#7（与 decrypt_segment 对称） */
    fn encrypt_segment(key: &[u8], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        use aes::cipher::BlockEncrypt;
        let cipher = Aes128::new_from_slice(key).unwrap();
        let pad = 16 - (plain.len() % 16);
        let mut data = plain.to_vec();
        data.extend(std::iter::repeat(pad as u8).take(pad));
        let mut prev = *iv;
        let mut out = Vec::with_capacity(data.len());
        for chunk in data.chunks(16) {
            let mut block = [0u8; 16];
            for i in 0..16 {
                block[i] = chunk[i] ^ prev[i];
            }
            cipher.encrypt_block((&mut block).into());
            prev = block;
            out.extend_from_slice(&block);
        }
        out
    }

    #[test]
    fn decrypt_variants() {
        let key = b"0123456789abcdef";
        let plain: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        // 显式 IV
        let iv = [7u8; 16];
        let ct = encrypt_segment(key, &iv, &plain);
        assert_eq!(decrypt_segment(key, &iv, &ct).unwrap(), plain);
        // 序列号 IV（无 IV 属性时的规范默认）
        let iv2 = seq_iv(42);
        let ct2 = encrypt_segment(key, &iv2, &plain);
        assert_eq!(decrypt_segment(key, &iv2, &ct2).unwrap(), plain);
        // 恰好整块（pad=16 整块填充）
        let plain3: Vec<u8> = (0..188u32).map(|i| (i % 256) as u8).collect();
        let ct3 = encrypt_segment(key, &iv, &plain3);
        assert_eq!(decrypt_segment(key, &iv, &ct3).unwrap(), plain3);
    }

    #[test]
    fn range_serving() {
        let dir = std::env::temp_dir().join(format!("hghub_range_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("x.ts");
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        std::fs::write(&f, &data).unwrap();
        // 全量
        let r = serve_file_range(&f, "ts", None);
        assert_eq!(r.status(), 200);
        assert_eq!(r.body(), &data);
        // 区间
        let r = serve_file_range(&f, "ts", Some("bytes=100-199"));
        assert_eq!(r.status(), 206);
        assert_eq!(r.body(), &data[100..200]);
        assert_eq!(r.headers().get("Content-Range").unwrap().to_str().unwrap(), "bytes 100-199/1000");
        // 开放结尾
        let r = serve_file_range(&f, "mp4", Some("bytes=990-"));
        assert_eq!(r.body(), &data[990..]);
        // 后缀
        let r = serve_file_range(&f, "ts", Some("bytes=-10"));
        assert_eq!(r.body(), &data[990..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：本地 HTTP 服务器供一套 AES-128 加密 HLS（播放列表/密钥/分片），
    /// 走与 run_task 相同的 fetch_media_playlist → fetch_bytes → decrypt_segment 路径，
    /// 拼接结果与原始明文逐字节比对。再验 ffprobe 能解析拼接出的 TS。
    #[tokio::test]
    async fn local_hls_download_roundtrip() {
        use std::collections::HashMap as Map;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let key = b"fedcba9876543210";
        let iv = parse_iv("0x0000000000000000000000000000002a").unwrap();
        // 造 3 片带 TS 同步字节的明文
        let mut plains: Vec<Vec<u8>> = Vec::new();
        for s in 0..3 {
            let mut p = Vec::with_capacity(188 * 20);
            for pkt in 0..20 {
                p.push(0x47);
                for i in 1..188 {
                    p.push(((s * 31 + pkt * 7 + i) % 256) as u8);
                }
            }
            plains.push(p);
        }
        let expected: Vec<u8> = plains.concat();
        let mut files: Map<String, Vec<u8>> = Map::new();
        files.insert("/key.bin".into(), key.to_vec());
        for (i, p) in plains.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), encrypt_segment(key, &iv, p));
        }
        let m3u8 = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0000000000000000000000000000002a\n#EXTINF:5.0,\nseg0.ts\n#EXTINF:5.0,\nseg1.ts\n#EXTINF:5.0,\nseg2.ts\n#EXT-X-ENDLIST\n";
        files.insert("/list.m3u8".into(), m3u8.as_bytes().to_vec());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let files = Arc::new(files);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let files = files.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/").to_string();
                    let (status, body) = match files.get(&path) {
                        Some(b) => ("200 OK", b.clone()),
                        None => ("404 Not Found", b"nf".to_vec()),
                    };
                    let resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/list.m3u8");
        let (_u, pl) = fetch_media_playlist(&url).await.expect("fetch playlist");
        assert_eq!(pl.segs.len(), 3);
        let mut merged: Vec<u8> = Vec::new();
        let mut key_cache: Map<String, Vec<u8>> = Map::new();
        for seg in &pl.segs {
            let (_, raw) = fetch_bytes(&seg.url).await.expect("fetch seg");
            let k = seg.key.as_ref().expect("seg key");
            let uri = k.uri.clone().expect("key uri");
            if !key_cache.contains_key(&uri) {
                let (_, kb) = fetch_bytes(&uri).await.expect("fetch key");
                key_cache.insert(uri.clone(), kb);
            }
            let iv2 = k.iv.unwrap_or_else(|| seq_iv(seg.seq));
            let plain = decrypt_segment(key_cache.get(&uri).unwrap(), &iv2, &raw).expect("decrypt");
            merged.extend_from_slice(&plain);
        }
        server.abort();
        assert_eq!(merged, expected, "解密拼接结果与明文不一致");
    }

    /// 真实流校验：ffmpeg 现场生成一套加密 HLS（真 TS + PAT/PMT + H.264），
    /// 经本地服务器走下载管线解密拼接；期望值用 openssl 独立解密逐片拼接得到，
    /// 最后 ffprobe 确认成品是可解析的 mpegts。无 ffmpeg/openssl 时跳过。
    #[tokio::test]
    async fn ffmpeg_hls_roundtrip() {
        use std::collections::HashMap as Map;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let has_tools = std::process::Command::new("ffmpeg").arg("-version").output().is_ok()
            && std::process::Command::new("openssl").arg("version").output().is_ok();
        if !has_tools {
            eprintln!("skip: ffmpeg/openssl not available");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hghub_real_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key: &[u8] = b"fedcba9876543210";
        let key_hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let iv_hex = "0000000000000000000000000000002a";
        std::fs::write(dir.join("key.bin"), key).unwrap();
        std::fs::write(
            dir.join("keyinfo"),
            format!("key.bin\n{}\n{iv_hex}\n", dir.join("key.bin").to_string_lossy()),
        )
        .unwrap();
        let gen = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=2:size=176x144:rate=10",
                   "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "10",
                   "-hls_time", "1", "-hls_segment_filename"])
            .arg(dir.join("seg%d.ts"))
            .arg("-hls_key_info_file")
            .arg(dir.join("keyinfo"))
            .arg(dir.join("out.m3u8"))
            .output()
            .expect("run ffmpeg");
        assert!(gen.status.success(), "ffmpeg 生成失败: {}", String::from_utf8_lossy(&gen.stderr));

        // 把生成目录搬上本地服务器
        let playlist_text = std::fs::read_to_string(dir.join("out.m3u8")).unwrap();
        assert!(playlist_text.contains("#EXT-X-KEY"), "ffmpeg 列表应带 KEY: {playlist_text}");
        let seg_names: Vec<String> = playlist_text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect();
        assert!(seg_names.len() >= 2, "至少 2 片: {seg_names:?}");
        let mut files: Map<String, Vec<u8>> = Map::new();
        files.insert("/out.m3u8".into(), playlist_text.as_bytes().to_vec());
        files.insert("/key.bin".into(), key.to_vec());
        for n in &seg_names {
            files.insert(format!("/{n}"), std::fs::read(dir.join(n)).unwrap());
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let files = Arc::new(files);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let files = files.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/").to_string();
                    let (status, body) = match files.get(&path) {
                        Some(b) => ("200 OK", b.clone()),
                        None => ("404 Not Found", b"nf".to_vec()),
                    };
                    let resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/out.m3u8");
        let (_u, pl) = fetch_media_playlist(&url).await.expect("fetch real playlist");
        assert_eq!(pl.segs.len(), seg_names.len());
        let mut merged: Vec<u8> = Vec::new();
        for seg in &pl.segs {
            let (_, raw) = fetch_bytes(&seg.url).await.expect("fetch real seg");
            let k = seg.key.as_ref().expect("real seg key");
            let uri = k.uri.clone().expect("real key uri");
            let (_, kb) = fetch_bytes(&uri).await.expect("fetch real key");
            let iv = k.iv.unwrap_or_else(|| seq_iv(seg.seq));
            merged.extend_from_slice(&decrypt_segment(&kb, &iv, &raw).expect("decrypt real seg"));
        }
        server.abort();

        // 期望值：openssl 独立逐片解密拼接
        let mut expected: Vec<u8> = Vec::new();
        for n in &seg_names {
            let out = std::process::Command::new("openssl")
                .args(["enc", "-d", "-aes-128-cbc", "-K", &key_hex, "-iv", iv_hex, "-in"])
                .arg(dir.join(n))
                .output()
                .expect("run openssl");
            assert!(out.status.success(), "openssl 解密失败");
            expected.extend_from_slice(&out.stdout);
        }
        assert_eq!(merged, expected, "与 openssl 独立解密结果不一致");
        assert_eq!(merged[0], 0x47, "TS 同步字节");

        // ffprobe 可解析
        let tmp = dir.join("merged.ts");
        std::fs::write(&tmp, &merged).unwrap();
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=format_name", "-of", "csv=p=0"])
            .arg(&tmp)
            .output()
            .expect("run ffprobe");
        let s = String::from_utf8_lossy(&probe.stdout).to_string();
        assert!(s.contains("mpegts"), "ffprobe 不认成品: {s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlist_and_decrypt_roundtrip() {
        // 用 openssl 造的固定样本在 hls_roundtrip 里验；这里先验纯解析
        let m3u8 = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00000000000000000000000000000009\n#EXTINF:5.5,\nseg0.ts\n#EXTINF:5.5,\nhttps://cdn.example.com/x/seg1.ts?a=1\n#EXT-X-ENDLIST\n";
        let pl = parse_playlist(m3u8, "https://cdn.example.com/x/list.m3u8");
        assert_eq!(pl.segs.len(), 2);
        assert_eq!(pl.segs[0].url, "https://cdn.example.com/x/seg0.ts");
        assert_eq!(pl.segs[0].seq, 7);
        assert_eq!(pl.segs[1].seq, 8);
        let k = pl.segs[0].key.as_ref().unwrap();
        assert_eq!(k.uri.as_deref(), Some("https://cdn.example.com/x/key.bin"));
        assert_eq!(k.iv.unwrap()[15], 9);

        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=720x1280\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2500000\nhi.m3u8\n";
        let mp = parse_playlist(master, "https://cdn.example.com/master.m3u8");
        assert_eq!(mp.variants.len(), 2);
        assert!(mp.segs.is_empty());
    }
}
