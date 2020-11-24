use crate::{
    config::Config,
    path::Path,
    pool::{Pool, Pooled},
    protocol::resolver::v1::{FromRead, FromWrite, Referral, ToRead, ToWrite},
    resolver_single::{
        ResolverRead as SingleRead, ResolverWrite as SingleWrite, RAWFROMREADPOOL,
        RAWFROMWRITEPOOL,
    },
    utils,
};
pub use crate::{
    protocol::resolver::v1::{Resolved, Table},
    resolver_single::Auth,
};
use anyhow::Result;
use futures::future;
use fxhash::FxBuildHasher;
use globset::{self, GlobBuilder, GlobMatcher};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{
        BTreeMap,
        Bound::{self, Included, Unbounded},
        HashMap,
    },
    iter::IntoIterator,
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    result,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::oneshot, time::Instant};

const MAX_REFERRALS: usize = 128;

trait ToPath {
    fn path(&self) -> Option<&Path>;
}

impl ToPath for ToRead {
    fn path(&self) -> Option<&Path> {
        match self {
            ToRead::List(p) | ToRead::Table(p) | ToRead::Resolve(p) => Some(p),
        }
    }
}

impl ToPath for ToWrite {
    fn path(&self) -> Option<&Path> {
        match self {
            ToWrite::Clear | ToWrite::Heartbeat => None,
            ToWrite::Publish(p) | ToWrite::Unpublish(p) | ToWrite::PublishDefault(p) => {
                Some(p)
            }
        }
    }
}

#[derive(Debug)]
struct Router {
    cached: BTreeMap<Path, (Instant, Referral)>,
}

impl Router {
    fn new() -> Self {
        Router { cached: BTreeMap::new() }
    }

    fn route_batch<T>(
        &mut self,
        pool: &Pool<Vec<(usize, T)>>,
        batch: &Pooled<Vec<T>>,
    ) -> impl Iterator<Item = (Option<Path>, Pooled<Vec<(usize, T)>>)>
    where
        T: ToPath + Clone + Send + Sync + 'static,
    {
        let now = Instant::now();
        let mut batches = HashMap::new();
        let mut gc = Vec::new();
        let mut id = 0;
        for v in batch.iter() {
            let v = v.clone();
            match v.path() {
                None => batches.entry(None).or_insert_with(|| pool.take()).push((id, v)),
                Some(path) => {
                    let mut r = self.cached.range::<str, (Bound<&str>, Bound<&str>)>((
                        Unbounded,
                        Included(&*path),
                    ));
                    loop {
                        match r.next_back() {
                            None => {
                                batches
                                    .entry(None)
                                    .or_insert_with(|| pool.take())
                                    .push((id, v));
                                break;
                            }
                            Some((p, (exp, _))) => {
                                if !path.starts_with(p.as_ref()) {
                                    continue;
                                } else {
                                    if &now < exp {
                                        batches
                                            .entry(Some(p.clone()))
                                            .or_insert_with(|| pool.take())
                                            .push((id, v))
                                    } else {
                                        gc.push(p.clone());
                                        batches
                                            .entry(None)
                                            .or_insert_with(|| pool.take())
                                            .push((id, v))
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            id += 1;
        }
        for p in gc {
            self.cached.remove(p.as_ref());
        }
        batches.into_iter().map(|(p, batch)| match p {
            None => (None, batch),
            Some(p) => (Some(p), batch),
        })
    }

    fn get_referral(&self, path: &Path) -> Option<&Referral> {
        self.cached.get(path.as_ref()).map(|(_, r)| r)
    }

    fn add_referral(&mut self, r: Referral) {
        let exp = Instant::now() + Duration::from_secs(r.ttl);
        self.cached.insert(r.path.clone(), (exp, r));
    }
}

trait ToReferral: Sized {
    fn referral(self) -> result::Result<Referral, Self>;
}

impl ToReferral for FromRead {
    fn referral(self) -> result::Result<Referral, Self> {
        match self {
            FromRead::Referral(r) => Ok(r),
            m => Err(m),
        }
    }
}

impl ToReferral for FromWrite {
    fn referral(self) -> result::Result<Referral, Self> {
        match self {
            FromWrite::Referral(r) => Ok(r),
            m => Err(m),
        }
    }
}

trait Connection<T, F>
where
    T: ToPath + Send + Sync + 'static,
    F: ToReferral + Send + Sync + 'static,
{
    fn new(
        resolver: Config,
        desired_auth: Auth,
        writer_addr: SocketAddr,
        secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    ) -> Self;
    fn send(
        &mut self,
        batch: Pooled<Vec<(usize, T)>>,
    ) -> oneshot::Receiver<Pooled<Vec<(usize, F)>>>;
}

impl Connection<ToRead, FromRead> for SingleRead {
    fn new(
        resolver: Config,
        desired_auth: Auth,
        _writer_addr: SocketAddr,
        _secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    ) -> Self {
        SingleRead::new(resolver, desired_auth)
    }

    fn send(
        &mut self,
        batch: Pooled<Vec<(usize, ToRead)>>,
    ) -> oneshot::Receiver<Pooled<Vec<(usize, FromRead)>>> {
        SingleRead::send(self, batch)
    }
}

impl Connection<ToWrite, FromWrite> for SingleWrite {
    fn new(
        resolver: Config,
        desired_auth: Auth,
        writer_addr: SocketAddr,
        secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    ) -> Self {
        SingleWrite::new(resolver, desired_auth, writer_addr, secrets)
    }

    fn send(
        &mut self,
        batch: Pooled<Vec<(usize, ToWrite)>>,
    ) -> oneshot::Receiver<Pooled<Vec<(usize, FromWrite)>>> {
        SingleWrite::send(self, batch)
    }
}

lazy_static! {
    static ref RAWTOREADPOOL: Pool<Vec<ToRead>> = Pool::new(1000, 10000);
    static ref TOREADPOOL: Pool<Vec<(usize, ToRead)>> = Pool::new(1000, 10000);
    static ref FROMREADPOOL: Pool<Vec<(usize, FromRead)>> = Pool::new(1000, 10000);
    static ref RAWTOWRITEPOOL: Pool<Vec<ToWrite>> = Pool::new(1000, 10000);
    static ref TOWRITEPOOL: Pool<Vec<(usize, ToWrite)>> = Pool::new(1000, 10000);
    static ref FROMWRITEPOOL: Pool<Vec<(usize, FromWrite)>> = Pool::new(1000, 10000);
    static ref RESOLVEDPOOL: Pool<Vec<Resolved>> = Pool::new(1000, 10000);
    static ref LISTPOOL: Pool<Vec<Pooled<Vec<Path>>>> = Pool::new(1000, 10000);
}

#[derive(Debug)]
struct ResolverWrapInner<C, T, F>
where
    T: Send + Sync + 'static,
    F: Send + Sync + 'static,
{
    router: Router,
    desired_auth: Auth,
    default: C,
    by_path: HashMap<Path, C>,
    writer_addr: SocketAddr,
    secrets: Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>>,
    phantom: PhantomData<(T, F)>,
    f_pool: Pool<Vec<F>>,
    fi_pool: Pool<Vec<(usize, F)>>,
    ti_pool: Pool<Vec<(usize, T)>>,
}

#[derive(Debug, Clone)]
struct ResolverWrap<C, T: Send + Sync + 'static, F: Send + Sync + 'static>(
    Arc<Mutex<ResolverWrapInner<C, T, F>>>,
);

impl<C, T, F> ResolverWrap<C, T, F>
where
    C: Connection<T, F> + Clone + 'static,
    T: ToPath + Clone + Send + Sync + 'static,
    F: ToReferral + Clone + Send + Sync + 'static,
{
    fn new(
        default: Config,
        desired_auth: Auth,
        writer_addr: SocketAddr,
        f_pool: Pool<Vec<F>>,
        fi_pool: Pool<Vec<(usize, F)>>,
        ti_pool: Pool<Vec<(usize, T)>>,
    ) -> ResolverWrap<C, T, F> {
        let secrets =
            Arc::new(RwLock::new(HashMap::with_hasher(FxBuildHasher::default())));
        let router = Router::new();
        let default = C::new(default, desired_auth.clone(), writer_addr, secrets.clone());
        ResolverWrap(Arc::new(Mutex::new(ResolverWrapInner {
            router,
            desired_auth,
            default,
            by_path: HashMap::new(),
            writer_addr,
            secrets,
            f_pool,
            fi_pool,
            ti_pool,
            phantom: PhantomData,
        })))
    }

    fn secrets(&self) -> Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>> {
        self.0.lock().secrets.clone()
    }

    async fn send(&self, batch: &Pooled<Vec<T>>) -> Result<Pooled<Vec<F>>> {
        let mut referrals = 0;
        loop {
            let mut waiters = Vec::new();
            let (mut finished, mut res) = {
                let mut guard = self.0.lock();
                let inner = &mut *guard;
                if inner.by_path.len() > MAX_REFERRALS {
                    inner.by_path.clear(); // a workable sledgehammer
                }
                for (r, batch) in inner.router.route_batch(&inner.ti_pool, batch) {
                    match r {
                        None => waiters.push(inner.default.send(batch)),
                        Some(rp) => match inner.by_path.get_mut(&rp) {
                            Some(con) => waiters.push(con.send(batch)),
                            None => {
                                let r = inner.router.get_referral(&rp).unwrap().clone();
                                let mut con = C::new(
                                    Config::from(r),
                                    inner.desired_auth.clone(),
                                    inner.writer_addr,
                                    inner.secrets.clone(),
                                );
                                inner.by_path.insert(rp, con.clone());
                                waiters.push(con.send(batch))
                            }
                        },
                    }
                }
                (inner.fi_pool.take(), inner.f_pool.take())
            };
            let qresult = future::join_all(waiters).await;
            let mut referral = false;
            for r in qresult {
                let mut r = r?;
                for (id, reply) in r.drain(..) {
                    match reply.referral() {
                        Ok(r) => {
                            self.0.lock().router.add_referral(r);
                            referral = true;
                        }
                        Err(m) => finished.push((id, m)),
                    }
                }
            }
            if !referral {
                finished.sort_by_key(|(id, _)| *id);
                res.extend(finished.drain(..).map(|(_, m)| m));
                break Ok(res);
            }
            referrals += 1;
            if referrals > MAX_REFERRALS {
                bail!("maximum referral depth {} reached, giving up", MAX_REFERRALS);
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Scoped {
    OneLevel(Vec<GlobMatcher>),
    Subtree(GlobMatcher),
}

/// Unix style globs for matching paths in the resolver. All common
/// unix globing features are supported.
/// * ? matches any character except the path separator
/// * * matches zero or more characters, but not the path separator
/// * ** recursively matches containers. It's only legal uses are /**/foo, /foo/**/bar, and /foo/bar/**, which match respectively, any path ending in foo, any path starting with /foo and ending in bar, and any path starting with /foo/bar.
/// * {a, b}, matches a or b where a and b are glob patterns, but not **, and not a nested {} patteren.
/// * [ab], [!ab], matches respectively the char a or b, and any char but a or b.
/// * any of the above metacharacters can be escaped with a \, a literal \ may be produced with \\.
///
/// e.g.
/// `/solar/{stats,settings}/*` -> all leaf paths under /solar/stats or /solar/settings.
/// `/s*/s*/**` -> any path who's first two levels start with s.
/// `/**/?` -> any path who's final component is a single character
/// `/marketdata/{IBM,MSFT,AMZN}/last`
#[derive(Debug, Clone)]
pub struct Glob {
    raw: Path,
    base: Path,
    scoped: Scoped,
}

impl Glob {
    pub fn new(raw: Path) -> Result<Glob> {
        if !Path::is_absolute(&raw) {
            bail!("glob paths must be absolute")
        }
        let (lvl, base) =
            Path::dirnames(&raw).enumerate().fold((0, "/"), |cur, (lvl, p)| {
                let mut s = p;
                let is_glob = loop {
                    if s.is_empty() {
                        break false;
                    } else {
                        match s.find("?*{[") {
                            None => break false,
                            Some(i) => {
                                if utils::is_escaped(s, '\\', i) {
                                    s = &s[i + 1..];
                                } else {
                                    break true;
                                }
                            }
                        }
                    }
                };
                if !is_glob {
                    (lvl, p)
                } else {
                    cur
                }
            });
        let base = Path::from(String::from(base));
        let sc =
            if Path::dirnames(&raw).skip(lvl).any(|p| Path::basename(p) == Some("**")) {
                let g = GlobBuilder::new(&raw)
                    .backslash_escape(true)
                    .literal_separator(true)
                    .build()?
                    .compile_matcher();
                Scoped::Subtree(g)
            } else {
                let g = Path::dirnames(&raw)
                    .skip(lvl)
                    .map(|p| {
                        Ok(GlobBuilder::new(p)
                            .backslash_escape(true)
                            .literal_separator(true)
                            .build()?
                            .compile_matcher())
                    })
                    .collect::<Result<Vec<GlobMatcher>>>()?;
                Scoped::OneLevel(g)
            };
        Ok(Glob { raw, base, scoped: sc })
    }
}

#[derive(Debug, Clone)]
pub struct ResolverRead(ResolverWrap<SingleRead, ToRead, FromRead>);

impl ResolverRead {
    pub fn new(default: Config, desired_auth: Auth) -> Self {
        ResolverRead(ResolverWrap::new(
            default,
            desired_auth,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0),
            RAWFROMREADPOOL.clone(),
            FROMREADPOOL.clone(),
            TOREADPOOL.clone(),
        ))
    }

    /// send the specified messages to the resolver, and return the answers (in send order)
    pub async fn send(
        &self,
        batch: &Pooled<Vec<ToRead>>,
    ) -> Result<Pooled<Vec<FromRead>>> {
        self.0.send(batch).await
    }

    /// resolve the specified paths, results are in send order
    pub async fn resolve<I>(&self, batch: I) -> Result<Pooled<Vec<Resolved>>>
    where
        I: IntoIterator<Item = Path>,
    {
        let mut to = RAWTOREADPOOL.take();
        to.extend(batch.into_iter().map(ToRead::Resolve));
        let mut result = self.send(&to).await?;
        if result.len() != to.len() {
            bail!(
                "unexpected number of resolve results {} expected {}",
                result.len(),
                to.len()
            )
        } else {
            let mut out = RESOLVEDPOOL.take();
            for r in result.drain(..) {
                match r {
                    FromRead::Resolved(r) => {
                        out.push(r);
                    }
                    m => bail!("unexpected resolve response {:?}", m),
                }
            }
            Ok(out)
        }
    }

    /// list children of the specified path. Order is unspecified.
    pub async fn list(&self, path: Path) -> Result<Pooled<Vec<Path>>> {
        let mut to = RAWTOREADPOOL.take();
        to.push(ToRead::List(path));
        let mut result = self.send(&to).await?;
        if result.len() != 1 {
            bail!("expected 1 result from list got {}", result.len());
        } else {
            match result.pop().unwrap() {
                FromRead::List(paths) => Ok(paths),
                m => bail!("unexpected result from list {:?}", m),
            }
        }
    }

    /// list the children of the specified paths, results in send order.
    pub async fn list_many<I>(&self, batch: I) -> Result<Pooled<Vec<Pooled<Vec<Path>>>>>
    where
        I: IntoIterator<Item = Path>,
    {
        let mut to = RAWTOREADPOOL.take();
        to.extend(batch.into_iter().map(ToRead::List));
        let mut result = self.send(&to).await?;
        if result.len() != to.len() {
            bail!(
                "expected number of list results {} expected {}",
                result.len(),
                to.len()
            );
        } else {
            let mut out = LISTPOOL.take();
            for l in result.drain(..) {
                match l {
                    FromRead::List(paths) => {
                        out.push(paths);
                    }
                    m => bail!("unexpected list response {:?}", m),
                }
            }
            Ok(out)
        }
    }

    /// list all the non leaf children of the specified path to any
    /// depth, result order unspecified.
    pub async fn list_recursive(&self, base: Path) -> Result<Vec<Path>> {
        let mut leaf = Vec::new();
        let mut query = vec![base];
        while query.len() > 0 {
            let mut res = self.list_many(query.iter().cloned()).await?;
            let mut new_query = Vec::new();
            for (p, mut res) in query.iter().zip(res.drain(..)) {
                if res.is_empty() {
                    leaf.push(p.clone());
                } else {
                    new_query.extend(res.drain(..));
                }
            }
            query = new_query;
        }
        Ok(leaf)
    }

    /// Search the resolver server for paths matching the specified
    /// unix style `Glob`.
    pub async fn list_glob(&self, glob: &Glob) -> Result<Vec<Path>> {
        let mut matches = Vec::new();
        match &glob.scoped {
            Scoped::Subtree(pat) => {
                for p in self.list_recursive(glob.base.clone()).await?.drain(..) {
                    if pat.is_match(&*p) {
                        matches.push(p);
                    }
                }
            }
            Scoped::OneLevel(pats) => {
                let mut query = vec![glob.base.clone()];
                for pat in pats.iter() {
                    let mut res = self.list_many(query.iter().cloned()).await?;
                    let mut new_query = Vec::new();
                    for (p, mut res) in query.iter().zip(res.drain(..)) {
                        if res.is_empty() {
                            matches.push(p.clone());
                        } else {
                            new_query.extend(
                                res.drain(..).filter(|p| pat.is_match(p.as_ref())),
                            );
                        }
                    }
                    query = new_query;
                }
            }
        }
        Ok(matches)
    }

    pub async fn table(&self, path: Path) -> Result<Table> {
        let mut to = RAWTOREADPOOL.take();
        to.push(ToRead::Table(path));
        let mut result = self.send(&to).await?;
        if result.len() != 1 {
            bail!("expected 1 result from table got {}", result.len());
        } else {
            match result.pop().unwrap() {
                FromRead::Table(table) => Ok(table),
                m => bail!("unexpected result from table {:?}", m),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolverWrite(ResolverWrap<SingleWrite, ToWrite, FromWrite>);

impl ResolverWrite {
    pub fn new(default: Config, desired_auth: Auth, writer_addr: SocketAddr) -> Self {
        ResolverWrite(ResolverWrap::new(
            default,
            desired_auth,
            writer_addr,
            RAWFROMWRITEPOOL.clone(),
            FROMWRITEPOOL.clone(),
            TOWRITEPOOL.clone(),
        ))
    }

    pub async fn send(
        &self,
        batch: &Pooled<Vec<ToWrite>>,
    ) -> Result<Pooled<Vec<FromWrite>>> {
        self.0.send(batch).await
    }

    async fn send_expect<F, I>(&self, batch: I, f: F, expected: FromWrite) -> Result<()>
    where
        F: Fn(Path) -> ToWrite,
        I: IntoIterator<Item = Path>,
    {
        let mut to = RAWTOWRITEPOOL.take();
        let len = to.len();
        to.extend(batch.into_iter().map(f));
        let mut from = self.0.send(&to).await?;
        if from.len() != to.len() {
            bail!("unexpected number of responses {} vs expected {}", from.len(), len);
        }
        for (i, reply) in from.drain(..).enumerate() {
            if reply != expected {
                bail!("unexpected response to {:?}, {:?}", &to[i], reply)
            }
        }
        Ok(())
    }

    pub async fn publish<I: IntoIterator<Item = Path>>(&self, batch: I) -> Result<()> {
        self.send_expect(batch, ToWrite::Publish, FromWrite::Published).await
    }

    pub async fn publish_default<I: IntoIterator<Item = Path>>(
        &self,
        batch: I,
    ) -> Result<()> {
        self.send_expect(batch, ToWrite::PublishDefault, FromWrite::Published).await
    }

    pub async fn unpublish<I: IntoIterator<Item = Path>>(&self, batch: I) -> Result<()> {
        self.send_expect(batch, ToWrite::Unpublish, FromWrite::Unpublished).await
    }

    pub async fn clear(&self) -> Result<()> {
        let mut batch = RAWTOWRITEPOOL.take();
        batch.push(ToWrite::Clear);
        let r = self.0.send(&batch).await?;
        if r.len() != 1 {
            bail!("unexpected response to clear command {:?}", r)
        } else {
            match &r[0] {
                FromWrite::Unpublished => Ok(()),
                m => bail!("unexpected response to clear command {:?}", m),
            }
        }
    }

    pub(crate) fn secrets(
        &self,
    ) -> Arc<RwLock<HashMap<SocketAddr, u128, FxBuildHasher>>> {
        self.0.secrets()
    }
}
