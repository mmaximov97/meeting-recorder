//! Опрос `events.list` раз в 5 минут, пока календарь подключён.
//!
//! MutexGuard ни разу не живёт через `.await`: футура обязана быть `Send`
//! (её крутит `tauri::async_runtime`). Поэтому состояние читается отдельной
//! строкой `let x = lock(app)…;`, а не в условии `if let`.

use super::google::{self, FetchError};
use super::keys::{keys, Keys};
use super::model::merge_events;
use super::oauth::TokenError;
use super::runtime::{apply_poll_in_session, Auth, PollOutcome};
use super::{lock, token_store, Calendar};
use chrono::{TimeDelta, Utc};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const POLL_EVERY: Duration = Duration::from_secs(5 * 60);

pub fn spawn(app: AppHandle) {
    let notify = app.state::<Calendar>().poll_now.clone();
    tauri::async_runtime::spawn(async move {
        let client = google::client();
        loop {
            poll_once(&app, &client).await;
            tokio::select! {
                _ = tokio::time::sleep(POLL_EVERY) => {}
                _ = notify.notified() => {}
            }
        }
    });
}

async fn poll_once(app: &AppHandle, client: &reqwest::Client) {
    let Some(keys) = keys() else { return };
    let (connected, session) = {
        let rt = lock(app);
        (rt.auth == Auth::Connected, rt.session)
    };
    if !connected {
        return;
    }
    let outcome = fetch(app, client, keys).await;
    // Пока шёл запрос, могли нажать «Отключить» или войти заново — тогда ответ
    // не наш, и токен нового входа он стирать не должен.
    // Токен стираем только при настоящем отзыве; если связка отказала в
    // доступе, к ней не прикасаемся до явного входа.
    let wipe = matches!(outcome, PollOutcome::Revoked);
    // Календарная плашка на экране? Смотрим до применения: отзыв обнуляет on_screen.
    let (revoked, had_calendar_popup) = {
        let mut rt = lock(app);
        let had = rt.on_screen.is_some();
        (apply_poll_in_session(&mut rt, outcome, Utc::now(), session), had)
    };
    if revoked {
        if wipe {
            let _ = tauri::async_runtime::spawn_blocking(token_store::delete).await;
        }
        // Календарная плашка с мёртвой кнопкой не должна висеть; классическую
        // «Записать встречу?» не трогаем.
        if had_calendar_popup {
            let _ = app.emit_to("ask", "calendar-hide", ());
            crate::audio::hide_ask_popup(app);
        }
    }
    super::emit_status(app);
    super::wake(app);
}

/// Что делать опросу с ошибкой получения токена.
fn outcome_of_token_error(e: &TokenError) -> PollOutcome {
    match e {
        TokenError::InvalidGrant => PollOutcome::Revoked,
        TokenError::Keychain(_) => PollOutcome::KeychainDenied,
        TokenError::Network(_) | TokenError::Other(_) => PollOutcome::Offline,
    }
}

async fn fetch(app: &AppHandle, client: &reqwest::Client, keys: Keys) -> PollOutcome {
    for attempt in 0..2 {
        let token = match access_token(app, client, keys).await {
            Ok(t) => t,
            Err(e) => {
                if !matches!(e, TokenError::InvalidGrant) {
                    log::warn!("календарь: токен не обновился: {e:?}");
                }
                return outcome_of_token_error(&e);
            }
        };
        match google::list_events(client, &token, Utc::now()).await {
            Ok(bodies) => {
                return match merge_events(&bodies) {
                    Ok(meetings) => PollOutcome::Loaded(meetings),
                    Err(e) => {
                        log::warn!("календарь: {e}");
                        PollOutcome::Offline
                    }
                }
            }
            Err(FetchError::Unauthorized) if attempt == 0 => {
                lock(app).access = None;
            }
            Err(e) => {
                log::warn!("календарь: список встреч не загрузился: {e:?}");
                return PollOutcome::Offline;
            }
        }
    }
    PollOutcome::Offline
}

async fn access_token(app: &AppHandle, client: &reqwest::Client, keys: Keys) -> Result<String, TokenError> {
    let cached = lock(app).access.clone();
    if let Some((token, until)) = cached {
        if until > Utc::now() + TimeDelta::seconds(60) {
            return Ok(token);
        }
    }
    // Связка ключей может показать системный вопрос и ждать ответа человека —
    // не на рабочем потоке рантайма.
    let stored = tauri::async_runtime::spawn_blocking(token_store::load)
        .await
        .map_err(|e| TokenError::Other(e.to_string()))?;
    let refresh = match stored {
        Ok(Some(r)) => r,
        Ok(None) => return Err(TokenError::InvalidGrant),
        Err(e) => return Err(TokenError::Keychain(e)),
    };
    let t = google::refresh(client, keys, &refresh).await?;
    lock(app).access = Some((t.access_token.clone(), Utc::now() + TimeDelta::seconds(t.expires_in)));
    Ok(t.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ошибка_связки_это_отключился_а_сеть_это_офлайн() {
        assert!(matches!(outcome_of_token_error(&TokenError::Keychain("x".into())), PollOutcome::KeychainDenied));
        assert!(matches!(outcome_of_token_error(&TokenError::InvalidGrant), PollOutcome::Revoked));
        assert!(matches!(outcome_of_token_error(&TokenError::Network("x".into())), PollOutcome::Offline));
        assert!(matches!(outcome_of_token_error(&TokenError::Other("x".into())), PollOutcome::Offline));
    }
}
