//! Refresh token Google в связке ключей.
//!
//! Подпись у MeetRec ad-hoc, и после каждого обновления приложения macOS
//! может спросить «MeetRec хочет использовать конфиденциальную информацию»
//! при первом чтении: доступ к записи привязан к подписи бинаря. Поэтому
//! `save` сначала удаляет старую запись — новая создаётся с доступом для
//! текущей сборки.

const SERVICE: &str = "net.meetrec.app.google-calendar";
const ACCOUNT: &str = "refresh-token";

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())
}

pub fn save(refresh: &str) -> Result<(), String> {
    let e = entry()?;
    let _ = e.delete_credential();
    e.set_password(refresh).map_err(|e| e.to_string())
}

/// `Ok(None)` — записи нет (не входили или её стёрли).
pub fn load() -> Result<Option<String>, String> {
    match entry()?.get_password() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn delete() {
    if let Ok(e) = entry() {
        let _ = e.delete_credential();
    }
}
