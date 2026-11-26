//! Запросы к Google: обмен кода, обновление токена, отзыв, список встреч.
//!
//! Тонкий слой над reqwest: всё, что решается без сети, живёт рядом и
//! тестируется (`oauth::parse_token_response`, `events_query`,
//! `classify_status`).

use super::keys::Keys;
use super::oauth::{parse_token_response, TokenError, Tokens};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use std::time::Duration;

pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";
pub const CALENDARS_BASE: &str = "https://www.googleapis.com/calendar/v3/calendars";
pub const CALENDAR_LIST_ENDPOINT: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList";

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    Network(String),
    /// 401 — access token протух раньше срока; обновить и повторить.
    Unauthorized,
    Http(u16),
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// `timeMin` у Google — нижняя граница по КОНЦУ встречи, поэтому минус 15
/// минут хватает, чтобы идущая встреча осталась в списке.
pub fn events_query(now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    vec![
        ("timeMin", (now - TimeDelta::minutes(15)).to_rfc3339_opts(SecondsFormat::Secs, true)),
        ("timeMax", (now + TimeDelta::hours(24)).to_rfc3339_opts(SecondsFormat::Secs, true)),
        ("singleEvents", "true".to_string()),
        ("orderBy", "startTime".to_string()),
        ("maxResults", "100".to_string()),
    ]
}

pub fn classify_status(status: u16) -> Option<FetchError> {
    match status {
        200..=299 => None,
        401 => Some(FetchError::Unauthorized),
        s => Some(FetchError::Http(s)),
    }
}

async fn post_token(client: &reqwest::Client, form: &[(&str, &str)]) -> Result<Tokens, TokenError> {
    let resp = client
        .post(TOKEN_ENDPOINT)
        .form(form)
        .send()
        .await
        .map_err(|e| TokenError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| TokenError::Network(e.to_string()))?;
    parse_token_response(status, &body)
}

/// client_secret в документации Google для desktop помечен «Optional», но
/// клиент типа Desktop app без него на практике получает `invalid_client` —
/// поэтому шлём.
pub async fn exchange_code(
    client: &reqwest::Client,
    keys: Keys,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens, TokenError> {
    post_token(
        client,
        &[
            ("client_id", keys.client_id),
            ("client_secret", keys.client_secret),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ],
    )
    .await
}

pub async fn refresh(client: &reqwest::Client, keys: Keys, refresh_token: &str) -> Result<Tokens, TokenError> {
    post_token(
        client,
        &[
            ("client_id", keys.client_id),
            ("client_secret", keys.client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )
    .await
}

/// Отзыв при «Отключить». Ошибку не показываем: локально токен уже стёрт,
/// а недоотозванный в Google ключ без нашего хранилища бесполезен.
pub async fn revoke(client: &reqwest::Client, token: &str) {
    if let Err(e) = client.post(REVOKE_ENDPOINT).form(&[("token", token)]).send().await {
        log::warn!("календарь: отзыв токена не дошёл до Google: {e}");
    }
}

/// Адрес событий календаря; id кодируется (в нём бывают `@`, `#`, `/`).
pub fn events_url(calendar_id: &str) -> String {
    let mut url = url::Url::parse(CALENDARS_BASE).expect("базовый адрес календарей");
    if let Ok(mut seg) = url.path_segments_mut() {
        seg.push(calendar_id).push("events");
    }
    url.to_string()
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CalendarListPage {
    items: Vec<CalendarEntry>,
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CalendarEntry {
    id: String,
    deleted: bool,
    hidden: bool,
    access_role: Option<String>,
}

/// Календари, события которых читаем: не удалённые, не скрытые, с доступом
/// не ниже «читатель» (`freeBusyReader` видит только занятость, без названий
/// и ссылок).
fn readable_ids(items: Vec<CalendarEntry>) -> Vec<String> {
    items
        .into_iter()
        .filter(|c| {
            !c.deleted
                && !c.hidden
                && !c.id.is_empty()
                && matches!(
                    c.access_role.as_deref(),
                    Some("reader" | "writerWithoutPrivateAccess" | "writer" | "owner")
                )
        })
        .map(|c| c.id)
        .collect()
}

/// Разбор одной страницы `calendarList`: (id календарей, токен следующей страницы).
fn parse_calendar_list(body: &str) -> Result<(Vec<String>, Option<String>), String> {
    let page: CalendarListPage =
        serde_json::from_str(body).map_err(|e| format!("список календарей не разобран: {e}"))?;
    Ok((readable_ids(page.items), page.next_page_token.filter(|t| !t.is_empty())))
}

async fn get_text(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    query: &[(&str, String)],
) -> Result<String, FetchError> {
    let resp = client
        .get(url)
        .bearer_auth(access_token)
        .query(query)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if let Some(e) = classify_status(resp.status().as_u16()) {
        return Err(e);
    }
    resp.text().await.map_err(|e| FetchError::Network(e.to_string()))
}

/// Все календари аккаунта (в том числе расшаренные), доступные для чтения.
/// Ошибка (например 403 у токена без scope `calendar.calendarlist.readonly`)
/// уходит вызывающему — он откатывается на `primary`.
pub async fn list_calendar_ids(client: &reqwest::Client, access_token: &str) -> Result<Vec<String>, FetchError> {
    let mut ids = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..20 {
        let mut q = vec![("minAccessRole", "reader".to_string()), ("maxResults", "250".to_string())];
        if let Some(t) = &page_token {
            q.push(("pageToken", t.clone()));
        }
        let body = get_text(client, CALENDAR_LIST_ENDPOINT, access_token, &q).await?;
        let (mut page_ids, next) =
            parse_calendar_list(&body).map_err(|e| FetchError::Network(e))?;
        ids.append(&mut page_ids);
        match next {
            Some(t) => page_token = Some(t),
            None => break,
        }
    }
    Ok(ids)
}

/// Ответы `events.list` по каждому календарю. 401 прерывает всё (токен
/// обновят и повторят); любая другая ошибка одного календаря — лог и пропуск.
/// Если список календарей не получен — только `primary`.
pub async fn list_events(
    client: &reqwest::Client,
    access_token: &str,
    now: DateTime<Utc>,
) -> Result<Vec<String>, FetchError> {
    let ids = match list_calendar_ids(client, access_token).await {
        Ok(ids) if !ids.is_empty() => ids,
        Ok(_) => {
            log::warn!("календарь: список календарей пуст, читаю только primary");
            vec!["primary".to_string()]
        }
        Err(FetchError::Unauthorized) => return Err(FetchError::Unauthorized),
        Err(e) => {
            log::warn!("календарь: список календарей не получен ({e:?}), читаю только primary");
            vec!["primary".to_string()]
        }
    };
    let query = events_query(now);
    let mut bodies = Vec::new();
    let mut last_err = None;
    for id in &ids {
        match get_text(client, &events_url(id), access_token, &query).await {
            Ok(b) => bodies.push(b),
            Err(FetchError::Unauthorized) => return Err(FetchError::Unauthorized),
            Err(e) => {
                log::warn!("календарь: календарь пропущен, события не загрузились: {e:?}");
                last_err = Some(e);
            }
        }
    }
    // Не загрузился ни один — это сбой, а не «встреч нет».
    match (bodies.is_empty(), last_err) {
        (true, Some(e)) => Err(e),
        _ => Ok(bodies),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn окно_запроса_минус_15_минут_плюс_сутки() {
        let now = DateTime::parse_from_rfc3339("2026-10-11T12:00:00Z").unwrap().with_timezone(&Utc);
        let q = events_query(now);
        let get = |k: &str| q.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("timeMin"), "2026-10-11T11:45:00Z");
        assert_eq!(get("timeMax"), "2026-10-12T12:00:00Z");
        assert_eq!(get("singleEvents"), "true");
        assert_eq!(get("orderBy"), "startTime");
    }

    #[test]
    fn статусы_ответа_календаря() {
        assert_eq!(classify_status(200), None);
        assert_eq!(classify_status(401), Some(FetchError::Unauthorized));
        assert_eq!(classify_status(403), Some(FetchError::Http(403)));
        assert_eq!(classify_status(503), Some(FetchError::Http(503)));
    }

    #[test]
    fn адрес_событий_кодирует_id_календаря() {
        assert_eq!(
            events_url("primary"),
            "https://www.googleapis.com/calendar/v3/calendars/primary/events"
        );
        let u = events_url("c_abc#holiday@group.v.calendar.google.com");
        assert!(u.contains("%23holiday"), "{u}");
        assert!(u.ends_with("/events"), "{u}");
    }

    #[test]
    fn список_календарей_фильтруется_по_роли_скрытым_и_удалённым() {
        let body = r#"{"nextPageToken":"p2","items":[
            {"id":"a","accessRole":"owner"},
            {"id":"b","accessRole":"reader"},
            {"id":"c","accessRole":"writer"},
            {"id":"d","accessRole":"writerWithoutPrivateAccess"},
            {"id":"e","accessRole":"freeBusyReader"},
            {"id":"f","accessRole":"owner","hidden":true},
            {"id":"g","accessRole":"owner","deleted":true},
            {"id":"h"}
        ]}"#;
        let (ids, next) = parse_calendar_list(body).unwrap();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        assert_eq!(next.as_deref(), Some("p2"));
        assert_eq!(parse_calendar_list(r#"{"items":[]}"#).unwrap().1, None);
    }
}
