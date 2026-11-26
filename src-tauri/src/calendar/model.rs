//! Встреча из Google Календаря: разбор ответа `events.list`, ссылка и источник.
//!
//! Что отбрасывается (описание фичи, «Какие встречи считаются»): встречи на
//! весь день (`start.date` без `dateTime`), отклонённые (`attendees[self].
//! responseStatus == declined`), отменённые и встречи без ссылки Zoom, Google
//! Meet или Teams. Порядок — как прислал API (`orderBy=startTime`): по нему
//! планировщик решает, про какую из двух встреч в одно время спрашивать.

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Zoom,
    Meet,
    Teams,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meeting {
    pub id: String,
    /// Может быть пустым — подпись «Без названия» подставляет интерфейс.
    pub title: String,
    pub start: DateTime<Utc>,
    /// Если у события нет конца со временем, равен `start`.
    pub end: DateTime<Utc>,
    pub url: String,
    pub source: Source,
}

/// Источник по домену. Только `https`: ссылку отдаём системе (`open`), и
/// открывать по ней что-то, кроме браузера или звонилки, незачем.
pub fn source_of(link: &str) -> Option<Source> {
    let url = url::Url::parse(link).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "zoom.us" || host.ends_with(".zoom.us") {
        Some(Source::Zoom)
    } else if host == "meet.google.com" {
        Some(Source::Meet)
    } else if host == "teams.microsoft.com" || host == "teams.live.com" {
        Some(Source::Teams)
    } else {
        None
    }
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r#"(?i)https://(?:[a-z0-9-]+\.)*zoom\.us/j/[^\s<>"'\])]+"#,
            r#"|https://meet\.google\.com/[a-z]{3}-[a-z]{4}-[a-z]{3}"#,
            r#"|https://teams\.microsoft\.com/l/meetup-join/[^\s<>"'\])]+"#,
            r#"|https://teams\.live\.com/meet/[^\s<>"'\])]+"#,
        ))
        .expect("регулярка ссылок на встречи")
    })
}

/// Первая ссылка на встречу в свободном тексте (место, описание).
///
/// Описание бывает HTML: `&amp;` внутри ссылки раскрывается, а точка или
/// запятая в конце предложения к ссылке не относится.
pub fn link_in_text(text: &str) -> Option<String> {
    let found = link_re().find(text)?.as_str();
    let trimmed = found.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    Some(trimmed.replace("&amp;", "&"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Page {
    items: Vec<RawEvent>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawEvent {
    id: String,
    #[serde(rename = "iCalUID")]
    ical_uid: Option<String>,
    status: Option<String>,
    summary: Option<String>,
    start: RawTime,
    end: RawTime,
    hangout_link: Option<String>,
    conference_data: Option<Conference>,
    location: Option<String>,
    description: Option<String>,
    attendees: Vec<Attendee>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawTime {
    date_time: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Conference {
    entry_points: Vec<EntryPoint>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct EntryPoint {
    entry_point_type: Option<String>,
    uri: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Attendee {
    #[serde(rename = "self")]
    is_self: bool,
    response_status: Option<String>,
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

/// Ссылка в порядке надёжности: `hangoutLink` → видео из `conferenceData` →
/// место → описание. Кандидат без известного источника пропускается: видео
/// Webex не повод отказаться от запасной ссылки Zoom в описании.
fn link_of(ev: &RawEvent) -> Option<(String, Source)> {
    let video = ev.conference_data.as_ref().and_then(|c| {
        c.entry_points
            .iter()
            .find(|p| p.entry_point_type.as_deref() == Some("video"))
            .and_then(|p| p.uri.clone())
    });
    let candidates = [
        ev.hangout_link.clone(),
        video,
        ev.location.as_deref().and_then(link_in_text),
        ev.description.as_deref().and_then(link_in_text),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(|l| source_of(&l).map(|s| (normalized(&l).unwrap_or(l), s)))
}

/// Ссылка в разобранном виде: системе уходит ровно то, что проверили.
fn normalized(link: &str) -> Option<String> {
    url::Url::parse(link).ok().map(|u| u.as_str().to_string())
}

fn meeting_of(ev: RawEvent) -> Option<Meeting> {
    if ev.status.as_deref() == Some("cancelled") {
        return None;
    }
    // Весь день: у такого события `start.date`, а `dateTime` нет.
    let start = parse_time(ev.start.date_time.as_deref()?)?;
    let end = ev.end.date_time.as_deref().and_then(parse_time).unwrap_or(start);
    let declined = ev
        .attendees
        .iter()
        .any(|a| a.is_self && a.response_status.as_deref() == Some("declined"));
    if declined {
        return None;
    }
    let (url, source) = link_of(&ev)?;
    Some(Meeting {
        id: ev.id,
        title: ev.summary.unwrap_or_default().trim().to_string(),
        start,
        end,
        url,
        source,
    })
}

/// Ключ склейки одной встречи из разных календарей: iCalUID (или id, если его
/// нет) и время начала — у повторяющейся встречи iCalUID общий для всех вхождений.
fn dedup_key(ev: &RawEvent, start: DateTime<Utc>) -> String {
    let uid = ev.ical_uid.as_deref().filter(|u| !u.is_empty()).unwrap_or(&ev.id);
    format!("{uid}|{}", start.timestamp())
}

fn parse_keyed(json: &str) -> Result<Vec<(String, Meeting)>, String> {
    let page: Page =
        serde_json::from_str(json).map_err(|e| format!("ответ календаря не разобран: {e}"))?;
    Ok(page
        .items
        .into_iter()
        .filter_map(|ev| {
            let start = parse_time(ev.start.date_time.as_deref()?)?;
            let key = dedup_key(&ev, start);
            meeting_of(ev).map(|m| (key, m))
        })
        .collect())
}

#[cfg(test)]
pub fn parse_events(json: &str) -> Result<Vec<Meeting>, String> {
    Ok(parse_keyed(json)?.into_iter().map(|(_, m)| m).collect())
}

/// Ответы `events.list` нескольких календарей → одна лента: без дублей (первый
/// календарь выигрывает), по времени начала. Ответ, который не разобрался,
/// пропускается; ошибка — только если не разобрался ни один.
pub fn merge_events(bodies: &[String]) -> Result<Vec<Meeting>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut all = Vec::new();
    let mut first_err = None;
    let mut parsed_any = bodies.is_empty();
    for body in bodies {
        match parse_keyed(body) {
            Ok(items) => {
                parsed_any = true;
                for (key, m) in items {
                    if seen.insert(key) {
                        all.push(m);
                    }
                }
            }
            Err(e) => {
                log::warn!("календарь: {e}");
                first_err.get_or_insert(e);
            }
        }
    }
    if !parsed_any {
        return Err(first_err.unwrap_or_default());
    }
    all.sort_by_key(|m| m.start); // стабильная: при равном старте порядок календарей сохраняется
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/events.json");

    fn встреча(id: &str) -> Meeting {
        parse_events(FIXTURE)
            .unwrap()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("нет встречи {id}"))
    }

    fn событие(id: &str, uid: &str, start: &str) -> String {
        format!(
            r#"{{"id":"{id}","iCalUID":"{uid}","summary":"s","hangoutLink":"https://meet.google.com/abc-defg-hij","start":{{"dateTime":"{start}"}},"end":{{"dateTime":"{start}"}}}}"#
        )
    }

    #[test]
    fn одна_встреча_в_двух_календарях_склеивается_и_лента_по_времени() {
        let a = format!(
            r#"{{"items":[{},{}]}}"#,
            событие("x1", "uid-1", "2026-10-11T13:00:00Z"),
            событие("x2", "uid-2", "2026-10-11T15:00:00Z")
        );
        // тот же uid-1 и то же время (другой id), плюс uid-3 раньше всех
        // и uid-1 в другое время (повтор серии) — не дубль
        let b = format!(
            r#"{{"items":[{},{},{}]}}"#,
            событие("y3", "uid-3", "2026-10-11T12:00:00Z"),
            событие("y1", "uid-1", "2026-10-11T13:00:00Z"),
            событие("y1b", "uid-1", "2026-10-12T13:00:00Z")
        );
        let ids: Vec<String> = merge_events(&[a, b]).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["y3", "x1", "x2", "y1b"]);
    }

    #[test]
    fn без_ical_uid_ключ_это_id_и_старт_а_битый_ответ_не_валит_остальные() {
        let a = r#"{"items":[{"id":"e","summary":"s","hangoutLink":"https://meet.google.com/abc-defg-hij","start":{"dateTime":"2026-10-11T12:00:00Z"}}]}"#.to_string();
        let ids = merge_events(&[a.clone(), a, "не json".to_string()]).unwrap();
        assert_eq!(ids.len(), 1);
        assert!(merge_events(&["не json".to_string()]).is_err());
    }

    #[test]
    fn системе_уходит_разобранная_ссылка() {
        assert_eq!(normalized("HTTPS://Zoom.US/j/1 ").as_deref(), Some("https://zoom.us/j/1"));
    }

    #[test]
    fn источник_по_домену_ссылки() {
        assert_eq!(source_of("https://zoom.us/j/1"), Some(Source::Zoom));
        assert_eq!(source_of("https://us02web.zoom.us/j/1?pwd=a"), Some(Source::Zoom));
        assert_eq!(source_of("https://meet.google.com/abc-defg-hij"), Some(Source::Meet));
        assert_eq!(source_of("https://teams.microsoft.com/l/meetup-join/x"), Some(Source::Teams));
        assert_eq!(source_of("https://teams.live.com/meet/9"), Some(Source::Teams));
    }

    #[test]
    fn чужой_домен_и_не_https_не_источник() {
        assert_eq!(source_of("https://acme.webex.com/meet/x"), None);
        assert_eq!(source_of("https://notzoom.us/j/1"), None);
        assert_eq!(source_of("http://zoom.us/j/1"), None);
        assert_eq!(source_of("не ссылка"), None);
    }

    #[test]
    fn ссылка_в_тексте_без_хвостовой_пунктуации_и_с_раскрытым_amp() {
        assert_eq!(
            link_in_text("Zoom: https://zoom.us/j/999?pwd=x&amp;uname=y, комната 3").as_deref(),
            Some("https://zoom.us/j/999?pwd=x&uname=y")
        );
        assert_eq!(
            link_in_text("Встреча: https://meet.google.com/abc-defg-hij.").as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
        assert_eq!(
            link_in_text("<a href=\"https://teams.live.com/meet/9381\">тут</a>").as_deref(),
            Some("https://teams.live.com/meet/9381")
        );
        assert_eq!(link_in_text("Кафе на углу"), None);
    }

    #[test]
    fn из_фикстуры_остаются_только_встречи_со_ссылкой_в_порядке_api() {
        let ids: Vec<String> = parse_events(FIXTURE).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["zoom-conf", "meet-hangout", "teams-desc", "zoom-location", "webex-then-zoom"]);
    }

    #[test]
    fn ссылка_из_conference_data_видео_а_не_телефон() {
        let m = встреча("zoom-conf");
        assert_eq!(m.url, "https://us02web.zoom.us/j/81234567890?pwd=abc");
        assert_eq!(m.source, Source::Zoom);
        assert_eq!(m.start.to_rfc3339(), "2026-10-11T12:00:00+00:00");
    }

    #[test]
    fn hangout_link_это_meet() {
        let m = встреча("meet-hangout");
        assert_eq!(m.url, "https://meet.google.com/abc-defg-hij");
        assert_eq!(m.source, Source::Meet);
    }

    #[test]
    fn ссылка_teams_из_html_описания() {
        let m = встреча("teams-desc");
        assert_eq!(
            m.url,
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_X%40thread.v2/0?context=%7b%7d"
        );
        assert_eq!(m.source, Source::Teams);
    }

    #[test]
    fn пустое_название_остаётся_пустым_а_ссылка_из_места() {
        let m = встреча("zoom-location");
        assert_eq!(m.title, "");
        assert_eq!(m.url, "https://zoom.us/j/999?pwd=x&uname=y");
    }

    #[test]
    fn чужое_видео_пропускается_и_берётся_ссылка_из_описания() {
        let m = встреча("webex-then-zoom");
        assert_eq!(m.url, "https://acme.zoom.us/j/555");
        assert_eq!(m.source, Source::Zoom);
    }

    #[test]
    fn источник_сериализуется_строчными() {
        assert_eq!(serde_json::to_string(&Source::Meet).unwrap(), "\"meet\"");
    }

    #[test]
    fn битый_json_это_ошибка_а_не_пустой_список() {
        assert!(parse_events("{ не json").is_err());
    }
}
