//! Состояние календаря в памяти и чистые переходы между состояниями.

use super::model::{Meeting, Source};
use super::planner::{Memory, OnScreen};
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    #[default]
    Disconnected,
    Waiting,
    Connected,
    Revoked,
    /// Google не дал доступ (`access_denied`) или браузер не ответил за
    /// 5 минут. Кнопка «Подключить» обычная, под ней серая подсказка.
    Refused,
}

#[derive(Default)]
pub struct Runtime {
    pub auth: Auth,
    pub email: Option<String>,
    pub meetings: Vec<Meeting>,
    pub loaded_at: Option<DateTime<Utc>>,
    pub offline: bool,
    pub access: Option<(String, DateTime<Utc>)>,
    /// Номер сессии входа: растёт при отключении и новом входе. Опрос,
    /// начатый в прежней сессии, своим ответом новую не трогает.
    pub session: u64,
    /// Флаг «Отмена» текущего входа: свой на каждый вход.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Про какие встречи уже спрашивали и от каких отказались. Только в
    /// памяти: после перезапуска приложения забывается.
    pub memory: Memory,
    /// Что сейчас во всплывашке, если там встреча из календаря.
    pub on_screen: Option<OnScreen>,
    /// Встречи, к которым уже присоединились (ссылку открывали). Повторный
    /// «Зайти» по той же паре (id, начало) ничего не делает.
    pub opened: std::collections::HashSet<(String, DateTime<Utc>)>,
    /// Что сейчас стоит в меню иконки — чтобы не перестраивать его каждые 15 с.
    pub last_menu: Option<Vec<super::menu::MenuLine>>,
}

pub enum PollOutcome {
    Loaded(Vec<Meeting>),
    Offline,
    Revoked,
    /// Связка ключей отказала в доступе (не «записи нет»): опрос больше не
    /// стучится в неё, пока человек сам не войдёт заново.
    KeychainDenied,
}

pub fn apply_poll(rt: &mut Runtime, outcome: PollOutcome, now: DateTime<Utc>) {
    match outcome {
        PollOutcome::Loaded(meetings) => {
            rt.memory.retain_known(&meetings);
            rt.opened.retain(|(id, st)| meetings.iter().any(|m| &m.id == id && m.start == *st));
            rt.meetings = meetings;
            rt.loaded_at = Some(now);
            rt.offline = false;
        }
        // Нет сети — держим последний удачный список, без ошибки на экране.
        PollOutcome::Offline => rt.offline = true,
        PollOutcome::Revoked | PollOutcome::KeychainDenied => {
            rt.auth = Auth::Revoked;
            rt.memory = Memory::default();
            rt.opened.clear();
            rt.on_screen = None;
            rt.meetings.clear();
            rt.access = None;
            rt.loaded_at = None;
            rt.offline = false;
        }
    }
}

/// Применить ответ опроса, начатого в сессии `session`. Не применяет, если
/// сессия сменилась или календарь уже не «Connected». Возвращает true, только
/// когда календарь реально стал «отключился» (`Revoked` или `KeychainDenied`).
pub fn apply_poll_in_session(rt: &mut Runtime, outcome: PollOutcome, now: DateTime<Utc>, session: u64) -> bool {
    if rt.session != session || rt.auth != Auth::Connected {
        return false;
    }
    let revoked = matches!(outcome, PollOutcome::Revoked | PollOutcome::KeychainDenied);
    apply_poll(rt, outcome, now);
    revoked
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginKind {
    Connected,
    /// `access_denied` или таймаут ожидания браузера.
    NoAccess,
    /// «Отмена» в самом MeetRec.
    Abandoned,
    Failed,
}

/// Состояние после попытки входа. Отмена и сбой возвращают то, что было до
/// входа: «Google Календарь отключился» не должно пропасть оттого, что
/// человек нажал «Отмена». «Google не дал доступ» после отмены уходит — это
/// подсказка к прошлой попытке, а не состояние.
///
/// `NoAccess` не говорит «кончились места»: `access_denied` приходит и на
/// обычную «Отмену» на странице Google (решение 10 плана).
pub fn auth_after(before: Auth, kind: LoginKind) -> Auth {
    match kind {
        LoginKind::Connected => Auth::Connected,
        LoginKind::NoAccess => Auth::Refused,
        LoginKind::Abandoned | LoginKind::Failed => match before {
            Auth::Refused | Auth::Waiting => Auth::Disconnected,
            other => other,
        },
    }
}

/// Записать результат входа в состояние, если сессия та же. false — пока
/// шёл вход, нажали «Отключить»: записанное в связку и конфиг надо откатить.
pub fn commit_login(rt: &mut Runtime, session: u64, email: Option<String>, access: (String, DateTime<Utc>)) -> bool {
    if rt.session != session {
        return false;
    }
    rt.email = email;
    rt.access = Some(access);
    true
}

/// Итог входа для текущей сессии. Если за время ожидания сессия сменилась
/// («Отключить»), состояние не трогаем. Успешный вход начинает новую сессию.
pub fn settle_login(rt: &mut Runtime, before: Auth, kind: LoginKind, session: u64) -> bool {
    if rt.session != session {
        return false;
    }
    rt.auth = auth_after(before, kind);
    if kind == LoginKind::Connected {
        rt.session += 1;
    }
    true
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct NextView {
    pub title: String,
    pub start_ms: i64,
}

/// Встреча для зоны «Дальше» главного окна. Отсчёт и «Зайти и записать»
/// (за 10 минут до начала и пока идёт) окно считает само по `start_ms` и
/// `end_ms`: статус приходит раз в опрос (5 минут), а отсчёт должен идти.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct MeetingView {
    pub id: String,
    /// Может быть пустым — «Без названия» подставляет окно.
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub source: Source,
    pub has_link: bool,
}

/// Сколько встреч показывает зона «Дальше». Больше трёх в окне 460×640
/// съедают место под записи.
pub const TODAY_MAX: usize = 3;
/// За сколько до начала встреча считается «скоро» и в окно попадает, даже
/// если начнётся уже завтра (без пяти полночь).
pub const SOON_SECS: i64 = 10 * 60;

/// Встречи на сегодня для главного окна: идущие и будущие, до конца
/// местных суток (плюс те, что начнутся в ближайшие 10 минут), по началу,
/// не больше [`TODAY_MAX`]. Часовой пояс — параметром, чтобы тесты не
/// зависели от машины.
pub fn today_view<Tz: TimeZone>(meetings: &[Meeting], now: DateTime<Utc>, tz: &Tz) -> Vec<MeetingView> {
    let local = now.with_timezone(tz);
    let end_of_day = local
        .date_naive()
        .succ_opt()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|d| tz.from_local_datetime(&d).earliest())
        .map(|d| d.with_timezone(&Utc));
    let soon = now + chrono::TimeDelta::seconds(SOON_SECS);
    let mut out: Vec<&Meeting> = meetings
        .iter()
        // Идёт (конец ещё впереди) или ещё не началась. У встречи без конца
        // `end == start`: она видна, пока не началась.
        .filter(|m| m.end > now || m.start >= now)
        .filter(|m| m.start <= soon || end_of_day.is_none_or(|e| m.start < e))
        .collect();
    out.sort_by_key(|m| m.start);
    out.into_iter()
        .take(TODAY_MAX)
        .map(|m| MeetingView {
            id: m.id.clone(),
            title: m.title.clone(),
            start_ms: m.start.timestamp_millis(),
            end_ms: m.end.timestamp_millis(),
            source: m.source,
            has_link: !m.url.is_empty(),
        })
        .collect()
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct StatusView {
    pub available: bool,
    pub auth: Auth,
    pub email: Option<String>,
    pub next: Option<NextView>,
    /// Зона «Дальше» главного окна, см. [`today_view`].
    pub today: Vec<MeetingView>,
    pub offline: bool,
    pub loaded_at_ms: Option<i64>,
}

pub fn status_view(rt: &Runtime, now: DateTime<Utc>, available: bool) -> StatusView {
    status_view_in(rt, now, available, &chrono::Local)
}

pub fn status_view_in<Tz: TimeZone>(rt: &Runtime, now: DateTime<Utc>, available: bool, tz: &Tz) -> StatusView {
    let next = rt
        .meetings
        .iter()
        .filter(|m| m.start >= now)
        .min_by_key(|m| m.start)
        .map(|m| NextView { title: m.title.clone(), start_ms: m.start.timestamp_millis() });
    StatusView {
        available,
        auth: rt.auth,
        email: rt.email.clone(),
        next,
        today: today_view(&rt.meetings, now, tz),
        offline: rt.offline,
        loaded_at_ms: rt.loaded_at.map(|t| t.timestamp_millis()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::model::parse_events;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn встречи() -> Vec<Meeting> {
        parse_events(include_str!("fixtures/events.json")).unwrap()
    }

    fn подключён() -> Runtime {
        Runtime { auth: Auth::Connected, email: Some("name@gmail.com".into()), ..Runtime::default() }
    }

    #[test]
    fn отказ_связки_переводит_в_отключился_и_чистит_встречи() {
        let mut rt = подключён();
        rt.meetings = встречи();
        assert!(apply_poll_in_session(&mut rt, PollOutcome::KeychainDenied, t("2026-10-11T11:00:00Z"), 0));
        assert_eq!(rt.auth, Auth::Revoked);
        assert!(rt.meetings.is_empty());
        // Дальше опрос не идёт: он работает только в Connected.
        assert!(!apply_poll_in_session(&mut rt, PollOutcome::KeychainDenied, t("2026-10-11T11:05:00Z"), 0));
    }

    #[test]
    fn загрузка_кладёт_встречи_и_время_и_снимает_офлайн() {
        let mut rt = подключён();
        rt.offline = true;
        apply_poll(&mut rt, PollOutcome::Loaded(встречи()), t("2026-10-11T11:00:00Z"));
        assert_eq!(rt.meetings.len(), 5);
        assert_eq!(rt.loaded_at, Some(t("2026-10-11T11:00:00Z")));
        assert!(!rt.offline);
    }

    #[test]
    fn загрузка_забывает_память_про_пропавшие_встречи() {
        let mut rt = подключён();
        let k = ("old".to_string(), t("2026-10-11T09:00:00Z"));
        rt.memory.shown.insert(k.clone());
        rt.memory.refused.insert(k);
        apply_poll(&mut rt, PollOutcome::Loaded(встречи()), t("2026-10-11T11:00:00Z"));
        assert_eq!(rt.memory, Memory::default());
    }

    #[test]
    fn отзыв_стирает_память_и_плашку() {
        use crate::calendar::planner::Mode;
        let mut rt = подключён();
        rt.memory.shown.insert(("a".into(), t("2026-10-11T12:00:00Z")));
        rt.on_screen = Some(OnScreen { id: "a".into(), start: t("2026-10-11T12:00:00Z"), mode: Mode::Join });
        apply_poll(&mut rt, PollOutcome::Revoked, t("2026-10-11T11:05:00Z"));
        assert_eq!(rt.memory, Memory::default());
        assert!(rt.on_screen.is_none());
    }

    #[test]
    fn без_сети_встречи_остаются() {
        let mut rt = подключён();
        apply_poll(&mut rt, PollOutcome::Loaded(встречи()), t("2026-10-11T11:00:00Z"));
        apply_poll(&mut rt, PollOutcome::Offline, t("2026-10-11T11:05:00Z"));
        assert_eq!(rt.meetings.len(), 5);
        assert!(rt.offline);
        assert_eq!(rt.loaded_at, Some(t("2026-10-11T11:00:00Z")), "время последней удачной загрузки");
        assert_eq!(rt.auth, Auth::Connected);
    }

    #[test]
    fn отзыв_доступа_чистит_встречи_и_токен() {
        let mut rt = подключён();
        rt.access = Some(("a".into(), t("2026-10-11T12:00:00Z")));
        apply_poll(&mut rt, PollOutcome::Loaded(встречи()), t("2026-10-11T11:00:00Z"));
        apply_poll(&mut rt, PollOutcome::Revoked, t("2026-10-11T11:05:00Z"));
        assert_eq!(rt.auth, Auth::Revoked);
        assert!(rt.meetings.is_empty());
        assert!(rt.access.is_none());
        assert!(!rt.offline);
        assert_eq!(rt.email.as_deref(), Some("name@gmail.com"), "почта нужна для «войти заново»");
    }

    #[test]
    fn старый_опрос_не_применяется_к_новой_сессии() {
        let mut rt = подключён();
        rt.session = 2;
        assert!(!apply_poll_in_session(&mut rt, PollOutcome::Revoked, t("2026-10-11T11:00:00Z"), 1));
        assert_eq!(rt.auth, Auth::Connected, "новый вход не сброшен чужим отзывом");
        assert!(apply_poll_in_session(&mut rt, PollOutcome::Revoked, t("2026-10-11T11:00:00Z"), 2));
        assert_eq!(rt.auth, Auth::Revoked);
    }

    #[test]
    fn отзыв_не_применяется_если_уже_не_подключён() {
        let mut rt = Runtime::default();
        assert!(!apply_poll_in_session(&mut rt, PollOutcome::Revoked, t("2026-10-11T11:00:00Z"), 0));
        assert_eq!(rt.auth, Auth::Disconnected);
    }

    #[test]
    fn отключение_во_время_входа_не_возвращает_старое_состояние() {
        let mut rt = Runtime { session: 3, ..Runtime::default() };
        assert!(!settle_login(&mut rt, Auth::Revoked, LoginKind::Abandoned, 2));
        assert_eq!(rt.auth, Auth::Disconnected);
        assert!(settle_login(&mut rt, Auth::Revoked, LoginKind::Abandoned, 3));
        assert_eq!(rt.auth, Auth::Revoked, "обычная отмена в той же сессии");
        assert!(settle_login(&mut rt, Auth::Revoked, LoginKind::Connected, 3));
        assert_eq!((rt.auth, rt.session), (Auth::Connected, 4));
    }

    #[test]
    fn результат_входа_пишется_только_в_свою_сессию() {
        let mut rt = Runtime { session: 5, ..Runtime::default() };
        let acc = ("a".to_string(), t("2026-10-11T12:00:00Z"));
        assert!(!commit_login(&mut rt, 4, Some("x@gmail.com".into()), acc.clone()));
        assert!(rt.email.is_none() && rt.access.is_none());
        assert!(commit_login(&mut rt, 5, Some("x@gmail.com".into()), acc));
        assert_eq!(rt.email.as_deref(), Some("x@gmail.com"));
        assert!(rt.access.is_some());
    }

    #[test]
    fn после_входа() {
        use Auth::*;
        assert_eq!(auth_after(Disconnected, LoginKind::Connected), Connected);
        assert_eq!(auth_after(Revoked, LoginKind::Connected), Connected);
        assert_eq!(auth_after(Disconnected, LoginKind::NoAccess), Refused);
        assert_eq!(auth_after(Revoked, LoginKind::NoAccess), Refused);
        assert_eq!(auth_after(Disconnected, LoginKind::Abandoned), Disconnected);
        assert_eq!(auth_after(Revoked, LoginKind::Abandoned), Revoked, "отмена не стирает «отключился»");
        assert_eq!(auth_after(Refused, LoginKind::Abandoned), Disconnected, "подсказка — к прошлой попытке");
        assert_eq!(auth_after(Revoked, LoginKind::Failed), Revoked);
    }

    #[test]
    fn ближайшая_встреча_первая_ещё_не_начавшаяся() {
        let mut rt = подключён();
        apply_poll(&mut rt, PollOutcome::Loaded(встречи()), t("2026-10-11T12:10:00Z"));
        let v = status_view(&rt, t("2026-10-11T12:10:00Z"), true);
        assert_eq!(v.auth, Auth::Connected);
        assert_eq!(v.email.as_deref(), Some("name@gmail.com"));
        assert_eq!(v.next, Some(NextView { title: "Синк с Мишей".into(), start_ms: t("2026-10-11T12:30:00Z").timestamp_millis() }));
        assert_eq!(v.loaded_at_ms, Some(t("2026-10-11T12:10:00Z").timestamp_millis()));
    }

    #[test]
    fn нет_встреч_впереди_нет_ближайшей() {
        let mut rt = подключён();
        apply_poll(&mut rt, PollOutcome::Loaded(встречи()), t("2026-10-11T20:30:00Z"));
        assert_eq!(status_view(&rt, t("2026-10-11T20:30:00Z"), true).next, None);
    }

    fn мск() -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(3 * 3600).unwrap()
    }

    #[test]
    fn сегодня_идущие_и_будущие_по_началу_не_больше_трёх() {
        // 15:20 по Москве: «Дизайн-ревью» (15:00–16:00) идёт, дальше
        // «Синк» 15:30, «Интервью» 17:00, без названия 21:00, «Партнёры» 22:00.
        let v = today_view(&встречи(), t("2026-10-11T12:20:00Z"), &мск());
        let ids: Vec<&str> = v.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["zoom-conf", "meet-hangout", "teams-desc"]);
        assert_eq!(v[0].source, Source::Zoom);
        assert_eq!(v[1].source, Source::Meet);
        assert!(v.iter().all(|m| m.has_link));
        assert_eq!(v[0].end_ms, t("2026-10-11T13:00:00Z").timestamp_millis());
    }

    #[test]
    fn закончившиеся_не_показываем() {
        let v = today_view(&встречи(), t("2026-10-11T18:10:00Z"), &мск());
        let ids: Vec<&str> = v.iter().map(|m| m.id.as_str()).collect();
        // 21:10 МСК: без названия (21:00–21:30) идёт, «Партнёры» в 22:00.
        assert_eq!(ids, ["zoom-location", "webex-then-zoom"]);
    }

    #[test]
    fn завтрашние_не_показываем_кроме_ближайших_десяти_минут() {
        let mut m = встречи();
        m.retain(|x| x.id == "zoom-conf");
        // Встреча в 00:05 по Москве, сейчас 23:50 — через 15 минут, но уже завтра.
        m[0].start = t("2026-10-11T21:05:00Z");
        m[0].end = t("2026-10-11T22:00:00Z");
        assert!(today_view(&m, t("2026-10-11T20:50:00Z"), &мск()).is_empty());
        // В 23:56 до неё 9 минут — показываем, иначе «Зайти» пропадёт в полночь.
        assert_eq!(today_view(&m, t("2026-10-11T20:56:00Z"), &мск()).len(), 1);
    }

    #[test]
    fn вид_для_окна_сериализуется_как_ждёт_main_js() {
        let v = status_view(&Runtime::default(), t("2026-10-11T12:00:00Z"), false);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["available"], false);
        assert_eq!(json["auth"], "disconnected");
        assert!(json["next"].is_null());
        assert_eq!(json["today"], serde_json::json!([]));
        assert!(json["loaded_at_ms"].is_null());
    }
}
