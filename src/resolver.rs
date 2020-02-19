use crate::{
    channel::Channel,
    path::Path,
    protocol::resolver,
    config,
};
use failure::Error;
use futures::{prelude::*, select};
use std::{
    collections::HashSet,
    marker::PhantomData,
    net::SocketAddr,
    result,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    task,
    time::{self, Instant},
};

static TTL: u64 = 600;
static LINGER: u64 = 10;

type FromCon = oneshot::Sender<resolver::From>;
type ToCon = (resolver::To, FromCon);

pub trait Readable {}
pub trait Writeable {}
pub trait ReadableOrWritable {}

#[derive(Debug, Clone)]
pub struct ReadOnly {}

impl Readable for ReadOnly {}
impl ReadableOrWritable for ReadOnly {}

#[derive(Debug, Clone)]
pub struct WriteOnly {}

impl Writeable for WriteOnly {}
impl ReadableOrWritable for WriteOnly {}

type Result<T> = result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Resolver<R> {
    sender: mpsc::UnboundedSender<ToCon>,
    kind: PhantomData<R>,
}

impl<R: ReadableOrWritable> Resolver<R> {
    async fn send(&mut self, m: resolver::To) -> Result<resolver::From> {
        let (tx, rx) = oneshot::channel();
        self.sender.send((m, tx))?;
        match rx.await? {
            resolver::From::Error(s) => bail!(s),
            m => Ok(m),
        }
    }

    pub fn new_w(
        resolver: config::Resolver,
        publisher: SocketAddr,
    ) -> Result<Resolver<WriteOnly>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        task::spawn(connection(receiver, resolver, Some(publisher)));
        Ok(Resolver {
            sender,
            kind: PhantomData,
        })
    }

    // CR estokes: when given more than one socket address the
    // resolver should make use of all of them.
    pub fn new_r(resolver: config::Resolver) -> Result<Resolver<ReadOnly>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        task::spawn(connection(receiver, resolver, None));
        Ok(Resolver {
            sender,
            kind: PhantomData,
        })
    }
}

impl<R: Readable + ReadableOrWritable> Resolver<R> {
    pub async fn resolve(&mut self, paths: Vec<Path>) -> Result<Vec<Vec<SocketAddr>>> {
        match self.send(resolver::To::Resolve(paths)).await? {
            resolver::From::Resolved(ports) => Ok(ports),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn list(&mut self, p: Path) -> Result<Vec<Path>> {
        match self.send(resolver::To::List(p)).await? {
            resolver::From::List(v) => Ok(v),
            _ => bail!("unexpected response"),
        }
    }
}

impl<R: Writeable + ReadableOrWritable> Resolver<R> {
    pub async fn publish(&mut self, paths: Vec<Path>) -> Result<()> {
        match self.send(resolver::To::Publish(paths)).await? {
            resolver::From::Published => Ok(()),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn unpublish(&mut self, paths: Vec<Path>) -> Result<()> {
        match self.send(resolver::To::Unpublish(paths)).await? {
            resolver::From::Unpublished => Ok(()),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn clear(&mut self) -> Result<()> {
        match self.send(resolver::To::Clear).await? {
            resolver::From::Unpublished => Ok(()),
            _ => bail!("unexpected response"),
        }
    }
}

async fn connect(
    resolver: config::Resolver,
    publisher: Option<SocketAddr>,
    published: &HashSet<Path>,
) -> Channel {
    let mut backoff = 0;
    loop {
        if backoff > 0 {
            time::delay_for(Duration::from_secs(backoff)).await;
        }
        backoff += 1;
        let con = try_cont!("connect", TcpStream::connect(&resolver.addr).await);
        let mut con = Channel::new(con);
        // the unwrap can't fail, we can always encode the message,
        // and it's never bigger then 4 GB.
        try_cont!(
            "hello",
            con.send_one(&match publisher {
                None => resolver::ClientHello::ReadOnly,
                Some(write_addr) => resolver::ClientHello::WriteOnly {
                    ttl: TTL,
                    write_addr
                },
            })
            .await
        );
        let r: resolver::ServerHello = try_cont!("hello reply", con.receive().await);
        if !r.ttl_expired {
            break con;
        } else {
            let m = resolver::To::Publish(published.iter().cloned().collect());
            try_cont!("republish", con.send_one(&m).await);
            match try_cont!("replublish reply", con.receive().await) {
                resolver::From::Published => break con,
                _ => (),
            }
        }
    }
}

async fn connection(
    receiver: mpsc::UnboundedReceiver<ToCon>,
    resolver: config::Resolver,
    publisher: Option<SocketAddr>,
) {
    let mut published = HashSet::new();
    let mut con: Option<Channel> = None;
    let ttl = Duration::from_secs(TTL / 2);
    let linger = Duration::from_secs(LINGER);
    let now = Instant::now();
    let mut act = false;
    let mut receiver = receiver.fuse();
    let mut hb = time::interval_at(now + ttl, ttl).fuse();
    let mut dc = time::interval_at(now + linger, linger).fuse();
    loop {
        select! {
            _ = dc.next() => {
                if act {
                   act = false;
                } else {
                    con = None;
                }
            },
            _ = hb.next() => loop {
                if act {
                    break;
                } else {
                    match con {
                        None => {
                            con = Some(connect(resolver, publisher, &published).await);
                            break;
                        },
                        Some(ref mut c) =>
                            match c.send_one(&resolver::To::Heartbeat).await {
                                Ok(()) => break,
                                Err(_) => { con = None; }
                            }
                    }
                }
            },
            m = receiver.next() => match m {
                None => break,
                Some((m, reply)) => {
                    act = true;
                    let m_r = &m;
                    let r = loop {
                        let c = match con {
                            Some(ref mut c) => c,
                            None => {
                                con = Some(connect(
                                    resolver, publisher, &published).await
                                );
                                con.as_mut().unwrap()
                            }
                        };
                        match c.send_one(m_r).await {
                            Err(_) => { con = None; }
                            Ok(()) => match c.receive().await {
                                Err(_) => { con = None; }
                                Ok(r) => break r,
                                Ok(resolver::From::Published) => {
                                    if let resolver::To::Publish(p) = m_r {
                                        published.extend(p.iter().cloned());
                                    }
                                    break resolver::From::Published
                                }
                            }
                        }
                    };
                    let _ = reply.send(r);
                }
            }
        }
    }
}
