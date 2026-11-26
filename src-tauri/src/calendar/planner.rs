//! Что показать прямо сейчас: всплывашку про какую встречу, спрятать её,
//! какие встречи поставить в меню иконки, чем ответить детектору.
//!
//! Чистые функции: время, встречи, память об отказах и состояние записи
//! приходят параметрами. Решения о времени — только здесь.

use super::model::{Meeting, Source};
use crate::audio::UiState;
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use std::collections::HashSet;

pub const POPUP_LEAD_SECS: i64 = 60;
pub const POPUP_TAIL_SECS: i64 = 120;
pub const MENU_LEAD_SECS: i64 = 5 * 60;
pub const MENU_TAIL_SECS: i64 = 10 * 60;
pub const DETECT_LEAD_SECS: i64 = 5 * 60;

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    /// Ключ — пара (id, начало): перенесённая встреча — это новая встреча,
    /// про неё спрашиваем заново.
    pub shown: HashSet<(String, DateTime<Utc>)>,
    pub refused: HashSet<(String, DateTime<Utc>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Join,
    Record,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnScreen {
    pub id: String,
    /// Начало встречи, как оно было, когда плашку показали.
    pub start: DateTime<Utc>,
    pub mode: Mode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PopupAction {
    Stay,
    Show(String),
    Hide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: String,
    pub title: String,
    pub start: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub popup: PopupAction,
    pub menu: Vec<MenuEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Armed {
    Classic,
    Calendar(String),
    Silent,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PopupPayload {
    pub id: String,
    pub title: String,
    pub source: Source,
    pub start_ms: i64,
    pub tail_ms: i64,
    pub mode: Mode,
}

fn secs(s: i64) -> TimeDelta {
    TimeDelta::seconds(s)
}

fn key(m: &Meeting) -> (String, DateTime<Utc>) {
    (m.id.clone(), m.start)
}

/// `at` лежит в [from - lead, from + tail]. Переполнение времени — окно не активно.
fn within(at: DateTime<Utc>, from: DateTime<Utc>, lead: i64, tail: i64) -> bool {
    let lo = from.checked_sub_signed(secs(lead));
    let hi = from.checked_add_signed(secs(tail));
    matches!((lo, hi), (Some(lo), Some(hi)) if at >= lo && at <= hi)
}

impl Memory {
    /// Пары (id, начало), которых нет в ответе, — встреча кончилась
    /// (`timeMin` режет по концу), её удалили или перенесли. Помнить
    /// про них больше незачем.
    pub fn retain_known(&mut self, meetings: &[Meeting]) {
        let known: HashSet<(&str, DateTime<Utc>)> = meetings.iter().map(|m| (m.id.as_str(), m.start)).collect();
        self.shown.retain(|(id, st)| known.contains(&(id.as_str(), *st)));
        self.refused.retain(|(id, st)| known.contains(&(id.as_str(), *st)));
    }
}

pub fn payload(m: &Meeting, mode: Mode) -> PopupPayload {
    PopupPayload {
        id: m.id.clone(),
        title: m.title.clone(),
        source: m.source,
        start_ms: m.start.timestamp_millis(),
        tail_ms: m.start.checked_add_signed(secs(POPUP_TAIL_SECS)).unwrap_or(m.start).timestamp_millis(),
        mode,
    }
}

fn in_popup_window(m: &Meeting, now: DateTime<Utc>) -> bool {
    within(now, m.start, POPUP_LEAD_SECS, POPUP_TAIL_SECS)
}

/// Первая по порядку API встреча, про которую ещё не спрашивали. Две
/// встречи с одним началом — один вопрос: если про соседку уже спросили,
/// про эту молчим.
fn pick_popup<'a>(meetings: &'a [Meeting], now: DateTime<Utc>, mem: &Memory) -> Option<&'a Meeting> {
    meetings.iter().find(|m| {
        in_popup_window(m, now)
            && !mem.shown.contains(&key(m))
            && !mem.refused.contains(&key(m))
            && !meetings
                .iter()
                .any(|o| o.id != m.id && o.start == m.start && mem.shown.contains(&key(o)))
    })
}

pub fn plan(
    meetings: &[Meeting],
    now: DateTime<Utc>,
    mem: &Memory,
    machine: UiState,
    on_screen: Option<&OnScreen>,
) -> Plan {
    let recording = machine == UiState::Recording;
    let menu = if recording {
        Vec::new()
    } else {
        meetings
            .iter()
            .filter(|m| within(now, m.start, MENU_LEAD_SECS, MENU_TAIL_SECS))
            .map(|m| MenuEntry { id: m.id.clone(), title: m.title.clone(), start: m.start })
            .collect()
    };
    let popup = match on_screen {
        // Плашка «Записать» живёт столько же, сколько взвод: её прячет
        // `audio::sync` на выходе из Armed.
        Some(s) if s.mode == Mode::Record => PopupAction::Stay,
        Some(s) => {
            let alive = !recording
                && !mem.refused.contains(&(s.id.clone(), s.start))
                && meetings
                    .iter()
                    .any(|m| m.id == s.id && m.start == s.start && m.start.checked_add_signed(secs(POPUP_TAIL_SECS)).is_some_and(|hi| now <= hi));
            if alive { PopupAction::Stay } else { PopupAction::Hide }
        }
        // Пока висит вопрос детектора или идёт запись, своих вопросов не задаём.
        None if machine != UiState::Idle => PopupAction::Stay,
        None => pick_popup(meetings, now, mem).map_or(PopupAction::Stay, |m| PopupAction::Show(m.id.clone())),
    };
    Plan { popup, menu }
}

/// Звонилка заняла микрофон, машина во взводе. Вне окна встречи — старый
/// вопрос; в окне — календарная плашка «Записать»; если по всем идущим
/// встречам уже отказались — молчать.
pub fn on_armed(meetings: &[Meeting], now: DateTime<Utc>, mem: &Memory) -> Armed {
    let mut in_window = meetings
        .iter()
        .filter(|m| {
            m.start.checked_sub_signed(secs(DETECT_LEAD_SECS)).is_some_and(|lo| now >= lo) && now <= m.end
        })
        .peekable();
    if in_window.peek().is_none() {
        return Armed::Classic;
    }
    match in_window.find(|m| !mem.refused.contains(&key(m))) {
        Some(m) => Armed::Calendar(m.id.clone()),
        None => Armed::Silent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn m(id: &str, start: &str, end: &str) -> Meeting {
        Meeting {
            id: id.into(),
            title: format!("Встреча {id}"),
            start: t(start),
            end: t(end),
            url: format!("https://meet.google.com/{id}"),
            source: Source::Meet,
        }
    }

    /// «a» — 12:00–13:00.
    fn одна() -> Vec<Meeting> {
        vec![m("a", "2026-10-11T12:00:00Z", "2026-10-11T13:00:00Z")]
    }

    fn вход(id: &str) -> OnScreen {
        OnScreen { id: id.into(), start: t("2026-10-11T12:00:00Z"), mode: Mode::Join }
    }

    fn mem(shown: &[&str], refused: &[&str]) -> Memory {
        Memory {
            shown: shown.iter().map(|s| (s.to_string(), t("2026-10-11T12:00:00Z"))).collect(),
            refused: refused.iter().map(|s| (s.to_string(), t("2026-10-11T12:00:00Z"))).collect(),
        }
    }

    fn ids(p: &Plan) -> Vec<&str> {
        p.menu.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn всплывашка_с_минуты_до_начала() {
        let w = одна();
        let рано = plan(&w, t("2026-10-11T11:58:59Z"), &Memory::default(), UiState::Idle, None);
        assert_eq!(рано.popup, PopupAction::Stay);
        let пора = plan(&w, t("2026-10-11T11:59:00Z"), &Memory::default(), UiState::Idle, None);
        assert_eq!(пора.popup, PopupAction::Show("a".into()));
    }

    #[test]
    fn всплывашка_живёт_до_двух_минут_после_начала() {
        let w = одна();
        let s = вход("a");
        let ещё = plan(&w, t("2026-10-11T12:02:00Z"), &mem(&["a"], &[]), UiState::Idle, Some(&s));
        assert_eq!(ещё.popup, PopupAction::Stay);
        let всё = plan(&w, t("2026-10-11T12:02:01Z"), &mem(&["a"], &[]), UiState::Idle, Some(&s));
        assert_eq!(всё.popup, PopupAction::Hide);
    }

    #[test]
    fn появившаяся_позже_всплывает_если_окно_ещё_открыто() {
        let p = plan(&одна(), t("2026-10-11T12:01:30Z"), &Memory::default(), UiState::Idle, None);
        assert_eq!(p.popup, PopupAction::Show("a".into()));
        let поздно = plan(&одна(), t("2026-10-11T12:02:01Z"), &Memory::default(), UiState::Idle, None);
        assert_eq!(поздно.popup, PopupAction::Stay);
    }

    #[test]
    fn один_раз_на_встречу() {
        let p = plan(&одна(), t("2026-10-11T12:00:30Z"), &mem(&["a"], &[]), UiState::Idle, None);
        assert_eq!(p.popup, PopupAction::Stay);
    }

    #[test]
    fn смахнутую_прячет_и_больше_не_показывает_но_пункт_меню_остаётся() {
        let w = одна();
        let память = mem(&["a"], &["a"]);
        let p = plan(&w, t("2026-10-11T12:00:30Z"), &память, UiState::Idle, Some(&вход("a")));
        assert_eq!(p.popup, PopupAction::Hide);
        assert_eq!(ids(&p), ["a"]);
        let потом = plan(&w, t("2026-10-11T12:00:45Z"), &память, UiState::Idle, None);
        assert_eq!(потом.popup, PopupAction::Stay);
    }

    #[test]
    fn меню_с_пяти_минут_до_до_десяти_после() {
        let w = одна();
        let at = |s: &str| plan(&w, t(s), &Memory::default(), UiState::Idle, None);
        assert!(ids(&at("2026-10-11T11:54:59Z")).is_empty());
        assert_eq!(ids(&at("2026-10-11T11:55:00Z")), ["a"]);
        assert_eq!(ids(&at("2026-10-11T12:10:00Z")), ["a"]);
        assert!(ids(&at("2026-10-11T12:10:01Z")).is_empty());
    }

    #[test]
    fn во_время_записи_меню_пусто_и_всплывашка_уходит() {
        let w = одна();
        let p = plan(&w, t("2026-10-11T12:00:10Z"), &mem(&["a"], &[]), UiState::Recording, Some(&вход("a")));
        assert_eq!(p.popup, PopupAction::Hide);
        assert!(p.menu.is_empty());
        let без = plan(&w, t("2026-10-11T11:59:30Z"), &Memory::default(), UiState::Recording, None);
        assert_eq!(без.popup, PopupAction::Stay);
    }

    #[test]
    fn после_остановки_записи_встреча_снова_в_меню() {
        let p = plan(&одна(), t("2026-10-11T12:05:00Z"), &mem(&["a"], &[]), UiState::Idle, None);
        assert_eq!(ids(&p), ["a"]);
    }

    #[test]
    fn пока_висит_вопрос_детектора_новых_всплывашек_нет() {
        let p = plan(&одна(), t("2026-10-11T11:59:30Z"), &Memory::default(), UiState::Armed, None);
        assert_eq!(p.popup, PopupAction::Stay);
        assert_eq!(ids(&p), ["a"]);
    }

    #[test]
    fn режим_записать_живёт_по_взводу_а_не_по_часам() {
        let s = OnScreen { id: "a".into(), start: t("2026-10-11T12:00:00Z"), mode: Mode::Record };
        let p = plan(&одна(), t("2026-10-11T12:40:00Z"), &mem(&["a"], &[]), UiState::Armed, Some(&s));
        assert_eq!(p.popup, PopupAction::Stay);
    }

    #[test]
    fn две_в_одно_время_всплывашка_про_первую_в_меню_обе() {
        let w = vec![
            m("a", "2026-10-11T12:00:00Z", "2026-10-11T13:00:00Z"),
            m("b", "2026-10-11T12:00:00Z", "2026-10-11T12:30:00Z"),
        ];
        let p = plan(&w, t("2026-10-11T11:59:30Z"), &Memory::default(), UiState::Idle, None);
        assert_eq!(p.popup, PopupAction::Show("a".into()));
        assert_eq!(ids(&p), ["a", "b"]);
        // Первую смахнули — про вторую в то же время не спрашиваем.
        let после = plan(&w, t("2026-10-11T11:59:40Z"), &mem(&["a"], &["a"]), UiState::Idle, None);
        assert_eq!(после.popup, PopupAction::Stay);
        assert_eq!(ids(&после), ["a", "b"]);
    }

    #[test]
    fn две_рядом_каждая_в_своё_время() {
        let w = vec![
            m("a", "2026-10-11T12:00:00Z", "2026-10-11T12:30:00Z"),
            m("b", "2026-10-11T12:30:00Z", "2026-10-11T13:00:00Z"),
        ];
        let p = plan(&w, t("2026-10-11T12:29:00Z"), &mem(&["a"], &[]), UiState::Idle, None);
        assert_eq!(p.popup, PopupAction::Show("b".into()));
        assert_eq!(ids(&p), ["b"]);
    }

    #[test]
    fn пропавшую_из_календаря_всплывашку_прячет() {
        let p = plan(&[], t("2026-10-11T12:00:30Z"), &mem(&["a"], &[]), UiState::Idle, Some(&вход("a")));
        assert_eq!(p.popup, PopupAction::Hide);
    }

    #[test]
    fn детектор_вне_окна_встречи_спрашивает_по_старому() {
        assert_eq!(on_armed(&одна(), t("2026-10-11T11:54:59Z"), &Memory::default()), Armed::Classic);
        assert_eq!(on_armed(&одна(), t("2026-10-11T13:00:01Z"), &Memory::default()), Armed::Classic);
        assert_eq!(on_armed(&[], t("2026-10-11T12:10:00Z"), &Memory::default()), Armed::Classic);
    }

    #[test]
    fn детектор_в_окне_встречи_даёт_календарную_плашку() {
        assert_eq!(on_armed(&одна(), t("2026-10-11T11:55:00Z"), &Memory::default()), Armed::Calendar("a".into()));
        assert_eq!(on_armed(&одна(), t("2026-10-11T13:00:00Z"), &mem(&["a"], &[])), Armed::Calendar("a".into()));
    }

    #[test]
    fn детектор_после_отказа_молчит() {
        assert_eq!(on_armed(&одна(), t("2026-10-11T12:20:00Z"), &mem(&["a"], &["a"])), Armed::Silent);
    }

    #[test]
    fn детектор_берёт_первую_неотказанную_из_идущих() {
        let w = vec![
            m("a", "2026-10-11T12:00:00Z", "2026-10-11T13:00:00Z"),
            m("b", "2026-10-11T12:30:00Z", "2026-10-11T13:00:00Z"),
        ];
        assert_eq!(on_armed(&w, t("2026-10-11T12:31:00Z"), &mem(&[], &["a"])), Armed::Calendar("b".into()));
    }

    #[test]
    fn плашка_несёт_начало_и_конец_жизни_в_мс() {
        let p = payload(&одна()[0], Mode::Join);
        assert_eq!(p.start_ms, t("2026-10-11T12:00:00Z").timestamp_millis());
        assert_eq!(p.tail_ms, t("2026-10-11T12:02:00Z").timestamp_millis());
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["mode"], "join");
        assert_eq!(json["source"], "meet");
    }

    #[test]
    fn перенесённая_встреча_всплывает_заново() {
        let mut память = mem(&["a"], &["a"]);
        let перенесена = vec![m("a", "2026-10-11T15:00:00Z", "2026-10-11T16:00:00Z")];
        память.retain_known(&перенесена);
        assert_eq!(память, Memory::default());
        let p = plan(&перенесена, t("2026-10-11T14:59:00Z"), &память, UiState::Idle, None);
        assert_eq!(p.popup, PopupAction::Show("a".into()));
    }

    #[test]
    fn плашка_про_перенесённую_или_удалённую_встречу_уходит() {
        let s = вход("a");
        let перенесена = vec![m("a", "2026-10-11T15:00:00Z", "2026-10-11T16:00:00Z")];
        let p = plan(&перенесена, t("2026-10-11T12:00:30Z"), &mem(&["a"], &[]), UiState::Idle, Some(&s));
        assert_eq!(p.popup, PopupAction::Hide);
        let удалена = plan(&[], t("2026-10-11T12:00:30Z"), &mem(&["a"], &[]), UiState::Idle, Some(&s));
        assert_eq!(удалена.popup, PopupAction::Hide);
    }

    #[test]
    fn режим_записать_остаётся_и_во_время_записи() {
        let s = OnScreen { id: "a".into(), start: t("2026-10-11T12:00:00Z"), mode: Mode::Record };
        let p = plan(&одна(), t("2026-10-11T12:40:00Z"), &mem(&["a"], &[]), UiState::Recording, Some(&s));
        assert_eq!(p.popup, PopupAction::Stay);
    }

    #[test]
    fn после_перезапуска_пустая_память_показывает_снова() {
        let p = plan(&одна(), t("2026-10-11T12:01:00Z"), &Memory::default(), UiState::Idle, None);
        assert_eq!(p.popup, PopupAction::Show("a".into()));
    }

    #[test]
    fn память_забывает_встречи_которых_больше_нет() {
        let mut память = mem(&["a", "old"], &["old"]);
        память.retain_known(&одна());
        assert_eq!(память, mem(&["a"], &[]));
    }
}
