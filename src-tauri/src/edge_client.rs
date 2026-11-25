//! dx150-edge-v1 加密通道（scope=user）。
//!
//! 站点 2026-11 起停用旧明文信封 `/api/edge/request`，且直连 REST 不再接受
//! `Authorization: Bearer <token>`：登录态接口（登录/注册/me/收藏/点赞/关注/
//! 评论写入）只能走这条加密通道，token 放在加密包内由边缘校验。
//! 协议逆自官网 `edge-api-crypto.js` / `edge-cache-runtime.js`：
//!   1. hello：客户端生成 ECDH P-256 临时密钥对，POST {base}/api/user/request
//!      `{v:1,type:"hello",client_pub:base64url(未压缩公钥)}`，服务端回
//!      kid/sid/site/server_pub/salt/server_time/expires_at（会话 ≤600 秒）。
//!   2. 派生：shared = ECDH(服务端公钥)；HKDF-SHA256(salt=hello.salt,
//!      info="dx150-edge-v1\nuser\n{site}\n{kid}\n{sid}" + "\nreq" / "\nres")
//!      得到请求/响应两把 AES-256-GCM 密钥。
//!   3. call：明文 JSON `{"request":{"method","path","body":base64(原始正文),
//!      "content_type"}, "authorization":"Bearer <token>"}`（无 token 时省略），
//!      nonce = 12 字节、仅末 4 字节为序号（大端，从 1 递增），
//!      AAD = "{info}\nreq\n{seq}\n{ts}"，ts 为按 hello 时差校正后的秒级时间戳；
//!      外层 POST `{v:1,type:"call",kid,sid,seq,ts,ciphertext:base64url}`。
//!      响应同结构，方向词换 "res"，解密得 `{"status","headers","body":base64}`。
//! 已用一次性测试账号实测全通（注册→me→收藏列表→收藏写入→列表复核）。
//! 注意：站点把 token 与出口 IP 绑定——出口 IP 每次请求都变的网络环境
//! （如助理沙盒的轮换出口）下 token 永远校验失败，与本实现无关；
//! 用户在家里是固定宽带 IP，不受影响，IP 变更后重新登录即可（.7 起有过期提示）。

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use once_cell::sync::Lazy;
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::rand_core::OsRng;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::PublicKey;
use serde_json::{json, Value};
use sha2::Sha256;

const PROTOCOL: &str = "dx150-edge-v1";
const SCOPE: &str = "user";
const MAX_SEQUENCE: u32 = 16384;

struct Session {
    kid: String,
    sid: String,
    info: String,
    req_key: [u8; 32],
    res_key: [u8; 32],
    clock_offset: i64,
    expires_at_local: i64,
    seq: u32,
}

static SESSION: Lazy<tokio::sync::Mutex<Option<Session>>> =
    Lazy::new(|| tokio::sync::Mutex::new(None));

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn endpoint() -> String {
    format!("{}/api/{}/request", crate::net::base(), SCOPE)
}

async fn post_json(body: &Value) -> Result<Value, String> {
    let cur = crate::net::base();
    let resp = crate::net::client()
        .post(endpoint())
        .header("Accept", "application/json")
        .header("Referer", format!("{cur}/"))
        .json(body)
        .send()
        .await
        .map_err(|e| crate::net::err_text(&e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| crate::net::err_text(&e))?;
    if !status.is_success() {
        // 边缘自身的错误（如 {"error":"session_expired"}）原样带出，由调用方识别。
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                return Err(format!("edge:{err}"));
            }
        }
        return Err(format!("加密通道 HTTP {status}"));
    }
    serde_json::from_str::<Value>(&text).map_err(|e| format!("加密通道响应解析失败: {e}"))
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    B64URL
        .decode(s)
        .map_err(|e| format!("加密通道 base64 解码失败: {e}"))
}

async fn hello() -> Result<Session, String> {
    let secret = EphemeralSecret::random(&mut OsRng);
    let pub_bytes = PublicKey::from(&secret)
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    let resp = post_json(&json!({
        "v": 1,
        "type": "hello",
        "client_pub": B64URL.encode(&pub_bytes),
    }))
    .await?;
    if resp.get("type").and_then(|t| t.as_str()) != Some("hello") {
        return Err("加密握手响应无效".to_string());
    }
    let get_str = |k: &str| -> Result<String, String> {
        resp.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("加密握手缺少 {k}"))
    };
    let kid = get_str("kid")?;
    let sid = get_str("sid")?;
    let site = get_str("site")?;
    let server_pub_raw = b64url_decode(&get_str("server_pub")?)?;
    let server_pub = PublicKey::from_sec1_bytes(&server_pub_raw)
        .map_err(|_| "加密握手服务端公钥无效".to_string())?;
    let salt = b64url_decode(&get_str("salt")?)?;
    if salt.len() != 32 {
        return Err("加密握手盐长度无效".to_string());
    }
    let server_time = resp
        .get("server_time")
        .and_then(|v| v.as_i64())
        .ok_or("加密握手缺少 server_time")?;
    let expires_at = resp
        .get("expires_at")
        .and_then(|v| v.as_i64())
        .ok_or("加密握手缺少 expires_at")?;
    if expires_at <= server_time || expires_at - server_time > 600 {
        return Err("加密握手有效期无效".to_string());
    }
    let shared = secret.diffie_hellman(&server_pub);
    let info = format!("{PROTOCOL}\n{SCOPE}\n{site}\n{kid}\n{sid}");
    let hk = hkdf::Hkdf::<Sha256>::new(Some(&salt), shared.raw_secret_bytes().as_slice());
    let mut req_key = [0u8; 32];
    let mut res_key = [0u8; 32];
    hk.expand(format!("{info}\nreq").as_bytes(), &mut req_key)
        .map_err(|_| "加密密钥派生失败".to_string())?;
    hk.expand(format!("{info}\nres").as_bytes(), &mut res_key)
        .map_err(|_| "加密密钥派生失败".to_string())?;
    let now = unix_now();
    Ok(Session {
        kid,
        sid,
        info,
        req_key,
        res_key,
        clock_offset: server_time - now,
        expires_at_local: now + (expires_at - server_time),
        seq: 0,
    })
}

enum CallError {
    /// 会话过期/失效：丢弃会话后可安全重试一次。
    SessionExpired,
    Fatal(String),
}

async fn call_once(
    method: &str,
    path: &str,
    body_bytes: &[u8],
    content_type: &str,
    token: Option<&str>,
) -> Result<(u16, String), CallError> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};

    let mut guard = SESSION.lock().await;
    let stale = match &*guard {
        None => true,
        Some(s) => unix_now() + 5 >= s.expires_at_local || s.seq >= MAX_SEQUENCE,
    };
    if stale {
        *guard = Some(hello().await.map_err(CallError::Fatal)?);
    }
    let s = guard.as_mut().expect("session just set");
    s.seq += 1;
    let seq = s.seq;
    let ts = unix_now() + s.clock_offset;

    let mut packet = json!({
        "request": {
            "method": method,
            "path": path,
            "body": B64.encode(body_bytes),
            "content_type": content_type,
        }
    });
    if let Some(t) = token {
        if !t.is_empty() {
            packet["authorization"] = json!(format!("Bearer {t}"));
        }
    }
    let plaintext = serde_json::to_vec(&packet).map_err(|e| CallError::Fatal(e.to_string()))?;

    let mut iv = [0u8; 12];
    iv[8..12].copy_from_slice(&seq.to_be_bytes());
    let nonce = Nonce::from_slice(&iv);
    let aad_req = format!("{}\nreq\n{}\n{}", s.info, seq, ts);
    let ciphertext = Aes256Gcm::new_from_slice(&s.req_key)
        .map_err(|_| CallError::Fatal("加密密钥无效".to_string()))?
        .encrypt(
            nonce,
            Payload {
                msg: &plaintext,
                aad: aad_req.as_bytes(),
            },
        )
        .map_err(|_| CallError::Fatal("加密失败".to_string()))?;

    let resp = post_json(&json!({
        "v": 1,
        "type": "call",
        "kid": s.kid,
        "sid": s.sid,
        "seq": seq,
        "ts": ts,
        "ciphertext": B64URL.encode(&ciphertext),
    }))
    .await
    .map_err(|e| {
        if e.contains("session_expired") {
            CallError::SessionExpired
        } else {
            CallError::Fatal(e)
        }
    })?;

    if resp.get("type").and_then(|t| t.as_str()) != Some("result")
        || resp.get("sid").and_then(|v| v.as_str()) != Some(s.sid.as_str())
        || resp.get("seq").and_then(|v| v.as_u64()) != Some(seq as u64)
    {
        return Err(CallError::Fatal("加密接口响应不匹配".to_string()));
    }
    let resp_ct = b64url_decode(
        resp.get("ciphertext")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CallError::Fatal("加密接口响应缺少密文".to_string()))?,
    )
    .map_err(CallError::Fatal)?;
    let aad_res = format!("{}\nres\n{}\n{}", s.info, seq, ts);
    let plain = Aes256Gcm::new_from_slice(&s.res_key)
        .map_err(|_| CallError::Fatal("解密密钥无效".to_string()))?
        .decrypt(
            nonce,
            Payload {
                msg: &resp_ct,
                aad: aad_res.as_bytes(),
            },
        )
        .map_err(|_| CallError::Fatal("响应解密失败".to_string()))?;
    let inner: Value = serde_json::from_slice(&plain)
        .map_err(|e| CallError::Fatal(format!("加密接口响应格式无效: {e}")))?;
    let status = inner
        .get("status")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| CallError::Fatal("加密接口响应缺少 status".to_string()))? as u16;
    let raw = inner
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let bytes = if raw.is_empty() {
        Vec::new()
    } else {
        B64.decode(raw)
            .map_err(|e| CallError::Fatal(format!("响应正文解码失败: {e}")))?
    };
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// 走加密通道发一个用户类接口请求，返回内层 HTTP 状态与正文文本。
/// 会话过期时自动重新握手并重试一次；其余错误直接上抛。
pub async fn user_call(
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> Result<(u16, String), String> {
    let body_bytes = match &body {
        Some(v) => serde_json::to_vec(v).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let content_type = if body.is_some() {
        "application/json"
    } else {
        ""
    };
    for attempt in 0..2 {
        match call_once(method, path, &body_bytes, content_type, token).await {
            Ok(r) => return Ok(r),
            Err(CallError::SessionExpired) if attempt == 0 => {
                *SESSION.lock().await = None;
            }
            Err(CallError::SessionExpired) => {
                return Err("加密会话已过期，重试后仍失败".to_string());
            }
            Err(CallError::Fatal(e)) => return Err(e),
        }
    }
    unreachable!()
}
