/// Причина старта записи. От неё зависит правило остановки — см. handle().
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    /// Детект сработал, пишем в кольцо, ждём ответа пользователя.
    Armed,
    Recording(Trigger),
    /// Файл закрывается (`Action::CloseFile` уже выдан), ждём `FinalizeDone`.
    ///
    /// Контракт с вызывающим кодом: `FinalizeDone` — единственный выход из
    /// этого состояния (см. `enum Event` — ни ошибки, ни таймаута там нет),
    /// и он ОБЯЗАН прийти всегда, включая случай, когда закрытие файла на
    /// стороне вызывающего провалилось (I/O-ошибка и т.п.). Машина здесь не
    /// умеет отличать «успех» от «сбой» — это единственный явный выход.
    ///
    /// Если `FinalizeDone` не прислать, машина застревает в `Finalizing`
    /// навсегда: любые последующие `SessionAppeared`/`ManualStart` уйдут в
    /// catch-all (`Action::None`, без смены состояния), и приложение молча
    /// перестанет реагировать на новые сессии/ручной старт.
    Finalizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionAppeared,
    SessionGone,
    UserConfirmed,
    UserDeclined,
    ManualStart,
    ManualStop,
    FinalizeDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StartRingBuffer,
    DiscardRing,
    FlushRingToFile,
    StartFileWrite,
    CloseFile,
    None,
}

pub struct SessionMachine {
    state: State,
}

impl SessionMachine {
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn handle(&mut self, e: Event) -> Action {
        use Event::*;
        use State::*;

        match (self.state, e) {
            (Idle, SessionAppeared) => {
                self.state = Armed;
                Action::StartRingBuffer
            }
            (Idle, ManualStart) => {
                self.state = Recording(Trigger::Manual);
                Action::StartFileWrite
            }

            // Подтверждение и ручной старт из Armed — одно и то же:
            // кольцо уже набрано, сбрасываем его в файл.
            (Armed, UserConfirmed) | (Armed, ManualStart) => {
                self.state = Recording(Trigger::Auto);
                Action::FlushRingToFile
            }
            // ManualStop в Armed семантически — тот же отказ: микрофон уже
            // захвачен под кольцо, юзер жмёт «стоп» до подтверждения записи.
            // Без этого плеча событие проваливалось в catch-all, и микрофон
            // оставался висеть в захваченном состоянии до истечения сессии.
            (Armed, UserDeclined) | (Armed, SessionGone) | (Armed, ManualStop) => {
                self.state = Idle;
                Action::DiscardRing
            }

            // Ключевое место: исчезновение mic-сессии останавливает ТОЛЬКО
            // авто-запись. У ручной сессии могло не быть вовсе.
            (Recording(Trigger::Auto), SessionGone) => {
                self.state = Finalizing;
                Action::CloseFile
            }
            (Recording(Trigger::Manual), SessionGone) => Action::None,

            (Recording(_), ManualStop) => {
                self.state = Finalizing;
                Action::CloseFile
            }

            (Finalizing, FinalizeDone) => {
                self.state = Idle;
                Action::None
            }

            // Всё остальное — шум (повторный детект во время записи и т.п.)
            _ => Action::None,
        }
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn детект_переводит_в_armed_и_запускает_кольцо() {
        let mut m = SessionMachine::new();
        assert_eq!(m.handle(Event::SessionAppeared), Action::StartRingBuffer);
        assert_eq!(m.state(), State::Armed);
    }

    #[test]
    fn подтверждение_сбрасывает_кольцо_в_файл() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::UserConfirmed), Action::FlushRingToFile);
        assert_eq!(m.state(), State::Recording(Trigger::Auto));
    }

    #[test]
    fn отказ_выбрасывает_кольцо_и_ничего_не_пишет_на_диск() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::UserDeclined), Action::DiscardRing);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn сессия_исчезла_до_ответа_выбрасывает_кольцо() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::SessionGone), Action::DiscardRing);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn авто_запись_останавливается_когда_сессия_исчезла() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        m.handle(Event::UserConfirmed);
        assert_eq!(m.handle(Event::SessionGone), Action::CloseFile);
        assert_eq!(m.state(), State::Finalizing);
    }

    #[test]
    fn ручной_старт_из_idle_минует_armed_и_кольцо() {
        let mut m = SessionMachine::new();
        assert_eq!(m.handle(Event::ManualStart), Action::StartFileWrite);
        assert_eq!(m.state(), State::Recording(Trigger::Manual));
    }

    /// Гвоздь всей задачи: у ручной записи может вообще не быть mic-сессии
    /// (разговор в комнате, телефон на громкой). Общее правило «сессия исчезла →
    /// стоп» убило бы такую запись на первой секунде.
    #[test]
    fn ручную_запись_исчезновение_сессии_не_останавливает() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        assert_eq!(m.handle(Event::SessionGone), Action::None);
        assert_eq!(m.state(), State::Recording(Trigger::Manual));
    }

    #[test]
    fn ручной_стоп_останавливает_ручную_запись() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        assert_eq!(m.handle(Event::ManualStop), Action::CloseFile);
        assert_eq!(m.state(), State::Finalizing);
    }

    #[test]
    fn ручной_стоп_останавливает_и_авто_запись() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        m.handle(Event::UserConfirmed);
        assert_eq!(m.handle(Event::ManualStop), Action::CloseFile);
        assert_eq!(m.state(), State::Finalizing);
    }

    #[test]
    fn ручной_старт_из_armed_равен_подтверждению() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::ManualStart), Action::FlushRingToFile);
        assert_eq!(m.state(), State::Recording(Trigger::Auto));
    }

    #[test]
    fn финализация_возвращает_в_idle() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        m.handle(Event::ManualStop);
        assert_eq!(m.handle(Event::FinalizeDone), Action::None);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn повторный_детект_во_время_записи_ничего_не_делает() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        assert_eq!(m.handle(Event::SessionAppeared), Action::None);
        assert_eq!(m.state(), State::Recording(Trigger::Manual));
    }

    /// Important 1 ревью: ManualStop в Armed — семантически тот же отказ,
    /// что и UserDeclined/SessionGone. Микрофон уже захвачен под кольцо,
    /// вопрос ещё висит на экране; жмём «стоп» — кольцо должно быть
    /// выброшено, а не молча провалиться в catch-all с зависшим в Armed
    /// (и потому не отпущенным) микрофоном.
    #[test]
    fn ручной_стоп_в_armed_равен_отказу_и_освобождает_микрофон() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::ManualStop), Action::DiscardRing);
        assert_eq!(m.state(), State::Idle);
    }

    /// Minor ревью: единственный прежний тест на catch-all покрывал только
    /// (Recording(Manual), SessionAppeared) — из 35 комбинаций (Armed,
    /// ManualStop) лежала ровно в непокрытых, и это была реальная бага
    /// (Important 1 выше). Табличный тест перебирает все 5 состояний × 7
    /// событий и фиксирует ожидаемую пару (Action, State) для каждой —
    /// это документация полной матрицы переходов не хуже, чем тест: любое
    /// будущее изменение поведения станет видимым здесь целиком.
    ///
    /// `State::Recording(Trigger::Auto)` и `State::Recording(Trigger::Manual)`
    /// — разные состояния, посчитаны отдельно (отсюда 5, а не 4 состояния).
    #[test]
    fn таблица_переходов_покрывает_все_state_x_event() {
        #[rustfmt::skip]
        let table: &[(State, Event, Action, State)] = &[
            // ---- Idle -----------------------------------------------------
            (State::Idle, Event::SessionAppeared, Action::StartRingBuffer, State::Armed),
            (State::Idle, Event::SessionGone,      Action::None,           State::Idle),
            (State::Idle, Event::UserConfirmed,    Action::None,           State::Idle),
            (State::Idle, Event::UserDeclined,     Action::None,           State::Idle),
            (State::Idle, Event::ManualStart,      Action::StartFileWrite, State::Recording(Trigger::Manual)),
            (State::Idle, Event::ManualStop,       Action::None,           State::Idle),
            (State::Idle, Event::FinalizeDone,     Action::None,           State::Idle),

            // ---- Armed ---- микрофон уже захвачен: SessionGone/UserDeclined/
            // ManualStop — все три эквивалентны отказу, выбрасывают кольцо
            // (Important 1 ревью, зафиксировано отдельным тестом выше тоже).
            (State::Armed, Event::SessionAppeared, Action::None,            State::Armed),
            (State::Armed, Event::SessionGone,     Action::DiscardRing,     State::Idle),
            (State::Armed, Event::UserConfirmed,   Action::FlushRingToFile, State::Recording(Trigger::Auto)),
            (State::Armed, Event::UserDeclined,    Action::DiscardRing,     State::Idle),
            (State::Armed, Event::ManualStart,     Action::FlushRingToFile, State::Recording(Trigger::Auto)),
            (State::Armed, Event::ManualStop,      Action::DiscardRing,     State::Idle),
            (State::Armed, Event::FinalizeDone,    Action::None,            State::Armed),

            // ---- Recording(Auto) -------------------------------------------
            (State::Recording(Trigger::Auto), Event::SessionAppeared, Action::None,      State::Recording(Trigger::Auto)),
            (State::Recording(Trigger::Auto), Event::SessionGone,     Action::CloseFile, State::Finalizing),
            (State::Recording(Trigger::Auto), Event::UserConfirmed,   Action::None,      State::Recording(Trigger::Auto)),
            (State::Recording(Trigger::Auto), Event::UserDeclined,    Action::None,      State::Recording(Trigger::Auto)),
            (State::Recording(Trigger::Auto), Event::ManualStart,     Action::None,      State::Recording(Trigger::Auto)),
            (State::Recording(Trigger::Auto), Event::ManualStop,      Action::CloseFile, State::Finalizing),
            (State::Recording(Trigger::Auto), Event::FinalizeDone,    Action::None,      State::Recording(Trigger::Auto)),

            // ---- Recording(Manual) ---- SessionGone здесь намеренно None: у
            // ручной записи mic-сессии могло не быть вовсе (см. докблок
            // State::Finalizing и тест ручную_запись_исчезновение...).
            (State::Recording(Trigger::Manual), Event::SessionAppeared, Action::None,      State::Recording(Trigger::Manual)),
            (State::Recording(Trigger::Manual), Event::SessionGone,     Action::None,      State::Recording(Trigger::Manual)),
            (State::Recording(Trigger::Manual), Event::UserConfirmed,   Action::None,      State::Recording(Trigger::Manual)),
            (State::Recording(Trigger::Manual), Event::UserDeclined,    Action::None,      State::Recording(Trigger::Manual)),
            (State::Recording(Trigger::Manual), Event::ManualStart,     Action::None,      State::Recording(Trigger::Manual)),
            (State::Recording(Trigger::Manual), Event::ManualStop,      Action::CloseFile, State::Finalizing),
            (State::Recording(Trigger::Manual), Event::FinalizeDone,    Action::None,      State::Recording(Trigger::Manual)),

            // ---- Finalizing ---- единственный легальный выход — FinalizeDone
            // (см. докблок State::Finalizing, Important 2 ревью). Всё
            // остальное — задокументированное застревание, не баг этого теста.
            (State::Finalizing, Event::SessionAppeared, Action::None, State::Finalizing),
            (State::Finalizing, Event::SessionGone,     Action::None, State::Finalizing),
            (State::Finalizing, Event::UserConfirmed,   Action::None, State::Finalizing),
            (State::Finalizing, Event::UserDeclined,    Action::None, State::Finalizing),
            (State::Finalizing, Event::ManualStart,     Action::None, State::Finalizing),
            (State::Finalizing, Event::ManualStop,      Action::None, State::Finalizing),
            (State::Finalizing, Event::FinalizeDone,    Action::None, State::Idle),
        ];

        assert_eq!(
            table.len(),
            35,
            "таблица должна покрывать все 5 состояний × 7 событий"
        );

        for &(from, event, expected_action, expected_to) in table {
            let mut m = SessionMachine { state: from };
            let action = m.handle(event);
            assert_eq!(
                action, expected_action,
                "{from:?} + {event:?}: ожидали действие {expected_action:?}, получили {action:?}"
            );
            assert_eq!(
                m.state(),
                expected_to,
                "{from:?} + {event:?}: ожидали состояние {expected_to:?}, получили {:?}",
                m.state()
            );
        }
    }
}
