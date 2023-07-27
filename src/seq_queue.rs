
use crate::SeqNum;

const INITIAL_SEQ_NUM: SeqNum = 1;
const SEQ_RECV_WINDOW_LEN: usize = 32;
const SEQ_SEND_WINDOW_LEN: usize = 64;

pub trait ApplicationLayer: Sized {
    type RecvData;
    type RecvDataRef<'a>;
    type RecvReturn;

    type SendData;

    fn send(&self, data: &Self::SendData);
    fn send_ack(&self, reply_num: SeqNum);
    fn send_empty_reply(&self, reply_num: SeqNum);

    fn deserialize<'a>(data: &'a Self::RecvData) -> Self::RecvDataRef<'a>;
    fn process(
        &self,
        packet: Self::RecvDataRef<'_>,
        reply: ReplyGuard<'_, Self>,
        send_data: Option<Self::SendData>,
    ) -> Self::RecvReturn;
}

pub trait IntoRecvData<App: ApplicationLayer>: Into<App::RecvData> {
    fn as_ref(&self) -> App::RecvDataRef<'_>;
}
impl<AppInner: ApplicationLayer> IntoRecvData<AppInner> for AppInner::RecvData {
    fn as_ref(&self) -> AppInner::RecvDataRef<'_> {
        AppInner::deserialize(self)
    }
}


pub struct SeqQueue<App: ApplicationLayer> {
    pub retry_interval: i64,
    next_send_seq_num: SeqNum,
    pre_recv_seq_num: SeqNum,
    recv_window: [Option<RecvEntry<App>>; SEQ_RECV_WINDOW_LEN],
    send_window: [Option<SendEntry<App>>; SEQ_SEND_WINDOW_LEN],
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

/// Whenever a packet is received, it must be replied to.
/// This Guard object guarantees that this is the case.
/// If it is dropped without calling `reply` an empty reply will be sent to the remote peer.
pub struct ReplyGuard<'a, App: ApplicationLayer> {
    app: Option<&'a App>,
    seq_queue: &'a mut SeqQueue<App>,
    reply_num: SeqNum,
}

impl<App: ApplicationLayer> SeqQueue<App> {
    pub fn new(retry_interval: i64) -> Self {
        Self {
            retry_interval,
            next_send_seq_num: INITIAL_SEQ_NUM,
            pre_recv_seq_num: INITIAL_SEQ_NUM - 1,
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
    pub fn send_seq(
        &mut self,
        app: App,
        create: impl FnOnce(SeqNum) -> App::SendData,
        current_time: i64,
    ) -> bool {
        let seq_num = self.next_send_seq_num;
        self.next_send_seq_num += 1;

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
    ) -> Option<App::RecvReturn> {
        let normalized_seq_num = seq_num.wrapping_sub(self.pre_recv_seq_num).wrapping_sub(1);
        let is_below_range = normalized_seq_num > SeqNum::MAX / 2;
        let is_above_range = !is_below_range && normalized_seq_num >= self.recv_window.len() as u32;
        let is_next = normalized_seq_num == 0;
        if is_below_range {
            // Check whether or not we are already replying to this packet.
            for entry in self.send_window.iter() {
                if entry.as_ref().map_or(false, |e| e.reply_num == Some(seq_num)) {
                    return None;
                }
            }
            app.send_empty_reply(seq_num);
            return None;
        } else if is_above_range {
            return None;
        }
        let i = seq_num as usize % self.recv_window.len();
        if let Some(pre) = self.recv_window[i].as_mut() {
            if seq_num == pre.seq_num {
                if is_next {
                    self.recv_window[i] = None;
                } else {
                    app.send_ack(seq_num);
                    return None;
                }
            } else {
                return None;
            }
        }
        if is_next {
            // This should reliably handle sequence number overflow.
            self.pre_recv_seq_num = seq_num;
            let data = reply_num.and_then(|r| self.take_send(r));
            Some(app.process(
                packet.as_ref(),
                ReplyGuard { app: Some(&app), seq_queue: self, reply_num: seq_num },
                data,
            ))
        } else {
            self.recv_window[i] = Some(RecvEntry {
                seq_num,
                reply_num,
                data: packet.into(),
            });
            if let Some(reply_num) = reply_num {
                self.receive_ack(reply_num);
            }
            app.send_ack(seq_num);
            None
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
    pub fn pump(&mut self, app: App) -> Option<App::RecvReturn> {
        let next_seq_num = self.pre_recv_seq_num.wrapping_add(1);
        let i = next_seq_num as usize % self.recv_window.len();

        if self.recv_window[i].as_ref().map_or(false, |pre| pre.seq_num == next_seq_num) {
            self.pre_recv_seq_num = next_seq_num;
            let entry = self.recv_window[i].take().unwrap();
            let data = entry.reply_num.and_then(|r| self.take_send(r));
            Some(app.process(
                App::deserialize(&entry.data),
                ReplyGuard { app: Some(&app), seq_queue: self, reply_num: entry.seq_num },
                data,
            ))
        } else {
            None
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

    pub fn window_iter(&self) -> Iter<'_, App> {
        Iter(self.send_window.iter())
    }
    pub fn window_iter_mut(&mut self) -> IterMut<'_, App> {
        IterMut(self.send_window.iter_mut())
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

pub struct Iter<'a, App: ApplicationLayer> (std::slice::Iter<'a, Option<SendEntry<App>>>);
pub struct IterMut<'a, App: ApplicationLayer> (std::slice::IterMut<'a, Option<SendEntry<App>>>);

iterator!(Iter, { });
iterator!(IterMut, {mut});

impl<'a, App: ApplicationLayer> ReplyGuard<'a, App> {
    pub fn is_full(&self) -> bool {
        let seq_queue = &self.seq_queue;
        seq_queue.send_window[seq_queue.next_send_seq_num as usize % seq_queue.send_window.len()].is_some()
    }
    /// If the return value is `false` the queue is full and the packet will not be sent.
    /// The caller must either cancel the reply or abort the connection.
    /// If the reply is cancelled then the remote peer will receive an empty reply instead.
    ///
    /// A packet can only be replied to once. Once a call to `reply` is successful and returns `true`,
    /// all subsequent calls will return `false`.
    #[must_use = "The queue might be full causing the packet to not be sent"]
    pub fn reply(
        &mut self,
        create: impl FnOnce(SeqNum, SeqNum) -> App::SendData,
        current_time: i64,
    ) -> bool {
        if let Some(app) = self.app {
            let seq_queue = &mut self.seq_queue;
            let seq_num = seq_queue.next_send_seq_num;
            seq_queue.next_send_seq_num += 1;

            let i = seq_num as usize % seq_queue.send_window.len();
            if seq_queue.send_window[i].is_some() {
                return false;
            }
            let next_resent_time = current_time + seq_queue.retry_interval;
            let entry = seq_queue.send_window[i].insert(SendEntry {
                seq_num,
                reply_num: Some(self.reply_num),
                next_resent_time,
                data: create(seq_num, self.reply_num),
            });

            app.send(&entry.data);
            self.app = None;
            true
        } else {
            false
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
