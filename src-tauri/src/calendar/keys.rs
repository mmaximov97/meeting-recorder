//! Client ID и client secret Google — только из окружения сборки.
//!
//! В репозиторий они не попадают. Локально их кладёт `src-tauri/build.rs`
//! из `~/.config/meetrec/google-oauth.env`, в релизе — секреты GitHub
//! (`.github/workflows/release.yml`). Нет ключей — `keys()` даёт `None`, и
//! календарь в этой сборке выключен целиком.
//!
//! Google считает secret десктопного клиента несекретным: он всё равно
//! вшит в приложение, и его можно достать из бинаря. Не держим его в
//! репозитории, чтобы его не растащили по форкам.

#[derive(Clone, Copy)]
pub struct Keys {
    pub client_id: &'static str,
    pub client_secret: &'static str,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys")
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .finish()
    }
}

pub fn keys() -> Option<Keys> {
    keys_from(
        option_env!("MEETREC_GOOGLE_CLIENT_ID"),
        option_env!("MEETREC_GOOGLE_CLIENT_SECRET"),
    )
}

fn keys_from(id: Option<&'static str>, secret: Option<&'static str>) -> Option<Keys> {
    let client_id = id.map(str::trim).filter(|s| !s.is_empty())?;
    let client_secret = secret.map(str::trim).filter(|s| !s.is_empty())?;
    Some(Keys { client_id, client_secret })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn оба_ключа_есть() {
        let k = keys_from(Some("id.apps.googleusercontent.com"), Some("GOCSPX-x")).unwrap();
        assert_eq!(k.client_id, "id.apps.googleusercontent.com");
        assert_eq!(k.client_secret, "GOCSPX-x");
    }

    #[test]
    fn debug_не_печатает_secret() {
        let k = keys_from(Some("id"), Some("GOCSPX-секрет")).unwrap();
        let out = format!("{k:?}");
        assert!(!out.contains("GOCSPX") && out.contains("***"));
    }

    #[test]
    fn нет_или_пустой_любой_ключ_это_выключено() {
        assert!(keys_from(None, Some("s")).is_none());
        assert!(keys_from(Some("i"), None).is_none());
        assert!(keys_from(Some("  "), Some("s")).is_none());
        assert!(keys_from(Some("i"), Some("")).is_none());
    }

    /// Ручная проверка, что сборка увидела ключи из файла:
    /// `cargo test -p meeting-recorder-gui keys_видны_сборке -- --ignored`
    #[test]
    #[ignore]
    fn keys_видны_сборке() {
        assert!(keys().is_some(), "ключей нет: проверь ~/.config/meetrec/google-oauth.env и `touch src-tauri/build.rs`");
    }
}
