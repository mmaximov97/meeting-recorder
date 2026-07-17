use std::collections::VecDeque;

/// Кольцевой буфер семплов. Держит последние N семплов; старое вытесняется.
/// Нужен, чтобы не потерять начало встречи, пока пользователь думает над тостом.
pub struct RingBuffer {
    buf: VecDeque<i16>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity_samples),
            capacity: capacity_samples,
        }
    }

    pub fn push_slice(&mut self, samples: &[i16]) {
        // Кусок длиннее ёмкости: интересен только его хвост.
        let tail = if samples.len() > self.capacity {
            &samples[samples.len() - self.capacity..]
        } else {
            samples
        };
        for &s in tail {
            if self.buf.len() == self.capacity {
                self.buf.pop_front();
            }
            self.buf.push_back(s);
        }
    }

    pub fn drain_to_vec(&mut self) -> Vec<i16> {
        self.buf.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn пустой_буфер_отдаёт_пусто() {
        let mut b = RingBuffer::new(4);
        assert_eq!(b.drain_to_vec(), Vec::<i16>::new());
    }

    #[test]
    fn отдаёт_то_что_положили_если_влезло() {
        let mut b = RingBuffer::new(4);
        b.push_slice(&[1, 2, 3]);
        assert_eq!(b.drain_to_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn вытесняет_старое_при_переполнении() {
        let mut b = RingBuffer::new(3);
        b.push_slice(&[1, 2, 3, 4, 5]);
        // влезают только последние 3
        assert_eq!(b.drain_to_vec(), vec![3, 4, 5]);
    }

    #[test]
    fn кусок_длиннее_ёмкости_не_ломает_буфер() {
        let mut b = RingBuffer::new(2);
        b.push_slice(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(b.drain_to_vec(), vec![6, 7]);
    }

    #[test]
    fn drain_опустошает() {
        let mut b = RingBuffer::new(4);
        b.push_slice(&[1, 2]);
        let _ = b.drain_to_vec();
        assert_eq!(b.len(), 0);
        assert_eq!(b.drain_to_vec(), Vec::<i16>::new());
    }

    // Тесты ниже закрывают находку ревью: 5 тестов выше вытесняют старое
    // только через предварительную обрезку среза в начале push_slice
    // (samples.len() > capacity), потому что весь избыток приходит одним
    // вызовом в пустой буфер. Ветка `if self.buf.len() == self.capacity`
    // внутри цикла — та, что реально работает в бою при накоплении аудио
    // через много мелких push_slice — там ни разу не срабатывает.
    //
    // Ключевое условие каждого теста ниже: длина каждого отдельного среза,
    // передаваемого в push_slice, НЕ превышает capacity. Значит ветка
    // предварительной обрезки (samples.len() > self.capacity) не может
    // сработать ни разу. VecDeque не имеет собственного ограничения на
    // размер — если бы `pop_front` внутри цикла не выполнялся, буфер
    // просто рос бы сверх capacity и хранил бы лишние старые семплы.
    // Поэтому то, что итоговая длина остаётся равна capacity, а старые
    // семплы вытеснены — прямое доказательство, что именно эта ветка
    // отработала.

    #[test]
    fn вытесняет_старое_накоплением_по_одному_семплу() {
        let mut b = RingBuffer::new(3);
        // Каждый push_slice несёт один семпл — длина среза (1) всегда
        // меньше capacity (3), предварительная обрезка не задействуется.
        b.push_slice(&[1]);
        assert_eq!(b.len(), 1);
        b.push_slice(&[2]);
        assert_eq!(b.len(), 2);
        b.push_slice(&[3]);
        // буфер заполнен ровно до ёмкости, вытеснения ещё не было
        assert_eq!(b.len(), 3);
        b.push_slice(&[4]);
        // если бы pop_front не сработал, длина стала бы 4 (VecDeque сама
        // размер не ограничивает) — а она осталась равна capacity
        assert_eq!(b.len(), 3);
        assert_eq!(b.drain_to_vec(), vec![2, 3, 4]);
    }

    #[test]
    fn вытесняет_старое_накоплением_мелкими_чанками() {
        let mut b = RingBuffer::new(5);
        // Три чанка по несколько семплов, каждый короче capacity (5):
        // 2, 2 и 3 семпла — суммарно 7, что больше ёмкости.
        b.push_slice(&[1, 2]);
        assert_eq!(b.len(), 2);
        b.push_slice(&[3, 4]);
        assert_eq!(b.len(), 4);
        b.push_slice(&[5, 6, 7]);
        // без pop_front длина была бы 4 + 3 = 7; вместо этого она
        // ограничена ёмкостью — значит вытеснение внутри цикла отработало
        // не один раз, а дважды за этот вызов (для 6 и для 7)
        assert_eq!(b.len(), 5);
        assert_eq!(b.drain_to_vec(), vec![3, 4, 5, 6, 7]);
    }

    #[test]
    fn вытесняет_старое_из_непустого_буфера_при_большом_куске() {
        // Ориентир из ревью: непустой буфер [9] + push_slice длиннее
        // capacity. Предварительная обрезка отдаёт хвост [2, 3, 4], но
        // класть его приходится поверх уже занятой ячейки (9), поэтому
        // pop_front внутри цикла всё равно срабатывает — на этот раз в
        // паре с обрезкой среза, а не вместо неё.
        let mut b = RingBuffer::new(3);
        b.push_slice(&[9]);
        assert_eq!(b.len(), 1);
        b.push_slice(&[1, 2, 3, 4]);
        assert_eq!(b.drain_to_vec(), vec![2, 3, 4]);
    }

    #[test]
    fn нулевая_ёмкость_всегда_пуста_и_не_паникует() {
        let mut b = RingBuffer::new(0);
        assert!(b.is_empty());

        b.push_slice(&[]);
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);

        // короткий срез (короче "ёмкости" не бывает, ёмкость — 0)
        b.push_slice(&[1]);
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);

        // срез заметно длиннее нулевой ёмкости
        b.push_slice(&[1, 2, 3, 4, 5]);
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
        assert_eq!(b.drain_to_vec(), Vec::<i16>::new());
    }

    #[test]
    fn вход_ровно_равен_ёмкости_вытеснения_нет() {
        let mut b = RingBuffer::new(4);
        b.push_slice(&[1, 2, 3, 4]);
        // capacity достигается только на последнем элементе среза —
        // pop_front ни разу не должен был сработать, весь вход цел
        assert_eq!(b.len(), 4);
        assert_eq!(b.drain_to_vec(), vec![1, 2, 3, 4]);
    }
}
