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
}
