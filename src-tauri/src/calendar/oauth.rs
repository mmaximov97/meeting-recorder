//! Вход в Google для десктопа: системный браузер, редирект на
//! `http://127.0.0.1:<порт>`, PKCE S256 и `state` (RFC 7636, RFC 8252).
//!
//! Всё здесь работает без сети: адрес входа собирается строкой, ответ
//! браузера принимает свой маленький HTTP-слушатель на `std::net`, ответ
//! token endpoint разбирается из текста. Сами запросы к Google — в
//! `google.rs`.
//!
//! `access_denied` здесь — это `Callback::Denied`, и отличить по нему «нажал
//! Отмена у Google» от «у MeetRec кончились места» нельзя: отдельного кода у
//! Google нет (developers.google.com/identity/protocols/oauth2/native-app,
//! раздел ошибок; проверено 11.10.2026).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar.events.readonly";
/// `calendar.calendarlist.readonly` — список календарей (`calendarList.list`),
/// чтобы читать события всех календарей, а не только `primary`. Необязателен:
/// без него (старый токен, снятая галочка) читаем только `primary`.
pub const SCOPES: &str = "openid email https://www.googleapis.com/auth/calendar.events.readonly https://www.googleapis.com/auth/calendar.calendarlist.readonly";

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = random_token();
        let challenge = challenge_for(&verifier);
        Self { verifier, challenge }
    }
}

/// S256: base64url без padding от SHA-256 verifier'а.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 32 случайных байта в base64url — 43 символа из алфавита, который RFC 7636
/// разрешает для verifier'а. Тем же способом делается `state`.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn new_state() -> String {
    random_token()
}

/// `prompt=consent` — чтобы Google выдал refresh token и при повторном входе
/// (после «отключился»), а не только при первом.
pub fn auth_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    url::Url::parse_with_params(
        AUTH_ENDPOINT,
        &[
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )
    .expect("адрес входа Google собирается всегда")
    .to_string()
}

#[derive(PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// `error=access_denied`. См. докблок модуля.
    Denied,
    Failed(String),
}

/// Код авторизации в логи не попадает.
impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Callback::Code(_) => f.write_str("Code(\"***\")"),
            Callback::Denied => f.write_str("Denied"),
            Callback::Failed(e) => f.debug_tuple("Failed").field(e).finish(),
        }
    }
}

const ERR_STATE: &str = "state в ответе не совпал";
const ERR_NO_CODE: &str = "в ответе нет кода";

/// Разобрать `GET /?code=…&state=…` из редиректа. Чужой `state` — сбой, даже
/// если код есть: это защита от подложенного ответа.
pub fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Failed("непонятный адрес ответа".into());
    };
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Failed(ERR_STATE.into());
    }
    match (error.as_deref(), code) {
        (Some("access_denied"), _) => Callback::Denied,
        (Some(e), _) => Callback::Failed(e.to_string()),
        (None, Some(c)) if !c.is_empty() => Callback::Code(c),
        _ => Callback::Failed(ERR_NO_CODE.into()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    Got(Callback),
    Cancelled,
    TimedOut,
}

/// Порт выбирает система (`:0`), слушаем только loopback.
pub fn bind_loopback() -> std::io::Result<(TcpListener, String)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    Ok((listener, format!("http://127.0.0.1:{port}")))
}

/// Ждать редирект из браузера. Блокирующая — звать из `spawn_blocking`.
///
/// Неблокирующий `accept` с опросом раз в 100 мс, а не блокирующий: иначе
/// «Отмена» в настройках не смогла бы прервать ожидание. Запросы не к `/`
/// (браузер просит `/favicon.ico`) получают 404 и ожидание не прерывают.
pub fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    cancel: &AtomicBool,
    timeout: Duration,
    page_ok: &str,
    page_fail: &str,
) -> Wait {
    if let Err(e) = listener.set_nonblocking(true) {
        return Wait::Got(Callback::Failed(format!("слушатель входа: {e}")));
    }
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Wait::Cancelled;
        }
        if started.elapsed() > timeout {
            return Wait::TimedOut;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(cb) = serve_one(stream, expected_state, page_ok, page_fail) {
                    return Wait::Got(cb);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Wait::Got(Callback::Failed(format!("слушатель входа: {e}"))),
        }
    }
}

/// Один запрос. `None` — запрос не к `/` или ответ без верного state/кода, ждём дальше.
fn serve_one(mut stream: TcpStream, expected_state: &str, page_ok: &str, page_fail: &str) -> Option<Callback> {
    // На macOS принятый сокет наследует неблокирующий режим слушателя.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&buf[..len]);
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    if target != "/" && !target.starts_with("/?") {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    }
    let cb = parse_callback(target, expected_state);
    let text = if matches!(cb, Callback::Code(_)) { page_ok } else { page_fail };
    // Чужой state или пустой ответ — не конец ожидания: иначе любая
    // посторонняя страница на loopback сорвала бы вход.
    let keep_waiting = matches!(&cb, Callback::Failed(m) if m == ERR_STATE || m == ERR_NO_CODE);
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>MeetRec</title>\
         <p style=\"font:16px -apple-system,system-ui,sans-serif;margin:40px\">{text}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if keep_waiting {
        None
    } else {
        Some(cb)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub expires_in: i64,
    /// Есть в ответе на обмен кода; в ответе на refresh обычно нет.
    pub refresh_token: Option<String>,
    /// Из `id_token` — только при обмене кода (scope `openid email`).
    pub email: Option<String>,
    /// Выданные доступы (пробелы между ними). Google отдаёт их при обмене кода.
    pub scope: Option<String>,
}

impl Tokens {
    /// Не отказал ли человек в доступе к календарю (нет поля scope — принимаем): на экране согласия Google позволяет
    /// снять отдельные галочки.
    pub fn has_calendar_scope(&self) -> bool {
        self.scope
            .as_deref()
            .is_none_or(|s| s.split_whitespace().any(|x| x == CALENDAR_SCOPE))
    }
}

/// Токены в логи не попадают; почта — не секрет.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"***")
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("email", &self.email)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Refresh token отозван или протух — человеку надо войти заново.
    InvalidGrant,
    Network(String),
    /// Связка ключей не отдала токен (доступ запрещён и т. п.), записи не «нет».
    Keychain(String),
    Other(String),
}

#[derive(Deserialize)]
struct TokenOk {
    access_token: String,
    #[serde(default)]
    expires_in: i64,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenErr {
    error: String,
}

pub fn parse_token_response(status: u16, body: &str) -> Result<Tokens, TokenError> {
    if (200..300).contains(&status) {
        let t: TokenOk = serde_json::from_str(body)
            .map_err(|e| TokenError::Other(format!("ответ Google не разобран: {e}")))?;
        return Ok(Tokens {
            email: t.id_token.as_deref().and_then(email_from_id_token),
            access_token: t.access_token,
            expires_in: if t.expires_in > 0 { t.expires_in } else { 3600 },
            refresh_token: t.refresh_token,
            scope: t.scope,
        });
    }
    match serde_json::from_str::<TokenErr>(body) {
        Ok(e) if e.error == "invalid_grant" => Err(TokenError::InvalidGrant),
        Ok(e) => Err(TokenError::Other(format!("Google ответил {status}: {}", e.error))),
        Err(_) => Err(TokenError::Other(format!("Google ответил {status}"))),
    }
}

/// Почта из `id_token` без проверки подписи. Это допустимо: токен пришёл
/// напрямую от token endpoint Google по HTTPS (developers.google.com/identity/
/// openid-connect/openid-connect, «Validating an ID token»), и почта нужна
/// только для показа в настройках.
pub fn email_from_id_token(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("email")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::Ordering;

    /// RFC 7636, приложение B.
    #[test]
    fn challenge_совпадает_с_вектором_из_rfc_7636() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_43_символа_из_разрешённого_алфавита_и_каждый_раз_новый() {
        let a = Pkce::new();
        let b = Pkce::new();
        assert_eq!(a.verifier.len(), 43);
        assert!(a
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')));
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(a.challenge, challenge_for(&a.verifier));
    }

    #[test]
    fn state_непустой_и_каждый_раз_новый() {
        let a = new_state();
        assert!(a.len() >= 32);
        assert_ne!(a, new_state());
    }

    #[test]
    fn адрес_входа_несёт_pkce_state_и_узкий_scope() {
        let u = url::Url::parse(&auth_url("cid", "http://127.0.0.1:5000", "chal", "st")).unwrap();
        assert_eq!(u.host_str(), Some("accounts.google.com"));
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "st");
        assert_eq!(q["scope"], SCOPES);
    }

    #[test]
    fn ответ_с_кодом() {
        assert_eq!(
            parse_callback("/?state=s1&code=4%2Fabc&scope=email", "s1"),
            Callback::Code("4/abc".into())
        );
    }

    #[test]
    fn отказ_access_denied() {
        assert_eq!(parse_callback("/?error=access_denied&state=s1", "s1"), Callback::Denied);
    }

    #[test]
    fn другая_ошибка_google() {
        assert_eq!(
            parse_callback("/?error=admin_policy_enforced&state=s1", "s1"),
            Callback::Failed("admin_policy_enforced".into())
        );
    }

    #[test]
    fn чужой_state_не_принимается_даже_с_кодом() {
        assert!(matches!(parse_callback("/?state=XX&code=abc", "s1"), Callback::Failed(_)));
    }

    #[test]
    fn без_кода_и_без_ошибки_это_сбой() {
        assert!(matches!(parse_callback("/?state=s1", "s1"), Callback::Failed(_)));
    }

    fn запрос(addr: String, путь: &'static str) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET {путь} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        })
    }

    #[test]
    fn петля_отдаёт_код_и_страницу_для_браузера() {
        let (l, redirect) = bind_loopback().unwrap();
        assert!(redirect.starts_with("http://127.0.0.1:"));
        let addr = redirect.trim_start_matches("http://").to_string();
        let ответ = запрос(addr, "/?state=s1&code=abc");
        let cancel = AtomicBool::new(false);
        let got = wait_for_callback(l, "s1", &cancel, Duration::from_secs(5), "готово", "не вышло");
        assert_eq!(got, Wait::Got(Callback::Code("abc".into())));
        let страница = ответ.join().unwrap();
        assert!(страница.starts_with("HTTP/1.1 200"));
        assert!(страница.contains("готово"));
    }

    #[test]
    fn favicon_не_сбивает_ожидание() {
        let (l, redirect) = bind_loopback().unwrap();
        let addr = redirect.trim_start_matches("http://").to_string();
        let a2 = addr.clone();
        let первый = запрос(addr, "/favicon.ico");
        let cancel = AtomicBool::new(false);
        let второй = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            запрос(a2, "/?state=s1&error=access_denied").join().unwrap()
        });
        let got = wait_for_callback(l, "s1", &cancel, Duration::from_secs(5), "готово", "не вышло");
        assert_eq!(got, Wait::Got(Callback::Denied));
        assert!(первый.join().unwrap().starts_with("HTTP/1.1 404"));
        assert!(второй.join().unwrap().contains("не вышло"));
    }

    #[test]
    fn чужой_state_не_завершает_ожидание() {
        let (l, redirect) = bind_loopback().unwrap();
        let addr = redirect.trim_start_matches("http://").to_string();
        let a2 = addr.clone();
        let чужой = запрос(addr, "/?state=XX&code=evil");
        let cancel = AtomicBool::new(false);
        let верный = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            запрос(a2, "/?state=s1&code=good").join().unwrap()
        });
        let got = wait_for_callback(l, "s1", &cancel, Duration::from_secs(5), "готово", "не вышло");
        assert_eq!(got, Wait::Got(Callback::Code("good".into())));
        assert!(чужой.join().unwrap().contains("не вышло"));
        assert!(верный.join().unwrap().contains("готово"));
    }

    #[test]
    fn debug_не_показывает_секреты() {
        let c = format!("{:?}", Callback::Code("SECRETCODE".into()));
        assert!(!c.contains("SECRETCODE"));
        let t = parse_token_response(200, include_str!("fixtures/token_ok.json")).unwrap();
        let d = format!("{t:?}");
        for секрет in ["ya29.test-access", "1//test-refresh", "eyJ"] {
            assert!(!d.contains(секрет), "{секрет} в {d}");
        }
    }

    #[test]
    fn отмена_прерывает_ожидание() {
        let (l, _) = bind_loopback().unwrap();
        let cancel = AtomicBool::new(true);
        let got = wait_for_callback(l, "s1", &cancel, Duration::from_secs(5), "", "");
        assert_eq!(got, Wait::Cancelled);
        cancel.store(false, Ordering::SeqCst);
    }

    #[test]
    fn без_ответа_ожидание_кончается_по_таймауту() {
        let (l, _) = bind_loopback().unwrap();
        let cancel = AtomicBool::new(false);
        let got = wait_for_callback(l, "s1", &cancel, Duration::from_millis(250), "", "");
        assert_eq!(got, Wait::TimedOut);
    }

    #[test]
    fn календарный_scope_виден_в_ответе() {
        let t = parse_token_response(200, include_str!("fixtures/token_ok.json")).unwrap();
        assert!(t.has_calendar_scope());
    }

    #[test]
    fn без_календарного_scope_доступа_нет() {
        let t = parse_token_response(200, include_str!("fixtures/token_no_calendar.json")).unwrap();
        assert!(!t.has_calendar_scope());
        let t = parse_token_response(200, r#"{"access_token":"a"}"#).unwrap();
        assert!(t.has_calendar_scope(), "нет поля scope — вход принимаем");
    }

    #[test]
    fn ответ_token_endpoint_с_refresh_и_почтой() {
        let t = parse_token_response(200, include_str!("fixtures/token_ok.json")).unwrap();
        assert_eq!(t.access_token, "ya29.test-access");
        assert_eq!(t.expires_in, 3599);
        assert_eq!(t.refresh_token.as_deref(), Some("1//test-refresh"));
        assert_eq!(t.email.as_deref(), Some("name@gmail.com"));
    }

    #[test]
    fn отозванный_токен_это_invalid_grant() {
        assert_eq!(
            parse_token_response(400, include_str!("fixtures/token_invalid_grant.json")),
            Err(TokenError::InvalidGrant)
        );
    }

    #[test]
    fn другие_ошибки_token_endpoint_не_invalid_grant() {
        assert!(matches!(
            parse_token_response(401, r#"{"error":"invalid_client"}"#),
            Err(TokenError::Other(_))
        ));
        assert!(matches!(parse_token_response(503, "<html>"), Err(TokenError::Other(_))));
    }

    #[test]
    fn refresh_без_нового_refresh_token_это_норма() {
        let t = parse_token_response(200, r#"{"access_token":"a","expires_in":3599}"#).unwrap();
        assert_eq!(t.refresh_token, None);
        assert_eq!(t.email, None);
    }

    #[test]
    fn почта_из_id_token_и_мусор_не_роняет() {
        assert_eq!(email_from_id_token("a.b.c"), None);
        assert_eq!(email_from_id_token("не-jwt"), None);
    }
}
