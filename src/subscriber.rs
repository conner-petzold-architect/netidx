use crate::{
    path::Path,
    utils::{BatchItem, Batched},
    resolver::{Resolver, ReadOnly},
    channel::{Channel, ReadChannel},
    protocol::publisher::{self, Id},
    config,
};
use std::{
    mem, iter, io,
    result::Result,
    marker::PhantomData,
    collections::{HashMap, hash_map::Entry},
    net::SocketAddr,
    sync::{Arc, Weak},
    cmp::max,
    time::Duration,
};
use tokio::{
    task,
    sync::oneshot,
    net::TcpStream,
    time::{self, Instant, Delay},
};
use fxhash::FxBuildHasher;
use futures::{
    prelude::*,
    select,
    channel::mpsc::{self, Sender, UnboundedReceiver, UnboundedSender},
    future::Fuse,
};
use rand::Rng;
use serde::de::DeserializeOwned;
use failure::Error;
use bytes::{Bytes, Buf};
use parking_lot::Mutex;

const BATCH: usize = 100_000;

#[derive(Debug)]
struct SubscribeValRequest {
    path: Path,
    finished: oneshot::Sender<Result<UVal, Error>>,
    con: UnboundedSender<ToCon>,
    deadline: Option<Instant>,
}

#[derive(Debug)]
enum ToCon {
    Subscribe(SubscribeValRequest),
    Unsubscribe(Id),
    Last(Id, oneshot::Sender<Bytes>),
    Stream {
        id: Id,
        tx: Sender<Bytes>,
        last: bool,
    }
}

#[derive(Debug)]
struct UValInner {
    id: Id,
    addr: SocketAddr,
    connection: UnboundedSender<ToCon>,
}

impl Drop for UValInner {
    fn drop(&mut self) {
        let _ = self.connection.send(ToCon::Unsubscribe(self.id));
    }
}

#[derive(Debug, Clone)]
pub struct UValWeak(Weak<UValInner>);

impl UValWeak {
    pub fn upgrade(&self) -> Option<UVal> {
        Weak::upgrade(&self.0).map(|r| UVal(r))
    }
}

/// A UVal is an untyped subscription to a value. A value has a
/// current value, and a stream of updates. The current value is
/// accessed by `UVal::last`, which will be available as long as the
/// subscription is alive. The stream of updates is accessed with the
/// `UVal::updates` function.
#[derive(Debug, Clone)]
pub struct UVal(Arc<UValInner>);

impl UVal {
    pub fn typed<T: DeserializeOwned>(self) -> Val<T> {
        Val(self, PhantomData)
    }

    pub fn downgrade(&self) -> UValWeak {
        UValWeak(Arc::downgrade(&self.0))
    }

    /// Get the last published value, or None if the subscription is
    /// dead.
    pub async fn last(&self) -> Option<Bytes> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.connection.unbounded_send(ToCon::Last(self.0.id, tx));
        match rx.await {
            Ok(b) => Some(b),
            Err(_) => None
        }
    }

    /// Get a stream of published values. Values will arrive in the
    /// order they are published. No value will be omitted. If
    /// `begin_with_last` is true, then the stream will start with the
    /// last published value at the time `updates` is called, and will
    /// then receive any updated values. Otherwise the stream will
    /// only receive new values.
    ///
    /// If the subscription dies the stream will end.
    pub fn updates(
        &self,
        begin_with_last: bool
    ) -> impl Stream<Item = Bytes> {
        let (tx, rx) = mpsc::channel(10);
        let m = ToCon::Stream { tx, last: begin_with_last, id: self.0.id };
        let _ = self.0.connection.unbounded_send(m);
        rx
    }
}

/// A typed version of UVal
#[derive(Debug, Clone)]
pub struct Val<T: DeserializeOwned>(UVal, PhantomData<T>);

impl<T: DeserializeOwned> Val<T> {
    pub fn untyped(self) -> UVal {
        self.0
    }

    /// Get and decode the last published value, or None if the
    /// subscription is dead.
    pub async fn last(&self) -> Option<Result<T, rmp_serde::decode::Error>> {
        self.0.last().await.map(|v| Ok(rmp_serde::decode::from_read(&*v)?))
    }

    /// Same as `SubscriptionUt::updates` but it decodes the value
    pub fn updates(
        &self,
        begin_with_last: bool
    ) -> impl Stream<Item = Result<T, rmp_serde::decode::Error>> {
        self.0.updates(begin_with_last).map(|v| rmp_serde::decode::from_read(&*v))
    }
}

#[derive(Debug, Copy, Clone)]
pub enum DVState {
    Subscribed,
    Unsubscribed,
}

#[derive(Debug)]
struct DUValInner {
    sub: Option<UVal>,
    streams: Vec<Sender<Bytes>>,
    states: Vec<UnboundedSender<DVState>>,
    tries: usize,
    next_try: Instant,
}

#[derive(Debug, Clone)]
struct DUValWeak(Weak<Mutex<DUValInner>>);

impl DUValWeak {
    fn upgrade(&self) -> Option<DUVal> {
        Weak::upgrade(&self.0).map(|s| DUVal(s))
    }
}

/// `DUVal` is a durable value subscription. it behaves just like
/// `UVal`, except that if it dies a task within subscriber will
/// attempt to resubscribe. The resubscription process goes through
/// the entire resolution and connection process again, so `DUVal` is
/// robust to many failures. For example,
/// 
/// - multiple publishers are publishing on a path and one of them dies.
///   `DUVal` will transparently move to another one.
///
/// - a publisher is restarted (possibly on a different machine).
///   Since `DUVal` uses linear backoff to avoid saturating the
///   resolver, and the network, but assuming the publisher is restarted
///   quickly, resubscription will happen almost immediatly.
///
/// - The resolver server cluster is restarted. In this case existing
///   subscriptions won't die, but new ones will fail if the new
///   cluster is missing data. However even in this very bad case the
///   publishers will notice during their heartbeat (default every 10
///   minutes) that the resolver server is missing their data, and
///   they will republish it. If the resolver is restarted quickly,
///   then in the worst case all the data is back in 10 minutes, and
///   all DUVals waiting to subscribe to missing data will retry and
///   succeed 35 seconds after that.
///
/// A `DUVal` uses more memory than a `UVal` subscription, but other
/// than that the performance is the same. It is therefore recommended
/// that `DUVal` and `DVal` be considered the default value
/// subscription type where semantics allow.
#[derive(Debug, Clone)]
pub struct DUVal(Arc<Mutex<DUValInner>>);

impl DUVal {
    pub fn typed<T: DeserializeOwned>(self) -> DVal<T> {
        DVal(self, PhantomData)
    }

    fn downgrade(&self) -> DUValWeak {
        DUValWeak(Arc::downgrade(&self.0))
    }

    /// Get the last value published by the publisher, or None if the
    /// subscription is currently dead.
    pub async fn last(&self) -> Option<Bytes> {
        let sub = self.0.lock().sub.clone();
        match sub {
            None => None,
            Some(sub) => sub.last().await
        }
    }

    /// Return a stream that produces a value when the state of the
    /// subscription changes. if include_current is true, then the
    /// current state will be immediatly emitted once even if there
    /// was no state change.
    pub fn state_updates(&self, include_current: bool) -> impl Stream<Item = DVState> {
        let (tx, rx) = mpsc::unbounded();
        let mut t = self.0.lock();
        t.states.retain(|c| !c.is_closed());
        if include_current {
            let current = match t.sub {
                None => DVState::Unsubscribed,
                Some(_) => DVState::Subscribed,
            };
            let _ = tx.unbounded_send(current);
        }
        t.states.push(tx);
        rx
    }

    pub fn state(&self) -> DVState {
        if self.0.lock().sub.is_none() {
            DVState::Unsubscribed
        } else {
            DVState::Subscribed
        }
    }

    /// Gets a stream of updates just like `UVal::updates` except that
    /// the stream will not end when the subscription dies, it will
    /// just stop producing values, and will start again if
    /// resubscription is successful.
    pub fn updates(
        &self,
        begin_with_last: bool
    ) -> impl Stream<Item = Bytes> {
        let mut t = self.0.lock();
        let (tx, rx) = mpsc::channel(10);
        t.streams.retain(|c| !c.is_closed());
        t.streams.push(tx.clone());
        if let Some(ref sub) = t.sub {
            let m = ToCon::Stream {tx, last: begin_with_last, id: sub.0.id };
            let _ = sub.0.connection.unbounded_send(m);
        }
        rx
    }
}

/// A typed version of `DUVal`
#[derive(Debug, Clone)]
pub struct DVal<T: DeserializeOwned>(DUVal, PhantomData<T>);

impl<T: DeserializeOwned> DVal<T> {
    pub fn untyped(self) -> DUVal {
        self.0
    }

    pub async fn last(&self) -> Option<Result<T, rmp_serde::decode::Error>> {
        self.0.last().await.map(|v| Ok(rmp_serde::decode::from_read(&*v)?))
    }

    pub fn state_updates(&self, include_current: bool) -> impl Stream<Item = DVState> {
        self.0.state_updates(include_current)
    }

    pub fn state(&self) -> DVState {
        self.0.state()
    }

    pub fn updates(
        &self,
        begin_with_last: bool
    ) -> impl Stream<Item = Result<T, rmp_serde::decode::Error>> {
        self.0.updates(begin_with_last).map(|v| rmp_serde::decode::from_read(&*v))
    }
}

enum SubStatus {
    Subscribed(UValWeak),
    Pending(Vec<oneshot::Sender<Result<UVal, Error>>>),
}

struct SubscriberInner {
    resolver: Resolver<ReadOnly>,
    connections: HashMap<SocketAddr, UnboundedSender<ToCon>, FxBuildHasher>,
    subscribed: HashMap<Path, SubStatus>,
    durable_dead: HashMap<Path, DUValWeak>,
    durable_alive: HashMap<Path, DUValWeak>,
    trigger_resub: UnboundedSender<()>,
}

struct SubscriberWeak(Weak<Mutex<SubscriberInner>>);

impl SubscriberWeak {
    fn upgrade(&self) -> Option<Subscriber> {
        Weak::upgrade(&self.0).map(|s| Subscriber(s))
    }
}

#[derive(Clone)]
pub struct Subscriber(Arc<Mutex<SubscriberInner>>);

impl Subscriber {
    pub fn new(resolver: config::Resolver) -> Result<Subscriber, Error> {
        let (tx, rx) = mpsc::unbounded();
        let t = Subscriber(Arc::new(Mutex::new(SubscriberInner {
            resolver: Resolver::<ReadOnly>::new_r(resolver)?,
            connections: HashMap::with_hasher(FxBuildHasher::default()),
            subscribed: HashMap::new(),
            durable_dead: HashMap::new(),
            durable_alive: HashMap::new(),
            trigger_resub: tx,
        })));
        t.start_resub_task(rx);
        Ok(t)
    }

    fn downgrade(&self) -> SubscriberWeak {
        SubscriberWeak(Arc::downgrade(&self.0))
    }

    fn start_resub_task(&self, incoming: UnboundedReceiver<()>) {
        async fn wait_retry(retry: &mut Option<Delay>) {
            match retry {
                None => future::pending().await,
                Some(d) => d.await,
            }
        }
        fn update_retry(subscriber: &mut SubscriberInner, retry: &mut Option<Delay>) {
            *retry = subscriber.durable_dead.values()
                .filter_map(|w| w.upgrade())
                .map(|ds| ds.0.lock().next_try)
                .fold(None, |min, v| match min {
                    None => Some(v),
                    Some(min) => if v < min {
                        Some(v)
                    } else {
                        Some(min)
                    }
                })
                .map(|t| time::delay_until(t + Duration::from_secs(1)));
        }
        async fn do_resub(subscriber: &SubscriberWeak, retry: &mut Option<Delay>) {
            if let Some(subscriber) = subscriber.upgrade() {
                let now = Instant::now();
                let (mut batch, timeout) = {
                    let mut b = HashMap::new();
                    let mut gc = Vec::new();
                    let mut subscriber = subscriber.0.lock();
                    let mut max_tries = 0;
                    for (p, w) in &subscriber.durable_dead {
                        match w.upgrade() {
                            None => { gc.push(p.clone()); }
                            Some(s) => {
                                let (next_try, tries) = {
                                    let s = s.0.lock();
                                    (s.next_try, s.tries)
                                };
                                if next_try <= now {
                                    b.insert(p.clone(), s);
                                    max_tries = max(max_tries, tries);
                                }
                            }
                        }
                    }
                    for p in gc {
                        subscriber.durable_dead.remove(&p);
                    }
                    (b, Duration::from_secs(10 + max_tries as u64))
                };
                if batch.len() == 0 {
                    let mut subscriber = subscriber.0.lock();
                    update_retry(&mut *subscriber, retry);
                } else {
                    let r = subscriber.subscribe_vals_ut(
                        batch.keys().cloned(),
                        Some(timeout)
                    ).await;
                    let mut subscriber = subscriber.0.lock();
                    let now = Instant::now();
                    for (p, r) in r {
                        let mut ds = batch.get_mut(&p).unwrap().0.lock();
                        match r {
                            Err(_) => { // CR estokes: log this error?
                                ds.tries += 1;
                                ds.next_try = now + Duration::from_secs(ds.tries as u64);
                            },
                            Ok(sub) => {
                                ds.tries = 0;
                                let mut i = 0;
                                while i < ds.states.len() {
                                    match
                                        ds.states[i].unbounded_send(DVState::Subscribed)
                                    {
                                        Ok(()) => { i += 1; }
                                        Err(_) => { ds.states.remove(i); }
                                    }
                                }
                                ds.streams.retain(|c| !c.is_closed());
                                for tx in ds.streams.iter().cloned() {
                                    let _ =
                                        sub.0.connection.unbounded_send(ToCon::Stream {
                                        tx, last: true, id: sub.0.id
                                    });
                                }
                                ds.sub = Some(sub);
                                let w = subscriber.durable_dead.remove(&p).unwrap();
                                subscriber.durable_alive.insert(p.clone(), w.clone());
                            },
                        }
                    }
                    update_retry(&mut *subscriber, retry);
                }
            }
        }
        let subscriber = self.downgrade();
        task::spawn(async move {
            let mut incoming = Batched::new(incoming, 100_000);
            let mut retry: Option<Delay> = None;
            loop {
                select! {
                    _ = wait_retry(&mut retry).fuse() => {
                        do_resub(&subscriber, &mut retry).await;
                    },
                    m = incoming.next() => match m {
                        None => break,
                        Some(BatchItem::InBatch(())) => (),
                        Some(BatchItem::EndBatch) => {
                            do_resub(&subscriber, &mut retry).await;
                        }
                    },
                }
            }
        });
    }
    
    /// Subscribe to the specified set of values.
    ///
    /// Path resolution and subscription are done in parallel, so the
    /// lowest latency per subscription will be achieved with larger
    /// batches.
    ///
    /// In case you are already subscribed to one or more of the paths
    /// in the batch, you will receive a reference to the existing
    /// subscription, no additional messages will be sent.
    ///
    /// It is safe to call this function concurrently with the same or
    /// overlapping sets of paths in the batch, only one subscription
    /// attempt will be made concurrently, and the result of that one
    /// attempt will be given to each concurrent caller upon success
    /// or failure.
    ///
    /// The timeout, if specified, will apply to each subscription
    /// individually. Any subscription that does not complete
    /// successfully before the specified timeout will result in an
    /// error, but that error will not effect other subscriptions in
    /// the batch, which may complete successfully. If you need all or
    /// nothing behavior, specify None for timeout and wrap the
    /// `subscribe_vals_ut` future in a `time::timeout`. In the case
    /// timeout is specified, `subscribe_vals_ut` is guaranteed to
    /// complete no later than `now + timeout`.
    pub async fn subscribe_vals_ut(
        &self, batch: impl IntoIterator<Item = Path>, timeout: Option<Duration>
    ) -> Vec<(Path, Result<UVal, Error>)> {
        enum St {
            Resolve,
            Subscribing(oneshot::Receiver<Result<UVal, Error>>),
            WaitingOther(oneshot::Receiver<Result<UVal, Error>>),
            Subscribed(UVal),
            Error(Error),
        }
        let now = Instant::now();
        let paths = batch.into_iter().collect::<Vec<_>>();
        let mut pending: HashMap<Path, St> = HashMap::new();
        let mut r = { // Init
            let mut t = self.0.lock();
            for p in paths.clone() {
                match t.subscribed.entry(p.clone()) {
                    Entry::Vacant(e) => {
                        e.insert(SubStatus::Pending(vec![]));
                        pending.insert(p, St::Resolve);
                    }
                    Entry::Occupied(mut e) => match e.get_mut() {
                        SubStatus::Pending(ref mut v) => {
                            let (tx, rx) = oneshot::channel();
                            v.push(tx);
                            pending.insert(p, St::WaitingOther(rx));
                        }
                        SubStatus::Subscribed(r) => match r.upgrade() {
                            Some(r) => { pending.insert(p, St::Subscribed(r)); }
                            None => {
                                e.insert(SubStatus::Pending(vec![]));
                                pending.insert(p, St::Resolve);
                            }
                        },
                    }
                }
            }
            t.resolver.clone()
        };
        fn pick(n: usize) -> usize {
            let mut rng = rand::thread_rng();
            rng.gen_range(0, n)
        }
        { // Resolve, Connect, Subscribe
            let to_resolve =
                pending.iter()
                .filter(|(_, s)| match s { St::Resolve => true, _ => false })
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>();
            let r = match timeout {
                None => Ok(r.resolve(to_resolve.clone()).await),
                Some(d) => time::timeout(d, r.resolve(to_resolve.clone())).await
            };
            match r {
                Err(_) => for p in to_resolve {
                    pending.insert(p.clone(), St::Error(
                        format_err!("resolving path: {} failed: request timed out", p)
                    ));
                },
                Ok(Err(e)) => for p in to_resolve {
                    pending.insert(p.clone(), St::Error(
                        format_err!("resolving path: {} failed: {}", p, e)
                    ));
                }
                Ok(Ok(addrs)) => {
                    let mut t = self.0.lock();
                    let deadline = timeout.map(|t| now + t);
                    for (p, addrs) in to_resolve.into_iter().zip(addrs.into_iter()) {
                        if addrs.len() == 0 {
                            pending.insert(p, St::Error(format_err!("path not found")));
                        } else {
                            let addr = {
                                if addrs.len() == 1 {
                                    addrs[0]
                                } else {
                                    addrs[pick(addrs.len())]
                                }
                            };
                            let con =
                                t.connections.entry(addr)
                                .or_insert_with(|| {
                                    let (tx, rx) = mpsc::unbounded();
                                    task::spawn(connection(self.downgrade(), addr, rx));
                                    tx
                                });
                            let (tx, rx) = oneshot::channel();
                            let con_ = con.clone();
                            let r =
                                con.unbounded_send(ToCon::Subscribe(SubscribeValRequest {
                                    con: con_,
                                    path: p.clone(),
                                    finished: tx,
                                    deadline,
                                }));
                            match r {
                                Ok(()) => { pending.insert(p, St::Subscribing(rx)); }
                                Err(e) => {
                                    pending.insert(p, St::Error(Error::from(e)));
                                }
                            }
                        }
                    }
                }
            }
        }
        // Wait
        for (path, st) in pending.iter_mut() {
            match st {
                St::Resolve => unreachable!(),
                St::Subscribed(_) => (),
                St::Error(e) => {
                    let mut t = self.0.lock();
                    if let Some(sub) = t.subscribed.remove(path.as_ref()) {
                        match sub {
                            SubStatus::Subscribed(_) => unreachable!(),
                            SubStatus::Pending(waiters) => {
                                for w in waiters {
                                    let err = Err(format_err!("{}", e));
                                    let _ = w.send(err);
                                }
                            }
                        }
                    }
                },
                St::WaitingOther(w) => match w.await {
                    Err(_) => *st = St::Error(format_err!("other side died")),
                    Ok(Err(e)) => *st = St::Error(e),
                    Ok(Ok(raw)) => *st = St::Subscribed(raw),
                }
                St::Subscribing(w) => {
                    let res = match w.await {
                        Err(_) => Err(format_err!("connection died")),
                        Ok(Err(e)) => Err(e),
                        Ok(Ok(raw)) => Ok(raw),
                    };
                    let mut t = self.0.lock();
                    match t.subscribed.entry(path.clone()) {
                        Entry::Vacant(_) => unreachable!(),
                        Entry::Occupied(mut e) => match res {
                            Err(err) => match e.remove() {
                                SubStatus::Subscribed(_) => unreachable!(),
                                SubStatus::Pending(waiters) => {
                                    for w in waiters {
                                        let err = Err(format_err!("{}", err));
                                        let _ = w.send(err);
                                    }
                                    *st = St::Error(err);
                                }
                            }
                            Ok(raw) => {
                                let s = mem::replace(
                                    e.get_mut(),
                                    SubStatus::Subscribed(raw.downgrade())
                                );
                                match s {
                                    SubStatus::Subscribed(_) => unreachable!(),
                                    SubStatus::Pending(waiters) => {
                                        for w in waiters {
                                            let _ = w.send(Ok(raw.clone()));
                                        }
                                        *st = St::Subscribed(raw);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        paths.into_iter().map(|p| match pending.remove(&p).unwrap() {
            St::Resolve | St::Subscribing(_) | St::WaitingOther(_) => unreachable!(),
            St::Subscribed(raw) => (p, Ok(raw)),
            St::Error(e) => (p, Err(e))
        }).collect()
    }

    /// Subscribe to one value. This is sufficient for a small number
    /// of paths, but if you need to subscribe to a lot of values it
    /// is more efficent to use `subscribe_vals_ut`
    pub async fn subscribe_val_ut(
        &self,
        path: Path,
        timeout: Option<Duration>
    ) -> Result<UVal, Error> {
        self.subscribe_vals_ut(iter::once(path), timeout).await.pop().unwrap().1
    }

    /// Same as `subscribe_vals_ut`, but typed.
    pub async fn subscribe_vals<T: DeserializeOwned>(
        &self, batch: impl IntoIterator<Item = Path>, timeout: Option<Duration>
    ) -> Vec<(Path, Result<Val<T>, Error>)> {
        self.subscribe_vals_ut(batch, timeout).await.into_iter().map(|(p, r)| {
            (p, r.map(|r| r.typed()))
        }).collect()
    }

    /// Same as `subscribe_val_ut` but typed.
    pub async fn subscribe_val<T: DeserializeOwned>(
        &self,
        path: Path,
        timeout: Option<Duration>
    ) -> Result<Val<T>, Error> {
        self.subscribe_val_ut(path, timeout).await.map(|v| v.typed())
    }

    /// Create a durable untyped value subscription to `path`.
    ///
    /// Batching of durable subscriptions is automatic, if you create
    /// a lot of durable subscriptions all at once they will batch,
    /// minimizing the number of messages exchanged with both the
    /// resolver server and the publishers.
    ///
    /// As with regular subscriptions there is only ever one
    /// subscription for a given path, calling
    /// `subscribe_val_durable_ut` again for the same path will just
    /// return another pointer to it.
    pub fn durable_subscribe_val_ut(&self, path: Path) -> DUVal {
        let mut t = self.0.lock();
        if let Some(s) =
            t.durable_dead.get(&path).or_else(|| t.durable_alive.get(&path))
        {
            if let Some(s) = s.upgrade() {
                return s;
            }
        }
        let s = DUVal(Arc::new(Mutex::new(DUValInner {
            sub: None,
            streams: Vec::new(),
            states: Vec::new(),
            tries: 0,
            next_try: Instant::now(),
        })));
        t.durable_dead.insert(path, s.downgrade());
        let _ = t.trigger_resub.unbounded_send(());
        s
    }

    /// Same as `durable_subscribe_val_ut` but typed.
    pub fn durable_subscribe_val<T: DeserializeOwned>(&self, path: Path) -> DVal<T> {
        self.durable_subscribe_val_ut(path).typed()
    }
}

struct CReader<R: io::Read>(R, usize);

impl<R: io::Read> io::Read for CReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let read = self.0.read(buf)?;
        self.1 += read;
        Ok(read)
    }
}

enum AugFP {
    NoSuchValue(Path),
    Unsubscribed(Id),
    Subscribed(Path, Id, Bytes),
    Message(Id, Bytes),
    Heartbeat
}

impl AugFP {
    fn decode(con: &mut ReadChannel) -> Option<Result<AugFP, rmp_serde::decode::Error>> {
        use byteorder::{BigEndian, ByteOrder};
        let u32s = mem::size_of::<u32>();
        let buf = con.buffer_mut();
        if buf.remaining() < u32s {
            None
        } else {
            let len = BigEndian::read_u32(&*buf) as usize;
            if buf.remaining() - u32s < len {
                None
            } else {
                buf.advance(u32s);
                let (head, sz) = {
                    let mut cr = CReader(buf.as_ref(), 0);
                    let r = rmp_serde::decode::from_read(&mut cr);
                    (r, cr.1)
                };
                match head {
                    Err(e) => Some(Err(e)),
                    Ok(m) => Some(Ok(match m {
                        publisher::From::NoSuchValue(p) => AugFP::NoSuchValue(p),
                        publisher::From::Unsubscribed(id) => AugFP::Unsubscribed(id),
                        publisher::From::Heartbeat => AugFP::Heartbeat,
                        publisher::From::Subscribed(p, id) => {
                            buf.advance(sz);
                            AugFP::Subscribed(
                                p, id, buf.split_to(len - sz - u32s).freeze()
                            )
                        }
                        publisher::From::Message(id) => {
                            buf.advance(sz);
                            AugFP::Message(id, buf.split_to(len - sz - u32s).freeze())
                        }
                    }))
                }
            }
        }
    }
}

struct Sub {
    path: Path,
    streams: Vec<Sender<Bytes>>,
    last: Bytes,
}

fn unsubscribe(
    subscriber: &mut SubscriberInner,
    sub: Sub,
    id: Id,
    addr: SocketAddr,
) {
    if let Some(dsw) = subscriber.durable_alive.remove(&sub.path) {
        if let Some(ds) = dsw.upgrade() {
            let mut inner = ds.0.lock();
            inner.sub = None;
            let mut i = 0;
            while i < inner.states.len() {
                match inner.states[i].unbounded_send(DVState::Unsubscribed) {
                    Ok(()) => { i+= 1; }
                    Err(_) => { inner.states.remove(i); }
                }
            }
            subscriber.durable_dead.insert(sub.path.clone(), dsw);
            let _ = subscriber.trigger_resub.unbounded_send(());
        }
    }
    match subscriber.subscribed.entry(sub.path) {
        Entry::Vacant(_) => (),
        Entry::Occupied(e) => match e.get() {
            SubStatus::Pending(_) => (),
            SubStatus::Subscribed(s) => match s.upgrade() {
                None => { e.remove(); }
                Some(s) => if s.0.id == id && s.0.addr == addr { e.remove(); }
            }
        }
    }
}

macro_rules! brk {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => break Err(Error::from(e))
        }
    }
}

macro_rules! brkm {
    ($l:tt, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => break $l Err(Error::from(e))
        }
    }
}

type Flush = Option<Fuse<oneshot::Receiver<Result<(), std::io::Error>>>>;

async fn wait_flush(flush: &mut Flush) -> Result<(), Error> {
    match flush {
        None => future::pending().await,
        Some(f) => match f.await {
            Err(_) => bail!("failed to read result of flush"),
            Ok(r) => Ok(r?)
        }
    }
}

const PERIOD: Duration = Duration::from_secs(10);

async fn connection(
    subscriber: SubscriberWeak,
    addr: SocketAddr,
    from_sub: UnboundedReceiver<ToCon>
) -> Result<(), Error> {
    let mut pending: HashMap<Path, SubscribeValRequest> = HashMap::new();
    let mut subscriptions: HashMap<Id, Sub, FxBuildHasher> =
        HashMap::with_hasher(FxBuildHasher::default());
    let mut idle: usize = 0;
    let mut msg_recvd = false;
    let mut from_sub = Batched::new(from_sub, BATCH);
    let con = Channel::new(time::timeout(PERIOD, TcpStream::connect(addr)).await??);
    let (mut read_con, mut write_con) = con.split();
    let mut flush: Flush = None;
    let mut periodic = time::interval_at(Instant::now() + PERIOD, PERIOD).fuse();
    let res = 'main: loop {
        select! {
            r = wait_flush(&mut flush).fuse() => match r {
                Err(e) => break Err(e),
                Ok(()) => {
                    if write_con.bytes_queued() == 0 {
                        flush = None;
                    } else {
                        flush = Some(write_con.flush().fuse());
                    }
                }
            },
            now = periodic.next() => if let Some(now) = now {
                if !msg_recvd {
                    break 'main Err(format_err!("hung publisher"));
                } else {
                    msg_recvd = false;
                }
                if subscriptions.len() == 0 && pending.len() == 0 {
                    idle += 1;
                    if idle == 2 { break 'main Ok(()); }
                } else {
                    idle = 0;
                }
                let mut timed_out = Vec::new();
                for (path, req) in pending.iter() {
                    if let Some(deadline) = req.deadline {
                        if deadline < now {
                            timed_out.push(path.clone());
                        }
                    }
                }
                for path in timed_out {
                    if let Some(req) = pending.remove(&path) {
                        let _ = req.finished.send(Err(format_err!("timed out")));
                    }
                }
            },
            r = read_con.fill_buffer().fuse() => match r {
                Err(e) => break Err(Error::from(e)),
                Ok(()) => if let Some(subscriber) = subscriber.upgrade() {
                    msg_recvd = true;
                    loop {
                        match AugFP::decode(&mut read_con) {
                            None => break,
                            Some(m) => match brkm!('main, m) {
                                AugFP::Heartbeat => (),
                                AugFP::NoSuchValue(path) =>
                                    if let Some(r) = pending.remove(&path) {
                                        let _ = r.finished.send(
                                            Err(format_err!("no such value"))
                                        );
                                    },
                                AugFP::Unsubscribed(id) => {
                                    if let Some(s) = subscriptions.remove(&id) {
                                        let mut t = subscriber.0.lock();
                                        unsubscribe(&mut *t, s, id, addr);
                                    }
                                },
                                AugFP::Subscribed(p, id, m) => match pending.remove(&p) {
                                    None => {
                                        brkm!('main, write_con.queue_send(
                                            &publisher::To::Unsubscribe(id))
                                        );
                                    }
                                    Some(req) => {
                                        let s = Ok(UVal(Arc::new(UValInner {
                                            id, addr, connection: req.con
                                        })));
                                        match req.finished.send(s) {
                                            Err(_) => {
                                                brkm!('main, write_con.queue_send(
                                                    &publisher::To::Unsubscribe(id)
                                                ));
                                            }
                                            Ok(()) => {
                                                subscriptions.insert(id, Sub {
                                                    path: req.path,
                                                    last: m,
                                                    streams: Vec::new(),
                                                });
                                            }
                                        }
                                    }
                                },
                                AugFP::Message(i, m) => match subscriptions.get_mut(&i) {
                                    None => {
                                        brkm!('main, write_con.queue_send(
                                            &publisher::To::Unsubscribe(i))
                                        );
                                    }
                                    Some(sub) => {
                                        let mut i = 0;
                                        while i < sub.streams.len() {
                                            let c = &mut sub.streams[i];
                                            match c.send(m.clone()).await {
                                                Ok(()) => { i += 1; }
                                                Err(_) => { sub.streams.remove(i); }
                                            }
                                        }
                                        sub.last = m;
                                    }
                                },
                            }
                        }
                    }
                    if flush.is_none() {
                        flush = Some(write_con.flush().fuse());
                    }
                }
            },
            msg = from_sub.next() => match msg {
                None => break Err(format_err!("dropped")),
                Some(BatchItem::EndBatch) => {
                    if flush.is_none() {
                        flush = Some(write_con.flush().fuse());
                    }
                }
                Some(BatchItem::InBatch(ToCon::Subscribe(req))) => {
                    let path = req.path.clone();
                    pending.insert(path.clone(), req);
                    brk!(write_con.queue_send(&publisher::To::Subscribe(path)))
                }
                Some(BatchItem::InBatch(ToCon::Unsubscribe(id))) => {
                    brk!(write_con.queue_send(&publisher::To::Unsubscribe(id)))
                }
                Some(BatchItem::InBatch(ToCon::Last(id, tx))) => {
                    if let Some(sub) = subscriptions.get(&id) {
                        let _ = tx.send(sub.last.clone());
                    }
                }
                Some(BatchItem::InBatch(ToCon::Stream { id, mut tx, last })) => {
                    if let Some(sub) = subscriptions.get_mut(&id) {
                        sub.streams.retain(|c| !c.is_closed());
                        if last {
                            let last = sub.last.clone();
                            match tx.send(last).await {
                                Err(_) => continue,
                                Ok(()) => ()
                            }
                        }
                        sub.streams.push(tx);
                    }
                }
            },
        }
    };
    if let Some(subscriber) = subscriber.upgrade() {
        let mut t = subscriber.0.lock();
        t.connections.remove(&addr);
        for (id, sub) in subscriptions {
            unsubscribe(&mut *t, sub, id, addr);
        }
        for (_, req) in pending {
            let _ = req.finished.send(Err(format_err!("connection died")));
        }
    }
    res
}

#[cfg(test)]
mod test {
    use std::{
        net::SocketAddr,
        time::Duration,
    };
    use tokio::{task, time, sync::oneshot, runtime::Runtime};
    use futures::{prelude::*, future::{self, Either}};
    use crate::{
        resolver_server::Server,
        publisher::{Publisher, BindCfg},
        subscriber::Subscriber,
        config,
    };

    async fn init_server() -> Server {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Server::new(addr, 100).await.expect("start server")
    }

    #[test]
    fn publish_subscribe() {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct V {
            id: usize,
            v: String,
        };
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async {
            let server = init_server().await;
            let cfg = config::Resolver { addr: *server.local_addr()};
            let (tx, ready) = oneshot::channel();
            task::spawn(async move {
                let publisher = Publisher::new(cfg, BindCfg::Local).await.unwrap();
                let vp0 = publisher.publish_val(
                    "/app/v0".into(),
                    &V {id: 0, v: "foo".into()}
                ).unwrap();
                let vp1 = publisher.publish_val(
                    "/app/v1".into(),
                    &V {id: 0, v: "bar".into()}
                ).unwrap();
                publisher.flush(None).await.unwrap();
                tx.send(()).unwrap();
                let mut c = 1;
                loop {
                    time::delay_for(Duration::from_millis(100)).await;
                    vp0.update(&V {id: c, v: "foo".into()})
                        .unwrap();
                    vp1.update(&V {id: c, v: "bar".into()})
                        .unwrap();
                    publisher.flush(None).await.unwrap();
                    c += 1
                }
            });
            time::timeout(Duration::from_secs(1), ready).await.unwrap().unwrap();
            let subscriber = Subscriber::new(cfg).unwrap();
            let vs0 = subscriber.subscribe_val::<V>("/app/v0".into(), None).await.unwrap();
            let vs1 = subscriber.subscribe_val::<V>("/app/v1".into(), None).await.unwrap();
            let mut c0: Option<usize> = None;
            let mut c1: Option<usize> = None;
            let mut vs0s = vs0.updates(true);
            let mut vs1s = vs1.updates(true);
            loop {
                let r = Either::factor_first(
                    future::select(vs0s.next(), vs1s.next()).await
                ).0;
                match r {
                    None => panic!("publishers died"),
                    Some(Err(e)) => panic!("publisher error: {}", e),
                    Some(Ok(v)) => {
                        let c = match &*v.v {
                            "foo" => &mut c0,
                            "bar" => &mut c1,
                            _ => panic!("unexpected v"),
                        };
                        match c {
                            None => { *c = Some(v.id); },
                            Some(c_id) => {
                                assert_eq!(*c_id + 1, v.id);
                                if *c_id >= 50 {
                                    break;
                                }
                                *c = Some(v.id);
                            }
                        }
                    }
                }
            }
            drop(server);
        });
    }
}
