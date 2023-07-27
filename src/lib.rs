pub type SeqNum = u32;

pub trait ApplicationLayer: Sized {
    type RecvData;
    type RecvDataRef<'a>;
    type RecvReturn;

    type SendData;

    fn send(&self, data: &Self::SendData);
    fn send_ack(&self, reply_num: SeqNum);
    fn send_empty_reply(&self, reply_num: SeqNum);

    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a>;
    fn process(&self, reply_cx: ReplyGuard<'_, Self>, recv_packet: Self::RecvDataRef<'_>, send_data: Option<Self::SendData>) -> Self::RecvReturn;
}

pub trait IntoRecvData<App: ApplicationLayer>: Into<App::RecvData> {
    fn as_ref(&self) -> App::RecvDataRef<'_>;
}
impl<AppInner: ApplicationLayer> IntoRecvData<AppInner> for AppInner::RecvData {
    fn as_ref(&self) -> AppInner::RecvDataRef<'_> {
        AppInner::deserialize(self)
    }
}

pub struct SeqQueue<App: ApplicationLayer, const SLEN: usize = 64, const RLEN: usize = 32> {
    pub retry_interval: i64,
    next_send_seq_num: SeqNum,
    pre_recv_seq_num: SeqNum,
    send_window: [Option<SendEntry<App>>; SLEN],
    recv_window: [Option<RecvEntry<App>>; RLEN],
}

struct RecvEntry<App: ApplicationLayer> {
    seq_num: SeqNum,
    reply_num: Option<SeqNum>,
    data: App::RecvData,
}

struct SendEntry<App: ApplicationLayer> {
    seq_num: SeqNum,
    reply_num: Option<SeqNum>,
    next_resent_time: i64,
    data: App::SendData,
}

pub enum Error {
    OutOfSequence,
    QueueIsFull,
}

/// Whenever a packet is received, it must be replied to.
/// This Guard object guarantees that this is the case.
/// If it is dropped without calling `reply` an empty reply will be sent to the remote peer.
pub struct ReplyGuard<'a, App: ApplicationLayer> {
    app: Option<&'a App>,
    seq_queue: &'a mut SeqQueue<App>,
    reply_num: SeqNum,
}

pub struct Iter<'a, App: ApplicationLayer>(std::slice::Iter<'a, Option<SendEntry<App>>>);
pub struct IterMut<'a, App: ApplicationLayer>(std::slice::IterMut<'a, Option<SendEntry<App>>>);

impl<App: ApplicationLayer> SeqQueue<App> {
    pub fn new(retry_interval: i64, initial_seq_num: SeqNum) -> Self {
        Self {
            retry_interval,
            next_send_seq_num: initial_seq_num,
            pre_recv_seq_num: initial_seq_num.wrapping_sub(1),
            recv_window: std::array::from_fn(|_| None),
            send_window: std::array::from_fn(|_| None),
        }
    }

    pub fn is_full(&self) -> bool {
        self.send_window[self.next_send_seq_num as usize % self.send_window.len()].is_some()
    }
    /// If the return value is `false` the queue is full and the packet will not be sent.
    /// The caller must either cancel sending, abort the connection, or wait until a call to
    /// `receive` returns `Some` and try again.
    #[must_use = "The queue might be full causing the packet to not be sent"]
    pub fn send_seq(&mut self, app: App, create: impl FnOnce(SeqNum) -> App::SendData, current_time: i64) -> bool {
        let seq_num = self.next_send_seq_num;
        self.next_send_seq_num = self.next_send_seq_num.wrapping_add(1);

        let i = seq_num as usize % self.send_window.len();
        if self.send_window[i].is_some() {
            return false;
        }
        let next_resent_time = current_time + self.retry_interval;
        let entry = self.send_window[i].insert(SendEntry {
            seq_num,
            reply_num: None,
            next_resent_time,
            data: create(seq_num),
        });

        app.send(&entry.data);
        true
    }

    pub fn receive(
        &mut self,
        app: App,
        seq_num: SeqNum,
        reply_num: Option<SeqNum>,
        packet: impl IntoRecvData<App>,
    ) -> Result<App::RecvReturn, Error> {
        let normalized_seq_num = seq_num.wrapping_sub(self.pre_recv_seq_num).wrapping_sub(1);
        let is_below_range = normalized_seq_num > SeqNum::MAX / 2;
        let is_above_range = !is_below_range && normalized_seq_num >= self.recv_window.len() as u32;
        let is_next = normalized_seq_num == 0;
        if is_below_range {
            // Check whether or not we are already replying to this packet.
            for entry in self.send_window.iter() {
                if entry.as_ref().map_or(false, |e| e.reply_num == Some(seq_num)) {
                    return Err(Error::OutOfSequence);
                }
            }
            app.send_empty_reply(seq_num);
            return Err(Error::OutOfSequence);
        } else if is_above_range {
            return Err(Error::OutOfSequence);
        }
        if self.is_full() {
            return Err(Error::QueueIsFull);
        }
        let i = seq_num as usize % self.recv_window.len();
        if let Some(pre) = self.recv_window[i].as_mut() {
            if seq_num == pre.seq_num {
                if is_next {
                    self.recv_window[i] = None;
                } else {
                    app.send_ack(seq_num);
                    return Err(Error::OutOfSequence);
                }
            } else {
                return Err(Error::OutOfSequence);
            }
        }
        if is_next {
            // This should reliably handle sequence number overflow.
            self.pre_recv_seq_num = seq_num;
            let data = reply_num.and_then(|r| self.take_send(r));
            Ok(app.process(ReplyGuard { app: Some(&app), seq_queue: self, reply_num: seq_num }, packet.as_ref(), data))
        } else {
            self.recv_window[i] = Some(RecvEntry { seq_num, reply_num, data: packet.into() });
            if let Some(reply_num) = reply_num {
                self.receive_ack(reply_num);
            }
            app.send_ack(seq_num);
            Err(Error::OutOfSequence)
        }
    }

    fn take_send(&mut self, reply_num: SeqNum) -> Option<App::SendData> {
        let i = reply_num as usize % self.send_window.len();
        if self.send_window[i].as_ref().map_or(false, |e| e.seq_num == reply_num) {
            self.send_window[i].take().map(|e| e.data)
        } else {
            None
        }
    }
    pub fn pump(&mut self, app: App) -> Result<App::RecvReturn, Error> {
        let next_seq_num = self.pre_recv_seq_num.wrapping_add(1);
        let i = next_seq_num as usize % self.recv_window.len();

        if self.recv_window[i].as_ref().map_or(false, |pre| pre.seq_num == next_seq_num) {
            if self.is_full() {
                return Err(Error::QueueIsFull);
            }
            self.pre_recv_seq_num = next_seq_num;
            let entry = self.recv_window[i].take().unwrap();
            let data = entry.reply_num.and_then(|r| self.take_send(r));
            Ok(app.process(
                ReplyGuard { app: Some(&app), seq_queue: self, reply_num: entry.seq_num },
                App::deserialize(&entry.data),
                data,
            ))
        } else {
            Err(Error::OutOfSequence)
        }
    }

    pub fn receive_ack(&mut self, reply_num: SeqNum) {
        let i = reply_num as usize % self.send_window.len();
        if let Some(entry) = self.send_window[i].as_mut() {
            if entry.seq_num == reply_num {
                entry.next_resent_time = i64::MAX;
            }
        }
    }
    pub fn receive_empty_reply(&mut self, reply_num: SeqNum) -> Option<App::SendData> {
        let i = reply_num as usize % self.send_window.len();
        if self.send_window[i].as_ref().map_or(false, |e| e.seq_num == reply_num) {
            let entry = self.send_window[i].take().unwrap();
            Some(entry.data)
        } else {
            None
        }
    }

    pub fn service(&mut self, app: App, current_time: i64) -> i64 {
        let next_interval = current_time + self.retry_interval;
        let mut next_activity = next_interval;
        for item in self.send_window.iter_mut() {
            if let Some(entry) = item {
                if entry.next_resent_time <= current_time {
                    entry.next_resent_time = next_interval;
                    app.send(&entry.data);
                } else {
                    next_activity = next_activity.min(entry.next_resent_time);
                }
            }
        }
        next_activity - current_time
    }

    pub fn iter(&self) -> Iter<'_, App> {
        Iter(self.send_window.iter())
    }
    pub fn iter_mut(&mut self) -> IterMut<'_, App> {
        IterMut(self.send_window.iter_mut())
    }
}
impl<'a, App: ApplicationLayer> IntoIterator for &'a SeqQueue<App> {
    type Item = &'a App::SendData;
    type IntoIter = Iter<'a, App>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, App: ApplicationLayer> IntoIterator for &'a mut SeqQueue<App> {
    type Item = &'a mut App::SendData;
    type IntoIter = IterMut<'a, App>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'a, App: ApplicationLayer> ReplyGuard<'a, App> {
    pub fn seq_num(&self) -> SeqNum {
        self.seq_queue.next_send_seq_num
    }
    pub fn reply_num(&self) -> SeqNum {
        self.reply_num
    }
    pub fn reply(mut self, packet: App::SendData, current_time: i64) {
        if let Some(app) = self.app {
            let seq_queue = &mut self.seq_queue;
            let seq_num = seq_queue.next_send_seq_num;
            seq_queue.next_send_seq_num = seq_queue.next_send_seq_num.wrapping_add(1);

            let i = seq_num as usize % seq_queue.send_window.len();
            let next_resent_time = current_time + seq_queue.retry_interval;
            let entry = seq_queue.send_window[i].insert(SendEntry {
                seq_num,
                reply_num: Some(self.reply_num),
                next_resent_time,
                data: packet,
            });

            app.send(&entry.data);
            self.app = None;
        }
    }
}
impl<'a, App: ApplicationLayer> Drop for ReplyGuard<'a, App> {
    fn drop(&mut self) {
        if let Some(app) = self.app {
            app.send_empty_reply(self.reply_num);
        }
    }
}

macro_rules! iterator {
    ($iter:ident, {$( $mut:tt )?}) => {
        impl<'a, App: ApplicationLayer> Iterator for $iter<'a, App> {
            type Item = &'a $($mut)? App::SendData;
            fn next(&mut self) -> Option<Self::Item> {
                while let Some(entry) = self.0.next() {
                    if let Some(entry) = entry {
                        return Some(& $($mut)? entry.data)
                    }
                }
                None
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                (0, Some(self.0.len()))
            }
        }
        impl<'a, App: ApplicationLayer> DoubleEndedIterator for $iter<'a, App> {
            fn next_back(&mut self) -> Option<Self::Item> {
                while let Some(entry) = self.0.next_back() {
                    if let Some(entry) = entry {
                        return Some(& $($mut)? entry.data)
                    }
                }
                None
            }
        }
    }
}
iterator!(Iter, {});
iterator!(IterMut, {mut});
