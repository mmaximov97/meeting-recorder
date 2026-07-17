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
            (Armed, UserDeclined) | (Armed, SessionGone) => {
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
}
