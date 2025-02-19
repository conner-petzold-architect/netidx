use anyhow::{Error, Result};
use arcstr::ArcStr;
use chrono::prelude::*;
use futures::{
    channel::{mpsc, oneshot},
    future::{self, Fuse},
    prelude::*,
    select_biased,
};
use fxhash::{FxHashMap, FxHashSet};
use log::{error, info, warn};
use netidx::{
    chars::Chars,
    config::Config,
    path::Path,
    pool::Pooled,
    protocol::{
        glob::{Glob, GlobSet},
        value::FromValue,
    },
    publisher::{
        self, BindCfg, ClId, PublishFlags, Publisher, PublisherBuilder, UpdateBatch, Val,
        Value, WriteRequest,
    },
    resolver_client::{ChangeTracker, DesiredAuth, ResolverRead},
    subscriber::{Dval, Event, SubId, Subscriber, UpdatesFlags},
    utils,
};
use netidx_archive::{
    ArchiveReader, ArchiveWriter, BatchItem, Cursor, Id, MonotonicTimestamper,
    RecordTooLarge, Seek, Timestamp, BATCH_POOL,
};
use netidx_protocols::{
    cluster::{uuid_string, Cluster},
    rpc::server::{ArgSpec, Proc},
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    mem,
    ops::Bound,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use structopt::StructOpt;
use tokio::{runtime::Runtime, sync::broadcast, task, time};
use uuid::Uuid;

#[derive(StructOpt, Debug)]
pub(super) struct Params {
    #[structopt(
        short = "b",
        long = "bind",
        help = "configure the bind address e.g. 192.168.0.0/16, 127.0.0.1:5000"
    )]
    bind: Option<BindCfg>,
    #[structopt(long = "publish-base", help = "base path for republishing the archive")]
    publish_base: Option<Path>,
    #[structopt(
        long = "image-frequency",
        help = "How often to write a full image, 0 for never (67108864)",
        default_value = "67108864"
    )]
    image_frequency: usize,
    #[structopt(
        long = "poll-interval",
        help = "How often to poll the resolver, 0 for never (5)",
        default_value = "5"
    )]
    poll_interval: u64,
    #[structopt(
        long = "flush-frequency",
        help = "How often to flush changes in pages, 0 only on exit (65534 pages)",
        default_value = "65534"
    )]
    flush_frequency: usize,
    #[structopt(
        long = "flush-interval",
        help = "How often to flush changes (seconds), 0 disable (30)",
        default_value = "30"
    )]
    flush_interval: u64,
    #[structopt(
        long = "shards",
        help = "how many other recorder shards to expect",
        default_value = "0"
    )]
    shards: usize,
    #[structopt(
        long = "max-sessions",
        help = "how many total client sesions to allow",
        default_value = "256"
    )]
    max_sessions: usize,
    #[structopt(
        long = "max-sessions-per-client",
        help = "how many sesions to allow each client",
        default_value = "64"
    )]
    max_sessions_per_client: usize,
    #[structopt(long = "archive", help = "path to the archive file")]
    archive: String,
    #[structopt(long = "spec", help = "glob pattern to archive, can be repeated")]
    spec: Vec<String>,
}

#[derive(Debug, Clone)]
enum BCastMsg {
    Batch(Timestamp, Arc<Pooled<Vec<BatchItem>>>),
    Stop,
}

mod publish {
    use netidx_protocols::rpc::server::{RpcCall, RpcReply};

    use super::*;

    static START_DOC: &'static str = "The timestamp you want to replay to start at, or Unbounded for the beginning of the archive. This can also be an offset from now in terms of [+-][0-9]+[.]?[0-9]*[yMdhms], e.g. -1.5d. Default Unbounded.";
    static END_DOC: &'static str = "Time timestamp you want to replay end at, or Unbounded for the end of the archive. This can also be an offset from now in terms of [+-][0-9]+[.]?[0-9]*[yMdhms], e.g. -1.5d. default Unbounded";
    static SPEED_DOC: &'static str = "How fast you want playback to run, e.g 1 = realtime speed, 10 = 10x realtime, 0.5 = 1/2 realtime, Unlimited = as fast as data can be read and sent. Default is 1";
    static STATE_DOC: &'static str = "The current state of playback, {pause, play, tail}. Tail, seek to the end of the archive and play any new messages that arrive. Default pause.";
    static POS_DOC: &'static str = "The current playback position. Null if the archive is empty, or the timestamp of the current record. Set to any timestamp where start <= t <= end to seek. Set to [+-][0-9]+ to seek a specific number of batches, e.g. +1 to single step forward -1 to single step back. Set to [+-][0-9]+[yMdhmsu] to step forward or back that amount of time, e.g. -1y step back 1 year. -1u to step back 1 microsecond. set to 'beginning' to seek to the beginning and 'end' to seek to the end. By default the initial position is set to 'beginning' when opening the archive.";
    static PLAY_AFTER_DOC: &'static str =
        "Start playing after waiting the specified timeout";

    fn session_base(publish_base: &Path, id: Uuid) -> Path {
        use uuid::fmt::Simple;
        let mut buf = [0u8; Simple::LENGTH];
        publish_base.append(Simple::from_uuid(id).encode_lower(&mut buf))
    }

    fn parse_speed(v: Value) -> Result<Option<f64>> {
        match v.clone().cast_to::<f64>() {
            Ok(speed) => Ok(Some(speed)),
            Err(_) => match v.cast_to::<Chars>() {
                Err(_) => bail!("expected a float, or unlimited"),
                Ok(s) => {
                    if s.trim().to_lowercase().as_str() == "unlimited" {
                        Ok(None)
                    } else {
                        bail!("expected a float, or unlimited")
                    }
                }
            },
        }
    }

    fn parse_bound(v: Value) -> Result<Bound<DateTime<Utc>>> {
        match v {
            Value::DateTime(ts) => Ok(Bound::Included(ts)),
            Value::String(c) if c.trim().to_lowercase().as_str() == "unbounded" => {
                Ok(Bound::Unbounded)
            }
            v => match v.cast_to::<Seek>()? {
                Seek::Beginning => Ok(Bound::Unbounded),
                Seek::End => Ok(Bound::Unbounded),
                Seek::Absolute(ts) => Ok(Bound::Included(ts)),
                Seek::TimeRelative(offset) => Ok(Bound::Included(Utc::now() + offset)),
                Seek::BatchRelative(_) => bail!("invalid bound"),
            },
        }
    }

    fn get_bound(r: WriteRequest) -> Option<Bound<DateTime<Utc>>> {
        match parse_bound(r.value) {
            Ok(b) => Some(b),
            Err(e) => {
                if let Some(reply) = r.send_result {
                    reply.send(Value::Error(Chars::from(format!("{}", e))))
                }
                None
            }
        }
    }

    fn bound_to_val(b: Bound<DateTime<Utc>>) -> Value {
        match b {
            Bound::Unbounded => Value::String(Chars::from("Unbounded")),
            Bound::Included(ts) | Bound::Excluded(ts) => Value::DateTime(ts),
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    enum ClusterCmd {
        NotIdle,
        SeekTo(String),
        SetStart(Bound<DateTime<Utc>>),
        SetEnd(Bound<DateTime<Utc>>),
        SetSpeed(Option<f64>),
        SetState(State),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum State {
        Play,
        Pause,
        Tail,
    }

    impl FromStr for State {
        type Err = Error;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let s = s.trim().to_lowercase();
            if s.as_str() == "play" {
                Ok(State::Play)
            } else if s.as_str() == "pause" {
                Ok(State::Play)
            } else if s.as_str() == "tail" {
                Ok(State::Tail)
            } else {
                bail!("expected state [play, pause, tail]")
            }
        }
    }

    impl FromValue for State {
        fn from_value(v: Value) -> Result<Self> {
            Ok(v.cast_to::<Chars>()?.parse::<State>()?)
        }

        fn get(_: Value) -> Option<Self> {
            None
        }
    }

    impl State {
        fn play(&self) -> bool {
            match self {
                State::Play => true,
                State::Pause | State::Tail => false,
            }
        }
    }

    #[derive(Debug)]
    enum Speed {
        Unlimited(Pooled<VecDeque<(DateTime<Utc>, Pooled<Vec<BatchItem>>)>>),
        Limited {
            rate: f64,
            next: time::Instant,
            current: Pooled<VecDeque<(DateTime<Utc>, Pooled<Vec<BatchItem>>)>>,
        },
    }

    struct Controls {
        _start_doc: Val,
        start_ctl: Val,
        _end_doc: Val,
        end_ctl: Val,
        _speed_doc: Val,
        speed_ctl: Val,
        _state_doc: Val,
        state_ctl: Val,
        _pos_doc: Val,
        pos_ctl: Val,
    }

    impl Controls {
        async fn new(
            session_base: &Path,
            publisher: &Publisher,
            control_tx: &mpsc::Sender<Pooled<Vec<WriteRequest>>>,
        ) -> Result<Self> {
            let _start_doc = publisher.publish(
                session_base.append("control/start/doc"),
                Value::String(Chars::from(START_DOC)),
            )?;
            let _end_doc = publisher.publish(
                session_base.append("control/end/doc"),
                Value::String(Chars::from(END_DOC)),
            )?;
            let _speed_doc = publisher.publish(
                session_base.append("control/speed/doc"),
                Value::String(Chars::from(SPEED_DOC)),
            )?;
            let _state_doc = publisher.publish(
                session_base.append("control/state/doc"),
                Value::String(Chars::from(STATE_DOC)),
            )?;
            let _pos_doc = publisher.publish(
                session_base.append("control/pos/doc"),
                Value::String(Chars::from(POS_DOC)),
            )?;
            let start_ctl = publisher.publish_with_flags(
                PublishFlags::USE_EXISTING,
                session_base.append("control/start/current"),
                Value::String(Chars::from("Unbounded")),
            )?;
            publisher.writes(start_ctl.id(), control_tx.clone());
            let end_ctl = publisher.publish_with_flags(
                PublishFlags::USE_EXISTING,
                session_base.append("control/end/current"),
                Value::String(Chars::from("Unbounded")),
            )?;
            publisher.writes(end_ctl.id(), control_tx.clone());
            let speed_ctl = publisher.publish_with_flags(
                PublishFlags::USE_EXISTING,
                session_base.append("control/speed/current"),
                Value::F64(1.),
            )?;
            publisher.writes(speed_ctl.id(), control_tx.clone());
            let state_ctl = publisher.publish_with_flags(
                PublishFlags::USE_EXISTING,
                session_base.append("control/state/current"),
                Value::String(Chars::from("pause")),
            )?;
            publisher.writes(state_ctl.id(), control_tx.clone());
            let pos_ctl = publisher.publish_with_flags(
                PublishFlags::USE_EXISTING,
                session_base.append("control/pos/current"),
                Value::Null,
            )?;
            publisher.writes(pos_ctl.id(), control_tx.clone());
            publisher.flushed().await;
            Ok(Controls {
                _start_doc,
                start_ctl,
                _end_doc,
                end_ctl,
                _speed_doc,
                speed_ctl,
                _state_doc,
                state_ctl,
                _pos_doc,
                pos_ctl,
            })
        }
    }

    struct NewSessionConfig {
        client: ClId,
        start: Bound<DateTime<Utc>>,
        end: Bound<DateTime<Utc>>,
        speed: Option<f64>,
        pos: Option<Seek>,
        state: Option<State>,
        play_after: Option<Duration>,
    }

    impl NewSessionConfig {
        fn new(
            mut req: RpcCall,
            start: Value,
            end: Value,
            speed: Value,
            pos: Option<Seek>,
            state: Option<State>,
            play_after: Option<Duration>,
        ) -> Option<(NewSessionConfig, RpcReply)> {
            let start = match parse_bound(start) {
                Ok(s) => s,
                Err(e) => rpc_err!(req.reply, format!("invalid start {}", e)),
            };
            let end = match parse_bound(end) {
                Ok(s) => s,
                Err(e) => rpc_err!(req.reply, format!("invalid end {}", e)),
            };
            let speed = match parse_speed(speed) {
                Ok(s) => s,
                Err(e) => rpc_err!(req.reply, format!("invalid speed {}", e)),
            };
            let s = NewSessionConfig {
                client: req.client,
                start,
                end,
                speed,
                pos,
                state,
                play_after,
            };
            Some((s, req.reply))
        }
    }

    struct T {
        controls: Controls,
        publisher: Publisher,
        published: FxHashMap<Id, Val>,
        published_ids: FxHashSet<publisher::Id>,
        cursor: Cursor,
        speed: Speed,
        state: State,
        archive: ArchiveReader,
        data_base: Path,
    }

    impl T {
        async fn new(
            publisher: Publisher,
            archive: ArchiveReader,
            session_base: Path,
            control_tx: &mpsc::Sender<Pooled<Vec<WriteRequest>>>,
        ) -> Result<T> {
            let controls = Controls::new(&session_base, &publisher, &control_tx).await?;
            Ok(T {
                controls,
                publisher,
                published: HashMap::default(),
                published_ids: HashSet::default(),
                cursor: Cursor::new(),
                speed: Speed::Limited {
                    rate: 1.,
                    next: time::Instant::now(),
                    current: Pooled::orphan(VecDeque::new()),
                },
                state: State::Pause,
                archive,
                data_base: session_base.append("data"),
            })
        }

        async fn next(&mut self) -> Result<(DateTime<Utc>, Pooled<Vec<BatchItem>>)> {
            if !self.state.play() {
                future::pending().await
            } else {
                match &mut self.speed {
                    Speed::Unlimited(batches) => match batches.pop_front() {
                        Some(batch) => Ok(batch),
                        None => {
                            let archive = &self.archive;
                            let cursor = &mut self.cursor;
                            *batches =
                                task::block_in_place(|| archive.read_deltas(cursor, 3))?;
                            match batches.pop_front() {
                                Some(batch) => Ok(batch),
                                None => {
                                    let mut cbatch = self.publisher.start_batch();
                                    self.set_state(&mut cbatch, State::Tail);
                                    cbatch.commit(None).await;
                                    future::pending().await
                                }
                            }
                        }
                    },
                    Speed::Limited { rate, next, current } => {
                        use tokio::time::Instant;
                        if current.len() < 2 {
                            let archive = &self.archive;
                            let cursor = &mut self.cursor;
                            let mut cur =
                                task::block_in_place(|| archive.read_deltas(cursor, 3))?;
                            for v in current.drain(..) {
                                cur.push_front(v);
                            }
                            *current = cur;
                            if current.is_empty() {
                                let mut cbatch = self.publisher.start_batch();
                                self.set_state(&mut cbatch, State::Tail);
                                cbatch.commit(None).await;
                                return future::pending().await;
                            }
                        }
                        let (ts, batch) = current.pop_front().unwrap();
                        let mut now = Instant::now();
                        if *next >= now {
                            time::sleep_until(*next).await;
                            now = Instant::now();
                        }
                        if current.is_empty() {
                            let mut cbatch = self.publisher.start_batch();
                            self.set_state(&mut cbatch, State::Tail);
                            cbatch.commit(None).await;
                        } else {
                            let wait = {
                                let ms = (current[0].0 - ts).num_milliseconds() as f64;
                                (ms / *rate).trunc() as u64
                            };
                            *next = now + Duration::from_millis(wait);
                        }
                        Ok((ts, batch))
                    }
                }
            }
        }

        async fn process_batch(
            &mut self,
            batch: (DateTime<Utc>, &mut Vec<BatchItem>),
        ) -> Result<()> {
            let mut pbatch = self.publisher.start_batch();
            for BatchItem(id, ev) in batch.1.drain(..) {
                let v = match ev {
                    Event::Unsubscribed => Value::Null,
                    Event::Update(v) => v,
                };
                match self.published.get(&id) {
                    Some(val) => {
                        val.update(&mut pbatch, v);
                    }
                    None => {
                        let path = self.archive.path_for_id(&id).unwrap();
                        let path = self.data_base.append(&path);
                        let val = self.publisher.publish(path, v)?;
                        self.published_ids.insert(val.id());
                        self.published.insert(id, val);
                    }
                }
            }
            self.controls.pos_ctl.update(&mut pbatch, Value::DateTime(batch.0));
            Ok(pbatch.commit(None).await)
        }

        async fn process_bcast(
            &mut self,
            msg: Result<BCastMsg, broadcast::error::RecvError>,
        ) -> Result<()> {
            match msg {
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("broadcast channel closed")
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => match self.state {
                    State::Play | State::Pause => Ok(()),
                    State::Tail => {
                        let archive = &self.archive;
                        let cursor = &mut self.cursor;
                        let mut batches = task::block_in_place(|| {
                            archive.read_deltas(cursor, missed as usize)
                        })?;
                        for (ts, mut batch) in batches.drain(..) {
                            self.process_batch((ts, &mut *batch)).await?;
                            self.cursor.set_current(ts);
                        }
                        Ok(())
                    }
                },
                Ok(BCastMsg::Batch(ts, batch)) => match self.state {
                    State::Play | State::Pause => Ok(()),
                    State::Tail => {
                        let dt = ts.datetime();
                        let pos =
                            self.cursor.current().unwrap_or(DateTime::<Utc>::MIN_UTC);
                        if self.cursor.contains(&dt) && pos < dt {
                            let mut batch = (*batch).clone();
                            self.process_batch((dt, &mut batch)).await?;
                            self.cursor.set_current(dt);
                        }
                        Ok(())
                    }
                },
                Ok(BCastMsg::Stop) => bail!("stop signal"),
            }
        }

        fn set_start(
            &mut self,
            cbatch: &mut UpdateBatch,
            new_start: Bound<DateTime<Utc>>,
        ) -> Result<()> {
            self.controls.start_ctl.update(cbatch, bound_to_val(new_start));
            self.cursor.set_start(new_start);
            if self.cursor.current().is_none() {
                self.seek(cbatch, Seek::Beginning)?;
            }
            Ok(())
        }

        fn set_end(
            &mut self,
            cbatch: &mut UpdateBatch,
            new_end: Bound<DateTime<Utc>>,
        ) -> Result<()> {
            self.controls.end_ctl.update(cbatch, bound_to_val(new_end));
            self.cursor.set_end(new_end);
            if self.cursor.current().is_none() {
                self.seek(cbatch, Seek::Beginning)?;
            }
            Ok(())
        }

        fn set_state(&mut self, cbatch: &mut UpdateBatch, state: State) {
            match (self.state, state) {
                (State::Tail, State::Play) => (),
                (s0, s1) if s0 == s1 => (),
                (_, state) => {
                    self.state = state;
                    self.controls.state_ctl.update(
                        cbatch,
                        Value::from(match state {
                            State::Play => "play",
                            State::Pause => "pause",
                            State::Tail => "tail",
                        }),
                    );
                }
            }
        }

        async fn apply_config(
            &mut self,
            cbatch: &mut UpdateBatch,
            cluster: &Cluster<ClusterCmd>,
            cfg: NewSessionConfig,
        ) -> Result<()> {
            self.set_start(cbatch, cfg.start)?;
            cluster.send_cmd(&ClusterCmd::SetStart(cfg.start));
            self.set_end(cbatch, cfg.end)?;
            cluster.send_cmd(&ClusterCmd::SetEnd(cfg.end));
            self.set_speed(cbatch, cfg.speed);
            cluster.send_cmd(&ClusterCmd::SetSpeed(cfg.speed));
            if let Some(pos) = cfg.pos {
                self.seek(cbatch, pos)?;
                cluster.send_cmd(&ClusterCmd::SeekTo(pos.to_string()));
            }
            if let Some(state) = cfg.state {
                self.set_state(cbatch, state);
                cluster.send_cmd(&ClusterCmd::SetState(state));
            }
            if let Some(play_after) = cfg.play_after {
                time::sleep(play_after).await;
                self.set_state(cbatch, State::Play);
                cluster.send_cmd(&ClusterCmd::SetState(State::Play));
            }
            Ok(())
        }

        async fn process_control_batch(
            &mut self,
            session_id: Uuid,
            cluster: &Cluster<ClusterCmd>,
            mut batch: Pooled<Vec<WriteRequest>>,
        ) -> Result<()> {
            let mut inst = HashMap::new();
            let mut cbatch = self.publisher.start_batch();
            for req in batch.drain(..) {
                inst.insert(req.id, req);
            }
            for (_, req) in inst {
                if req.id == self.controls.start_ctl.id() {
                    info!("set start {}: {}", session_id, req.value);
                    if let Some(new_start) = get_bound(req) {
                        self.set_start(&mut cbatch, new_start)?;
                        cluster.send_cmd(&ClusterCmd::SetStart(new_start));
                    }
                } else if req.id == self.controls.end_ctl.id() {
                    info!("set end {}: {}", session_id, req.value);
                    if let Some(new_end) = get_bound(req) {
                        self.set_end(&mut cbatch, new_end)?;
                        cluster.send_cmd(&ClusterCmd::SetEnd(new_end));
                    }
                } else if req.id == self.controls.speed_ctl.id() {
                    info!("set speed {}: {}", session_id, req.value);
                    match parse_speed(req.value) {
                        Ok(speed) => {
                            self.set_speed(&mut cbatch, speed);
                            cluster.send_cmd(&ClusterCmd::SetSpeed(speed));
                        }
                        Err(e) => {
                            if let Some(reply) = req.send_result {
                                reply.send(Value::Error(Chars::from(format!("{}", e))));
                            }
                        }
                    }
                } else if req.id == self.controls.state_ctl.id() {
                    info!("set state {}: {}", session_id, req.value);
                    match req.value.cast_to::<State>() {
                        Ok(state) => {
                            self.set_state(&mut cbatch, state);
                            cluster.send_cmd(&ClusterCmd::SetState(state));
                        }
                        Err(e) => {
                            if let Some(reply) = req.send_result {
                                reply.send(Value::Error(Chars::from(format!("{}", e))))
                            }
                        }
                    }
                } else if req.id == self.controls.pos_ctl.id() {
                    info!("set pos {}: {}", session_id, req.value);
                    match req.value.cast_to::<Seek>() {
                        Ok(pos) => {
                            self.seek(&mut cbatch, pos)?;
                            match self.state {
                                State::Pause | State::Play => (),
                                State::Tail => {
                                    self.set_state(&mut cbatch, State::Play);
                                }
                            }
                            cluster.send_cmd(&ClusterCmd::SeekTo(pos.to_string()));
                        }
                        Err(e) => {
                            if let Some(reply) = req.send_result {
                                reply.send(Value::Error(Chars::from(format!("{}", e))))
                            }
                        }
                    }
                }
            }
            Ok(cbatch.commit(None).await)
        }

        fn process_control_cmd(
            &mut self,
            cbatch: &mut UpdateBatch,
            cmd: ClusterCmd,
        ) -> Result<()> {
            match cmd {
                ClusterCmd::SeekTo(s) => match s.parse::<Seek>() {
                    Ok(pos) => self.seek(cbatch, pos),
                    Err(e) => {
                        warn!("invalid seek from cluster {}, {}", s, e);
                        Ok(())
                    }
                },
                ClusterCmd::SetStart(new_start) => self.set_start(cbatch, new_start),
                ClusterCmd::SetEnd(new_end) => self.set_end(cbatch, new_end),
                ClusterCmd::SetSpeed(sp) => Ok(self.set_speed(cbatch, sp)),
                ClusterCmd::SetState(st) => Ok(self.set_state(cbatch, st)),
                ClusterCmd::NotIdle => Ok(()),
            }
        }

        fn reimage(&mut self, pbatch: &mut UpdateBatch) -> Result<()> {
            let mut img =
                task::block_in_place(|| self.archive.build_image(&self.cursor))?;
            let mut idx = task::block_in_place(|| self.archive.get_index());
            self.controls.pos_ctl.update(
                pbatch,
                match self.cursor.current() {
                    Some(ts) => Value::DateTime(ts),
                    None => match self.cursor.start() {
                        Bound::Unbounded => Value::Null,
                        Bound::Included(ts) | Bound::Excluded(ts) => Value::DateTime(ts),
                    },
                },
            );
            for (id, path) in idx.drain(..) {
                let v = match img.remove(&id) {
                    None | Some(Event::Unsubscribed) => Value::Null,
                    Some(Event::Update(v)) => v,
                };
                match self.published.get(&id) {
                    Some(val) => {
                        val.update(pbatch, v);
                    }
                    None => {
                        let path = self.data_base.append(path.as_ref());
                        let val = self.publisher.publish(path, v)?;
                        self.published_ids.insert(val.id());
                        self.published.insert(id, val);
                    }
                }
            }
            Ok(())
        }

        fn seek(&mut self, pbatch: &mut UpdateBatch, seek: Seek) -> Result<()> {
            let current = match &mut self.speed {
                Speed::Unlimited(v) => v,
                Speed::Limited { current, next, .. } => {
                    *next = time::Instant::now();
                    current
                }
            };
            if let Some((ts, _)) = current.pop_front() {
                self.cursor.set_current(ts);
                current.clear()
            }
            self.archive.seek(&mut self.cursor, seek);
            self.reimage(pbatch)
        }

        fn set_speed(&mut self, cbatch: &mut UpdateBatch, new_rate: Option<f64>) {
            match new_rate {
                None => {
                    self.controls
                        .speed_ctl
                        .update(cbatch, Value::String(Chars::from("unlimited")));
                }
                Some(new_rate) => {
                    self.controls.speed_ctl.update(cbatch, Value::F64(new_rate));
                }
            };
            match &mut self.speed {
                Speed::Limited { rate, current, .. } => match new_rate {
                    Some(new_rate) => {
                        *rate = new_rate;
                    }
                    None => {
                        let c = mem::replace(current, Pooled::orphan(VecDeque::new()));
                        self.speed = Speed::Unlimited(c);
                    }
                },
                Speed::Unlimited(v) => {
                    if let Some(new_rate) = new_rate {
                        let v = mem::replace(v, Pooled::orphan(VecDeque::new()));
                        self.speed = Speed::Limited {
                            rate: new_rate,
                            next: time::Instant::now(),
                            current: v,
                        };
                    }
                }
            }
        }
    }

    fn not_idle(idle: &mut bool, cluster: &Cluster<ClusterCmd>) {
        *idle = false;
        cluster.send_cmd(&ClusterCmd::NotIdle);
    }

    async fn session(
        mut bcast: broadcast::Receiver<BCastMsg>,
        archive: ArchiveReader,
        subscriber: Subscriber,
        publisher: Publisher,
        publish_base: Path,
        session_id: Uuid,
        shards: usize,
        cfg: Option<NewSessionConfig>,
    ) -> Result<()> {
        let (control_tx, control_rx) = mpsc::channel(3);
        let (events_tx, mut events_rx) = mpsc::unbounded();
        publisher.events(events_tx);
        let session_base = session_base(&publish_base, session_id);
        let mut cluster =
            Cluster::new(&publisher, subscriber, session_base.append("cluster"), shards)
                .await?;
        archive.check_remap_rescan()?;
        let mut t = T::new(publisher.clone(), archive, session_base, &control_tx).await?;
        let mut batch = publisher.start_batch();
        t.seek(&mut batch, Seek::Beginning)?;
        if let Some(cfg) = cfg {
            t.apply_config(&mut batch, &cluster, cfg).await?
        }
        batch.commit(None).await;
        let mut control_rx = control_rx.fuse();
        let mut idle_check = time::interval(std::time::Duration::from_secs(30));
        let mut idle = false;
        let mut used = 0;
        loop {
            select_biased! {
                e = events_rx.select_next_some() => match e {
                    publisher::Event::Subscribe(id, _) => if t.published_ids.contains(&id) {
                        used += 1;
                    },
                    publisher::Event::Unsubscribe(id, _) => if t.published_ids.contains(&id) {
                        used -= 1;
                    },
                    publisher::Event::Destroyed(_) => (),
                },
                _ = idle_check.tick().fuse() => {
                    let has_clients = used > 0;
                    if !has_clients && idle {
                        break Ok(())
                    } else if has_clients {
                        not_idle(&mut idle, &cluster)
                    } else {
                        idle = true;
                    }
                },
                _ = publisher.wait_any_new_client().fuse() => {
                    if publisher.clients() > cluster.others() {
                        not_idle(&mut idle, &cluster)
                    }
                },
                m = bcast.recv().fuse() => t.process_bcast(m).await?,
                cmds = cluster.wait_cmds().fuse() => {
                    let mut cbatch = publisher.start_batch();
                    for cmd in cmds? {
                        t.process_control_cmd(&mut cbatch, cmd)?
                    }
                    cbatch.commit(None).await;
                },
                r = control_rx.next() => match r {
                    None => break Ok(()),
                    Some(batch) => {
                        t.process_control_batch(session_id, &cluster, batch).await?
                    }
                },
                r = t.next().fuse() => match r {
                    Err(e) => break Err(e),
                    Ok((ts, mut batch)) => { t.process_batch((ts, &mut *batch)).await?; }
                }
            }
        }
    }

    struct SessionsInner {
        max_total: usize,
        max_by_client: usize,
        total: usize,
        by_client: FxHashMap<ClId, usize>,
    }

    #[derive(Clone)]
    struct Sessions(Arc<Mutex<SessionsInner>>);

    impl Sessions {
        fn new(max_total: usize, max_by_client: usize) -> Self {
            Sessions(Arc::new(Mutex::new(SessionsInner {
                max_total,
                max_by_client,
                total: 0,
                by_client: HashMap::default(),
            })))
        }

        fn add_session(&self, client: ClId) -> Option<Session> {
            let mut inner = self.0.lock();
            let inner = &mut *inner;
            let by_client = inner.by_client.entry(client).or_insert(0);
            if inner.total < inner.max_total && *by_client < inner.max_by_client {
                inner.total += 1;
                *by_client += 1;
                Some(Session(self.clone(), client))
            } else {
                None
            }
        }

        fn delete_session(&self, session: &Session) {
            let mut inner = self.0.lock();
            if let Some(c) = inner.by_client.get_mut(&session.1) {
                *c -= 1;
                inner.total -= 1;
            }
        }
    }

    struct Session(Sessions, ClId);

    impl Drop for Session {
        fn drop(&mut self) {
            self.0.delete_session(self)
        }
    }

    async fn start_session(
        publisher: Publisher,
        session_id: Uuid,
        session_token: Session,
        bcast: &broadcast::Sender<BCastMsg>,
        subscriber: &Subscriber,
        archive: &ArchiveReader,
        shards: usize,
        publish_base: &Path,
        cfg: Option<NewSessionConfig>,
    ) -> Result<()> {
        let bcast = bcast.subscribe();
        let archive = archive.clone();
        let publish_base = publish_base.clone();
        let subscriber = subscriber.clone();
        let publisher_cl = publisher.clone();
        task::spawn(async move {
            let res = session(
                bcast,
                archive,
                subscriber,
                publisher_cl,
                publish_base,
                session_id,
                shards,
                cfg,
            )
            .await;
            match res {
                Ok(()) => {
                    info!("session {} existed", session_id)
                }
                Err(e) => {
                    error!("session {} exited {}", session_id, e)
                }
            }
            drop(session_token)
        });
        Ok(())
    }

    pub(super) async fn run(
        bcast: broadcast::Sender<BCastMsg>,
        archive: ArchiveReader,
        resolver: Config,
        desired_auth: DesiredAuth,
        bind_cfg: Option<BindCfg>,
        publish_base: Path,
        shards: usize,
        max_sessions: usize,
        max_sessions_per_client: usize,
    ) -> Result<()> {
        let sessions: Sessions = Sessions::new(max_sessions, max_sessions_per_client);
        let subscriber = Subscriber::new(resolver.clone(), desired_auth.clone())?;
        let mut builder = PublisherBuilder::new();
        builder.config(resolver.clone()).desired_auth(desired_auth.clone());
        if let Some(b) = bind_cfg {
            builder.bind_cfg(b);
        }
        let publisher = builder.build().await?;
        let (control_tx, control_rx) = mpsc::channel(3);
        let _new_session: Result<Proc> = define_rpc!(
            &publisher,
            publish_base.append("session"),
            "create a new playback session",
            NewSessionConfig::new,
            Some(control_tx.clone()),
            start: Value = "Unbounded"; START_DOC,
            end: Value = "Unbounded"; END_DOC,
            speed: Value = "1."; SPEED_DOC,
            pos: Option<Seek> = Value::Null; POS_DOC,
            state: Option<State> = Value::Null; STATE_DOC,
            play_after: Option<Duration> = None::<Duration>; PLAY_AFTER_DOC
        );
        let _new_session = _new_session?;
        let mut cluster = Cluster::<(ClId, Uuid)>::new(
            &publisher,
            subscriber.clone(),
            publish_base.append("cluster"),
            shards,
        )
        .await?;
        let mut control_rx = control_rx.fuse();
        let mut bcast_rx = bcast.subscribe();
        let mut poll_members = time::interval(std::time::Duration::from_secs(30));
        loop {
            select_biased! {
                m = bcast_rx.recv().fuse() => match m {
                    Err(_) | Ok(BCastMsg::Batch(_, _)) => (),
                    Ok(BCastMsg::Stop) => break Ok(()),
                },
                _ = poll_members.tick().fuse() => {
                    if let Err(e) = cluster.poll_members().await {
                        warn!("failed to poll cluster members, will retry {}", e)
                    }
                },
                cmds = cluster.wait_cmds().fuse() => match cmds {
                    Err(e) => {
                        error!("received unparsable cluster commands {}", e)
                    }
                    Ok(cmds) => for (client, session_id) in cmds {
                        match sessions.add_session(client) {
                            None => {
                                error!("can't start session requested by cluster member, too many sessions")
                            },
                            Some(session_token) => {
                                let r = start_session(
                                    publisher.clone(),
                                    session_id,
                                    session_token,
                                    &bcast,
                                    &subscriber,
                                    &archive,
                                    shards,
                                    &publish_base,
                                    None
                                ).await;
                                if let Err(e) = r {
                                    warn!("failed to start session {}, {}", session_id, e)
                                }
                            }
                        }
                    }
                },
                m = control_rx.next() => match m {
                    None => break Ok(()),
                    Some((cfg, mut reply)) => {
                        match sessions.add_session(cfg.client) {
                            None => {
                                let m = format!("too many sessions, client {:?}", cfg.client);
                                reply.send(Value::Error(Chars::from(m)));
                            },
                            Some(session_token) => {
                                let session_id = Uuid::new_v4();
                                let client = cfg.client;
                                info!("start session {}", session_id);
                                let r = start_session(
                                    publisher.clone(),
                                    session_id,
                                    session_token,
                                    &bcast,
                                    &subscriber,
                                    &archive,
                                    shards,
                                    &publish_base,
                                    Some(cfg)
                                ).await;
                                match r {
                                    Err(e) => {
                                        let e = Chars::from(format!("{}", e));
                                        warn!("failed to start session {}, {}", session_id, e);
                                        reply.send(Value::Error(e));
                                    }
                                    Ok(()) => {
                                        cluster.send_cmd(&(client, session_id));
                                        reply.send(Value::from(uuid_string(session_id)));
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

mod record {
    use super::*;

    #[derive(Debug)]
    struct CTS(BTreeMap<Path, ChangeTracker>);

    impl CTS {
        fn new(globs: &Vec<Glob>) -> CTS {
            let mut btm = BTreeMap::new();
            for glob in globs {
                let base = glob.base();
                match btm
                    .range::<str, (Bound<&str>, Bound<&str>)>((
                        Bound::Unbounded,
                        Bound::Excluded(base),
                    ))
                    .next_back()
                {
                    Some((p, _)) if Path::is_parent(p, base) => (),
                    None | Some(_) => {
                        let base = Path::from(ArcStr::from(base));
                        let ct = ChangeTracker::new(base.clone());
                        btm.insert(base, ct);
                    }
                }
            }
            CTS(btm)
        }

        async fn changed(&mut self, r: &ResolverRead) -> Result<bool> {
            let res =
                future::join_all(self.0.iter_mut().map(|(_, ct)| r.check_changed(ct)))
                    .await;
            for r in res {
                if r? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    async fn maybe_interval(poll: &mut Option<time::Interval>) {
        match poll {
            None => future::pending().await,
            Some(poll) => {
                poll.tick().await;
            }
        }
    }

    type Lst = Option<Pooled<Vec<Pooled<Vec<Path>>>>>;

    async fn list_task(
        mut rx: mpsc::UnboundedReceiver<oneshot::Sender<Lst>>,
        resolver: ResolverRead,
        spec: Vec<Glob>,
    ) -> Result<()> {
        let mut cts = CTS::new(&spec);
        let spec = GlobSet::new(true, spec)?;
        while let Some(reply) = rx.next().await {
            match cts.changed(&resolver).await {
                Ok(true) => match resolver.list_matching(&spec).await {
                    Ok(lst) => {
                        let _ = reply.send(Some(lst));
                    }
                    Err(e) => {
                        warn!("list_task: list_matching failed {}, will retry", e);
                        let _ = reply.send(None);
                    }
                },
                Ok(false) => {
                    let _ = reply.send(None);
                }
                Err(e) => {
                    warn!("list_task: check_changed failed {}, will retry", e);
                    let _ = reply.send(None);
                }
            }
        }
        Ok(())
    }

    fn start_list_task(
        rx: mpsc::UnboundedReceiver<oneshot::Sender<Lst>>,
        resolver: ResolverRead,
        spec: Vec<Glob>,
    ) {
        task::spawn(async move {
            let r = list_task(rx, resolver, spec).await;
            match r {
                Err(e) => error!("list task exited with error {}", e),
                Ok(()) => info!("list task exited"),
            }
        });
    }

    async fn wait_list(pending: &mut Option<Fuse<oneshot::Receiver<Lst>>>) -> Lst {
        match pending {
            None => future::pending().await,
            Some(r) => match r.await {
                Ok(r) => r,
                Err(_) => None,
            },
        }
    }

    pub(super) async fn run(
        bcast: broadcast::Sender<BCastMsg>,
        mut archive: ArchiveWriter,
        resolver: Config,
        desired_auth: DesiredAuth,
        poll_interval: Option<time::Duration>,
        image_frequency: Option<usize>,
        flush_frequency: Option<usize>,
        flush_interval: Option<time::Duration>,
        spec: Vec<Glob>,
    ) -> Result<()> {
        let (tx_batch, rx_batch) = mpsc::channel(10);
        let (tx_list, rx_list) = mpsc::unbounded();
        let mut rx_batch = utils::Batched::new(rx_batch.fuse(), 10);
        let mut by_subid: FxHashMap<SubId, Id> = HashMap::default();
        let mut image: FxHashMap<SubId, Event> = HashMap::default();
        let mut subscribed: HashMap<Path, Dval> = HashMap::new();
        let subscriber = Subscriber::new(resolver, desired_auth)?;
        let flush_frequency = flush_frequency.map(|f| archive.block_size() * f);
        let mut bcast_rx = bcast.subscribe();
        let mut poll = poll_interval.map(time::interval);
        let mut flush = flush_interval.map(time::interval);
        let mut to_add = Vec::new();
        let mut timest = MonotonicTimestamper::new();
        let mut last_image = archive.len();
        let mut last_flush = archive.len();
        let mut pending_list: Option<Fuse<oneshot::Receiver<Lst>>> = None;
        let mut pending_batches: Vec<Pooled<Vec<(SubId, Event)>>> = Vec::new();
        start_list_task(rx_list, subscriber.resolver(), spec);
        loop {
            select_biased! {
                m = bcast_rx.recv().fuse() => match m {
                    Err(_) | Ok(BCastMsg::Batch(_, _)) => (),
                    Ok(BCastMsg::Stop) => break,
                },
                _ = maybe_interval(&mut poll).fuse() => {
                    if pending_list.is_none() {
                        let (tx, rx) = oneshot::channel();
                        let _ = tx_list.unbounded_send(tx);
                        pending_list = Some(rx.fuse());
                    }
                },
                _ = maybe_interval(&mut flush).fuse() => {
                    if archive.len() > last_flush {
                        task::block_in_place(|| -> Result<()> {
                            archive.flush()?;
                            Ok(last_flush = archive.len())
                        })?;
                    }
                }
                r = wait_list(&mut pending_list).fuse() => {
                    pending_list = None;
                    if let Some(mut batches) = r {
                        for mut batch in batches.drain(..) {
                            for path in batch.drain(..) {
                                if !subscribed.contains_key(&path) {
                                    let dv = subscriber.subscribe(path.clone());
                                    let id = dv.id();
                                    dv.updates(
                                        UpdatesFlags::BEGIN_WITH_LAST
                                            | UpdatesFlags::STOP_COLLECTING_LAST,
                                        tx_batch.clone()
                                    );
                                    subscribed.insert(path.clone(), dv);
                                    to_add.push((path, id));
                                }
                            }
                        }
                        task::block_in_place(|| {
                            let i = to_add.iter().map(|(ref p, _)| p);
                            archive.add_paths(i)
                        })?;
                        for (path, subid) in to_add.drain(..) {
                            if !by_subid.contains_key(&subid) {
                                let id = archive.id_for_path(&path).unwrap();
                                by_subid.insert(subid, id);
                            }
                        }
                    }
                },
                batch = rx_batch.next() => match batch {
                    None => break,
                    Some(utils::BatchItem::InBatch(batch)) => {
                        pending_batches.push(batch);
                    },
                    Some(utils::BatchItem::EndBatch) => {
                        let mut overflow = Vec::new();
                        let mut tbatch = BATCH_POOL.take();
                        task::block_in_place(|| -> Result<()> {
                            for mut batch in pending_batches.drain(..) {
                                for (subid, ev) in batch.drain(..) {
                                    if image_frequency.is_some() {
                                        image.insert(subid, ev.clone());
                                    }
                                    tbatch.push(BatchItem(by_subid[&subid], ev));
                                }
                            }
                            loop { // handle batches >4 GiB
                                let ts = timest.timestamp();
                                match archive.add_batch(false, ts, &tbatch) {
                                    Err(e) if e.is::<RecordTooLarge>() => {
                                        let at = tbatch.len() >> 1;
                                        overflow.push(tbatch.split_off(at));
                                    }
                                    Err(e) => bail!(e),
                                    Ok(()) => {
                                        let m = BCastMsg::Batch(ts, Arc::new(tbatch));
                                        let _ = bcast.send(m);
                                        match overflow.pop() {
                                            None => break,
                                            Some(b) => { tbatch = Pooled::orphan(b); }
                                        }
                                    }
                                }
                            }
                            match image_frequency {
                                None => (),
                                Some(freq) if archive.len() - last_image < freq => (),
                                Some(_) => {
                                    let mut b = BATCH_POOL.take();
                                    let ts = timest.timestamp();
                                    for (id, ev) in image.iter() {
                                        b.push(BatchItem(by_subid[id], ev.clone()));
                                    }
                                    archive.add_batch(true, ts, &b)?;
                                    last_image = archive.len();
                                }
                            }
                            match flush_frequency {
                                None => (),
                                Some(freq) if archive.len() - last_flush < freq => (),
                                Some(_) => {
                                    archive.flush()?;
                                    last_flush = archive.len();
                                }
                            }
                            Ok(())
                        })?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
async fn should_exit() -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut intr = signal(SignalKind::interrupt())?;
    select_biased! {
        _ = term.recv().fuse() => Ok(()),
        _ = quit.recv().fuse() => Ok(()),
        _ = intr.recv().fuse() => Ok(()),
    }
}

#[cfg(windows)]
async fn should_exit() -> Result<()> {
    Ok(signal::ctrl_c().await?)
}

async fn run_async(
    config: Config,
    publish_args: Option<(Option<BindCfg>, Path)>,
    auth: DesiredAuth,
    image_frequency: Option<usize>,
    poll_interval: Option<time::Duration>,
    flush_frequency: Option<usize>,
    flush_interval: Option<time::Duration>,
    shards: usize,
    max_sessions: usize,
    max_sessions_per_client: usize,
    archive: String,
    spec: Vec<Glob>,
) {
    let mut wait = Vec::new();
    let (bcast_tx, bcast_rx) = broadcast::channel(100);
    drop(bcast_rx);
    let writer = if spec.is_empty() {
        None
    } else {
        Some(ArchiveWriter::open(archive.as_str()).unwrap())
    };
    if let Some((bind_cfg, publish_base)) = publish_args {
        let reader = writer
            .as_ref()
            .map(|w| w.reader().unwrap())
            .unwrap_or_else(|| ArchiveReader::open(archive.as_str()).unwrap());
        let bcast_tx = bcast_tx.clone();
        let config = config.clone();
        let auth = auth.clone();
        wait.push(task::spawn(async move {
            let res = publish::run(
                bcast_tx,
                reader,
                config,
                auth,
                bind_cfg,
                publish_base,
                shards,
                max_sessions,
                max_sessions_per_client,
            )
            .await;
            match res {
                Ok(()) => info!("archive publisher exited"),
                Err(e) => error!("archive publisher exited with error: {}", e),
            }
        }));
    }
    if !spec.is_empty() {
        let bcast_tx = bcast_tx.clone();
        wait.push(task::spawn(async move {
            let res = record::run(
                bcast_tx,
                writer.unwrap(),
                config,
                auth,
                poll_interval,
                image_frequency,
                flush_frequency,
                flush_interval,
                spec,
            )
            .await;
            match res {
                Ok(()) => info!("archive writer exited"),
                Err(e) => error!("archive writer exited with error: {}", e),
            }
        }));
    }
    let mut dead = future::join_all(wait).fuse();
    loop {
        select_biased! {
            _ = should_exit().fuse() => {
                let _ = bcast_tx.send(BCastMsg::Stop);
            },
            _ = dead => break
        }
    }
}

pub(super) fn run(config: Config, auth: DesiredAuth, params: Params) {
    let image_frequency =
        if params.image_frequency == 0 { None } else { Some(params.image_frequency) };
    let poll_interval = if params.poll_interval == 0 {
        None
    } else {
        Some(time::Duration::from_secs(params.poll_interval))
    };
    let flush_frequency =
        if params.flush_frequency == 0 { None } else { Some(params.flush_frequency) };
    let flush_interval = if params.flush_interval == 0 {
        None
    } else {
        Some(time::Duration::from_secs(params.flush_interval))
    };
    let publish_args = match (params.bind, params.publish_base) {
        (None, None) => None,
        (None, Some(publish_base)) => Some((None, publish_base)),
        (Some(bind), Some(publish_base)) => {
            match bind {
                BindCfg::Match { .. } | BindCfg::Local => (),
                BindCfg::Exact(_) => {
                    panic!("exact bindcfgs are not supported for this publisher")
                }
            }
            Some((Some(bind), publish_base))
        }
        (Some(_), None) => {
            panic!("you must specify bind and publish_base to publish an archive")
        }
    };
    if params.spec.is_empty() && publish_args.is_none() {
        panic!("you must specify a publish config, some paths to log, or both")
    }
    let spec = params
        .spec
        .into_iter()
        .map(Chars::from)
        .map(Glob::new)
        .collect::<Result<Vec<Glob>>>()
        .unwrap();
    let rt = Runtime::new().expect("failed to init tokio runtime");
    rt.block_on(run_async(
        config,
        publish_args,
        auth,
        image_frequency,
        poll_interval,
        flush_frequency,
        flush_interval,
        params.shards,
        params.max_sessions,
        params.max_sessions_per_client,
        params.archive,
        spec,
    ))
}
