//! SMTP → Cloudflare Email Sending REST API 프록시.
//!
//! 내부 서비스가 127.0.0.1:2525(또는 10.0.50.122:2525)에 SMTP로 메일을 보내면,
//! 이 프록시가 수신 → RFC822 파싱 → CF Email Sending API(JSON POST)로 변환.
//!
//! 필수 환경변수:
//!   CF_ACCOUNT_ID          Cloudflare account id
//!   CLOUDFLARE_EMAIL       auth email (X-Auth-Email)
//!   CLOUDFLARE_API_KEY     Global API key (X-Auth-Key)
//!
//! 선택 환경변수:
//!   PROXY_HOST             바인딩 IP (기본 0.0.0.0)
//!   PROXY_PORT             SMTP 포트 (기본 2525)
//!   DEFAULT_FROM           미허용 FROM rewrite 타겟 (기본 devops@prelik.com)
//!   ALLOWED_DOMAINS        쉼표 구분 (기본 prelik.com,ranode.net,internal.kr)

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use mail_parser::MessageParser;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

#[derive(Clone, Debug)]
struct Config {
    account_id: String,
    auth_email: String,
    auth_key: String,
    default_from: String,
    allowed_domains: HashSet<String>,
    listen: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        let host = std::env::var("PROXY_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let port = std::env::var("PROXY_PORT").unwrap_or_else(|_| "2525".into());
        let allowed = std::env::var("ALLOWED_DOMAINS")
            .unwrap_or_else(|_| "prelik.com,ranode.net,internal.kr".into());
        Ok(Self {
            account_id: std::env::var("CF_ACCOUNT_ID").context("CF_ACCOUNT_ID 환경변수 필요")?,
            auth_email: std::env::var("CLOUDFLARE_EMAIL")
                .context("CLOUDFLARE_EMAIL 환경변수 필요")?,
            auth_key: std::env::var("CLOUDFLARE_API_KEY")
                .context("CLOUDFLARE_API_KEY 환경변수 필요")?,
            default_from: std::env::var("DEFAULT_FROM")
                .unwrap_or_else(|_| "devops@prelik.com".into()),
            allowed_domains: allowed
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            listen: format!("{host}:{port}"),
        })
    }
}

#[derive(Deserialize, Default, Debug)]
struct CfResult {
    delivered: Option<Vec<String>>,
    queued: Option<Vec<String>>,
    permanent_bounces: Option<Vec<String>>,
}

#[derive(Deserialize, Debug)]
struct CfResponse {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: Option<CfResult>,
}

#[derive(Deserialize, Debug)]
struct CfError {
    code: i64,
    message: String,
}

// ==================== 헤더 디코딩 ====================

static ENCODED_WORD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"=\?([A-Za-z0-9_.\-]+)\?([BbQq])\?([^?\s]*?)\?=").unwrap());
static ADDR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<?([^<>\s]+@[^<>\s]+)>?").unwrap());
static MAIL_CMD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:MAIL\s+FROM|RCPT\s+TO):\s*<?([^>\s]*)>?").unwrap());

fn decode_encoded_word(charset: &str, encoding: &str, data: &str) -> String {
    let bytes: Vec<u8> = if encoding.eq_ignore_ascii_case("B") {
        B64.decode(data)
            .unwrap_or_else(|_| data.as_bytes().to_vec())
    } else {
        let q = data.replace('_', " ");
        quoted_printable::decode(q.as_bytes(), quoted_printable::ParseMode::Robust)
            .unwrap_or_else(|_| q.into_bytes())
    };
    decode_bytes(&bytes, charset)
}

fn decode_bytes(bytes: &[u8], charset: &str) -> String {
    if charset.eq_ignore_ascii_case("iso-8859-1") || charset.eq_ignore_ascii_case("latin1") {
        return bytes.iter().map(|&b| b as char).collect();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// MIME encoded-word (RFC 2047) 혼합 헤더를 유니코드로.
///
/// 표준 라이브러리는 혼합된 평문 비ASCII를 백슬래시 이스케이프로 바꿔버리는 이슈가 있어,
/// 정규식으로 encoded-word만 치환하는 방식으로 우회한다.
fn decode_header(raw: &str) -> String {
    ENCODED_WORD_RE
        .replace_all(raw, |caps: &regex::Captures| {
            decode_encoded_word(&caps[1], &caps[2], &caps[3])
        })
        .into_owned()
}

/// FROM 헤더 도메인이 허용되지 않으면 DEFAULT_FROM으로 rewrite.
fn rewrite_from(from_hdr: &str, cfg: &Config) -> String {
    if cfg.allowed_domains.is_empty() {
        return from_hdr.to_string();
    }
    let email = ADDR_RE
        .captures(from_hdr)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or(from_hdr);
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("")
        .to_ascii_lowercase();
    if cfg.allowed_domains.contains(&domain) {
        from_hdr.to_string()
    } else {
        info!(
            "FROM {} rewritten → {} (domain not allowed)",
            email, cfg.default_from
        );
        cfg.default_from.clone()
    }
}

fn extract_addr(line: &str) -> String {
    MAIL_CMD_RE
        .captures(line)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

// ==================== CF API ====================

#[allow(clippy::too_many_arguments)]
async fn send_via_cf(
    client: &reqwest::Client,
    cfg: &Config,
    from: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    reply_to: Option<&str>,
) -> Result<CfResult> {
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/email/sending/send",
        cfg.account_id
    );

    let mut payload = serde_json::Map::new();
    payload.insert("from".into(), from.into());
    payload.insert("to".into(), to.into());
    payload.insert("subject".into(), subject.into());
    if let Some(t) = text {
        payload.insert("text".into(), t.into());
    }
    if let Some(h) = html {
        payload.insert("html".into(), h.into());
    }
    if let Some(r) = reply_to {
        payload.insert("reply_to".into(), r.into());
    }

    let resp = client
        .post(&url)
        .header("X-Auth-Email", &cfg.auth_email)
        .header("X-Auth-Key", &cfg.auth_key)
        .json(&serde_json::Value::Object(payload))
        .send()
        .await
        .context("CF API 호출 실패")?;

    let status = resp.status();
    let body: CfResponse = resp.json().await.context("CF 응답 JSON 파싱 실패")?;

    if !body.success {
        let msg = body
            .errors
            .iter()
            .map(|e| format!("{}:{}", e.code, e.message))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("CF HTTP {} rejected: {}", status, msg);
    }

    let result = body.result.unwrap_or_default();
    if let Some(b) = &result.permanent_bounces {
        if !b.is_empty() {
            bail!("CF bounced: {:?}", b);
        }
    }
    Ok(result)
}

// ==================== SMTP 세션 ====================

async fn handle_session(
    cfg: Arc<Config>,
    client: reqwest::Client,
    stream: TcpStream,
) -> Result<()> {
    let peer = stream.peer_addr()?;
    info!(%peer, "session start");

    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);

    write_line(&mut writer, "220 cf-mail-proxy ESMTP ready").await?;

    let mut envelope_from = String::new();
    let mut envelope_rcpts: Vec<String> = vec![];
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf).await?;
        if n == 0 {
            return Ok(());
        }
        let line = line_buf.trim_end_matches(['\r', '\n']);
        let upper = line.to_ascii_uppercase();

        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            write_line(&mut writer, "250-cf-mail-proxy").await?;
            write_line(&mut writer, "250-8BITMIME").await?;
            write_line(&mut writer, "250-SMTPUTF8").await?;
            write_line(&mut writer, "250 PIPELINING").await?;
        } else if upper.starts_with("MAIL FROM:") {
            envelope_from = extract_addr(line);
            write_line(&mut writer, "250 OK").await?;
        } else if upper.starts_with("RCPT TO:") {
            let addr = extract_addr(line);
            if !addr.is_empty() {
                envelope_rcpts.push(addr);
            }
            write_line(&mut writer, "250 OK").await?;
        } else if upper == "DATA" {
            write_line(&mut writer, "354 End data with <CRLF>.<CRLF>").await?;
            let data = read_data(&mut reader).await?;
            let res = process_data(&cfg, &client, &envelope_from, &envelope_rcpts, &data).await;
            match res {
                Ok(_) => write_line(&mut writer, "250 Accepted via CF Email Sending").await?,
                Err(e) => {
                    error!(%e, "send failed");
                    let safe = e.to_string().replace(['\r', '\n'], " ");
                    let truncated = if safe.len() > 400 {
                        &safe[..400]
                    } else {
                        &safe
                    };
                    write_line(&mut writer, &format!("554 {}", truncated)).await?;
                }
            }
            envelope_from.clear();
            envelope_rcpts.clear();
        } else if upper == "RSET" {
            envelope_from.clear();
            envelope_rcpts.clear();
            write_line(&mut writer, "250 OK").await?;
        } else if upper == "NOOP" {
            write_line(&mut writer, "250 OK").await?;
        } else if upper == "QUIT" {
            write_line(&mut writer, "221 Bye").await?;
            break;
        } else if upper.starts_with("STARTTLS") {
            write_line(&mut writer, "502 STARTTLS not supported").await?;
        } else if upper.starts_with("VRFY") || upper.starts_with("EXPN") {
            write_line(&mut writer, "252 Cannot VRFY user").await?;
        } else {
            write_line(&mut writer, "500 Unknown command").await?;
        }
    }
    Ok(())
}

async fn write_line(
    writer: &mut BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    line: &str,
) -> Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn read_data(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "." {
            break;
        }
        // Leading-dot un-stuffing (RFC 5321)
        let payload = if line.starts_with("..") {
            &line[1..]
        } else {
            &line[..]
        };
        data.extend_from_slice(payload.as_bytes());
    }
    Ok(data)
}

async fn process_data(
    cfg: &Config,
    client: &reqwest::Client,
    envelope_from: &str,
    envelope_rcpts: &[String],
    raw: &[u8],
) -> Result<()> {
    if envelope_rcpts.is_empty() {
        bail!("no recipients");
    }

    let parser = MessageParser::default();
    let msg = parser.parse(raw).context("RFC822 파싱 실패")?;

    let subject = msg
        .subject()
        .map(decode_header)
        .unwrap_or_else(|| "(no subject)".into());

    let from_hdr = msg
        .from()
        .and_then(|addr| addr.first())
        .map(|a| {
            let email = a.address().unwrap_or("").to_string();
            match a.name() {
                Some(name) => {
                    let decoded = decode_header(name);
                    format!("\"{}\" <{}>", decoded, email)
                }
                None => email,
            }
        })
        .unwrap_or_else(|| envelope_from.to_string());

    let text = msg.body_text(0).map(|s| s.into_owned());
    let html = msg.body_html(0).map(|s| s.into_owned());
    let reply_to = msg
        .reply_to()
        .and_then(|addr| addr.first())
        .and_then(|a| a.address())
        .map(|s| s.to_string());

    let from_final = rewrite_from(&from_hdr, cfg);

    for rcpt in envelope_rcpts {
        info!("→ CF send to={} subj={}", rcpt, subject);
        let r = send_via_cf(
            client,
            cfg,
            &from_final,
            rcpt,
            &subject,
            text.as_deref(),
            html.as_deref(),
            reply_to.as_deref(),
        )
        .await?;
        info!(
            "← delivered={:?} queued={:?} bounced={:?}",
            r.delivered.as_deref().unwrap_or(&[]),
            r.queued.as_deref().unwrap_or(&[]),
            r.permanent_bounces.as_deref().unwrap_or(&[])
        );
    }
    Ok(())
}

// ==================== main ====================

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(Config::from_env()?);
    info!(
        "cf-mail-proxy listening on {} (CF account={})",
        cfg.listen, cfg.account_id
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let listener = TcpListener::bind(&cfg.listen).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let cfg = cfg.clone();
        let client = client.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_session(cfg, client, stream).await {
                warn!(%e, "session ended with error");
            }
        });
    }
}

// ==================== 단위 테스트 ====================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> Config {
        Config {
            account_id: "test".into(),
            auth_email: "test@example.com".into(),
            auth_key: "key".into(),
            default_from: "devops@prelik.com".into(),
            allowed_domains: ["prelik.com", "ranode.net", "internal.kr"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            listen: "0.0.0.0:2525".into(),
        }
    }

    // ----- header decoding -----

    #[test]
    fn plain_ascii() {
        assert_eq!(decode_header("Hello"), "Hello");
    }

    #[test]
    fn plain_korean() {
        assert_eq!(decode_header("한국어 제목"), "한국어 제목");
    }

    #[test]
    fn empty() {
        assert_eq!(decode_header(""), "");
    }

    #[test]
    fn utf8_b_encoded() {
        assert_eq!(
            decode_header("=?utf-8?B?7ZWc6rWt7Ja0IOygnOuqqQ==?="),
            "한국어 제목"
        );
    }

    #[test]
    fn utf8_q_encoded() {
        assert_eq!(
            decode_header("=?utf-8?Q?=ED=95=9C=EA=B5=AD=EC=96=B4?="),
            "한국어"
        );
    }

    #[test]
    fn mixed_encoded_and_plain_regression() {
        // 원래 버그: Laravel이 보낸 혼합 헤더가 `=?utf-8?Q?...?= 템플릿]` 형태로 옴
        let raw = "=?utf-8?Q?=F0=9F=8E=9F=EF=B8=8F_=5B=EA=B0=95=EC=9D=98=EC=8B=A4_?= 템플릿]";
        let out = decode_header(raw);
        assert!(out.contains("🎟️"), "out={out}");
        assert!(out.contains("[강의실"), "out={out}");
        assert!(out.contains("템플릿]"), "out={out}");
        assert!(!out.contains("=?"), "남은 encoded-word 토큰: out={out}");
    }

    #[test]
    fn emoji_only() {
        assert_eq!(decode_header("=?utf-8?B?8J+Onw==?="), "🎟");
    }

    #[test]
    fn name_with_email_address() {
        let encoded = "=?utf-8?B?7ZWY7Jik?= <noreply@prelik.com>";
        let out = decode_header(encoded);
        assert!(out.contains("하오"), "out={out}");
        assert!(out.contains("<noreply@prelik.com>"), "out={out}");
    }

    // ----- address extraction -----

    #[test]
    fn extract_mail_from() {
        assert_eq!(extract_addr("MAIL FROM:<a@b.com>"), "a@b.com");
        assert_eq!(
            extract_addr("MAIL FROM:<a@b.com> SIZE=123 BODY=8BITMIME"),
            "a@b.com"
        );
    }

    #[test]
    fn extract_rcpt_to() {
        assert_eq!(extract_addr("RCPT TO:<x@y.com>"), "x@y.com");
    }

    #[test]
    fn extract_unbracketed() {
        assert_eq!(extract_addr("MAIL FROM: a@b.com"), "a@b.com");
    }

    // ----- FROM rewrite -----

    #[test]
    fn rewrite_allowed_passes() {
        let cfg = test_cfg();
        assert_eq!(rewrite_from("devops@prelik.com", &cfg), "devops@prelik.com");
    }

    #[test]
    fn rewrite_allowed_with_name() {
        let cfg = test_cfg();
        let hdr = "\"Hi.Events\" <noreply@prelik.com>";
        assert_eq!(rewrite_from(hdr, &cfg), hdr);
    }

    #[test]
    fn rewrite_blocked() {
        let cfg = test_cfg();
        assert_eq!(
            rewrite_from("root@openclaw.internal", &cfg),
            "devops@prelik.com"
        );
    }

    #[test]
    fn rewrite_internal_kr() {
        let cfg = test_cfg();
        assert_eq!(
            rewrite_from("devops@internal.kr", &cfg),
            "devops@internal.kr"
        );
    }

    #[test]
    fn rewrite_subdomain_blocked() {
        let cfg = test_cfg();
        assert_eq!(
            rewrite_from("user@mail.prelik.com", &cfg),
            "devops@prelik.com"
        );
    }

    // ----- body extraction (integration via mail-parser) -----

    #[test]
    fn parse_multipart_korean() {
        let raw = concat!(
            "From: a@prelik.com\r\n",
            "To: b@naver.com\r\n",
            "Subject: =?utf-8?B?7ZWc6rWt7Ja0?=\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=\"X\"\r\n",
            "\r\n",
            "--X\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "안녕하세요\r\n",
            "--X\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<p>안녕하세요</p>\r\n",
            "--X--\r\n",
        );
        let msg = MessageParser::default().parse(raw.as_bytes()).unwrap();
        let subj = decode_header(msg.subject().unwrap_or(""));
        assert_eq!(subj, "한국어");
        let text = msg.body_text(0).unwrap_or_default().into_owned();
        assert!(text.contains("안녕하세요"));
        let html = msg.body_html(0).unwrap_or_default().into_owned();
        assert!(html.contains("안녕하세요"));
    }
}
