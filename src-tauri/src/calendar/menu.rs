//! Верх меню иконки: встречи, к которым можно подключиться, или просьба
//! войти заново. Тексты собираются здесь, без Tauri, — переводчик и часовой
//! пояс приходят параметрами.

use super::planner::MenuEntry;
use chrono::TimeZone;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuLine {
    Join { id: String, text: String },
    Caption(String),
    Reauth(String),
    Separator,
}

pub const TITLE_MAX: usize = 60;

/// Меню macOS не переносит строки, а «Подключиться и записать «…»» с
/// названием на 80 знаков растянуло бы меню на полэкрана.
pub fn short_title(title: &str, untitled: &str) -> String {
    let t = title.trim();
    if t.is_empty() {
        return untitled.to_string();
    }
    if t.chars().count() <= TITLE_MAX {
        return t.to_string();
    }
    let cut: String = t.chars().take(TITLE_MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

pub type Tr<'a> = dyn Fn(&str, &[(&str, &str)]) -> String + 'a;

pub fn lines<Tz: TimeZone>(
    entries: &[MenuEntry],
    reauth: bool,
    recording: bool,
    t: &Tr,
    tz: &Tz,
) -> Vec<MenuLine>
where
    Tz::Offset: std::fmt::Display,
{
    let mut out = Vec::new();
    if reauth {
        // Во время записи вход через браузер не предлагаем.
        if !recording {
            out.push(MenuLine::Reauth(t("calendar.trayReauth", &[])));
        }
    } else {
        let untitled = t("calendar.untitled", &[]);
        for e in entries {
            let title = short_title(&e.title, &untitled);
            out.push(MenuLine::Join {
                id: e.id.clone(),
                text: t("calendar.trayJoin", &[("title", &title)]),
            });
            let time = e.start.with_timezone(tz).format("%H:%M").to_string();
            out.push(MenuLine::Caption(t("calendar.trayStartsAt", &[("time", &time)])));
        }
    }
    if !out.is_empty() {
        out.push(MenuLine::Separator);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, FixedOffset, Utc};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Переводчик-подделка: ключ и параметры как есть — тест не зависит от словаря.
    fn тр(k: &str, p: &[(&str, &str)]) -> String {
        let mut s = k.to_string();
        for (n, v) in p {
            s.push_str(&format!(" {n}={v}"));
        }
        s
    }

    fn мск() -> FixedOffset {
        FixedOffset::east_opt(3 * 3600).unwrap()
    }

    fn e(id: &str, title: &str, start: &str) -> MenuEntry {
        MenuEntry { id: id.into(), title: title.into(), start: t(start) }
    }

    #[test]
    fn нет_встреч_нет_строк() {
        assert!(lines(&[], false, false, &тр, &мск()).is_empty());
    }

    #[test]
    fn встреча_это_пункт_подпись_и_разделитель() {
        let l = lines(&[e("a", "Дизайн-ревью PlanPan", "2026-10-11T12:00:00Z")], false, false, &тр, &мск());
        assert_eq!(
            l,
            vec![
                MenuLine::Join { id: "a".into(), text: "calendar.trayJoin title=Дизайн-ревью PlanPan".into() },
                MenuLine::Caption("calendar.trayStartsAt time=15:00".into()),
                MenuLine::Separator,
            ]
        );
    }

    #[test]
    fn две_встречи_две_пары_и_один_разделитель() {
        let l = lines(
            &[e("a", "A", "2026-10-11T12:00:00Z"), e("b", "B", "2026-10-11T12:30:00Z")],
            false,
            false,
            &тр,
            &мск(),
        );
        assert_eq!(l.len(), 5);
        assert_eq!(l[2], MenuLine::Join { id: "b".into(), text: "calendar.trayJoin title=B".into() });
        assert_eq!(l[3], MenuLine::Caption("calendar.trayStartsAt time=15:30".into()));
        assert_eq!(l[4], MenuLine::Separator);
    }

    #[test]
    fn отключился_это_один_пункт_войти_заново() {
        let l = lines(&[e("a", "A", "2026-10-11T12:00:00Z")], true, false, &тр, &мск());
        assert_eq!(l, vec![MenuLine::Reauth("calendar.trayReauth".into()), MenuLine::Separator]);
    }

    #[test]
    fn во_время_записи_войти_заново_не_показываем() {
        assert!(lines(&[e("a", "A", "2026-10-11T12:00:00Z")], true, true, &тр, &мск()).is_empty());
    }

    #[test]
    fn пустое_название_и_длинное() {
        assert_eq!(short_title("  ", "Без названия"), "Без названия");
        let длинное = "Еженедельная встреча команды дизайна и разработки по мобильному приложению";
        let s = short_title(длинное, "");
        assert_eq!(s.chars().count(), TITLE_MAX);
        assert!(s.ends_with('…'));
        assert_eq!(short_title("Синк", ""), "Синк");
    }
}
