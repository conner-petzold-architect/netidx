use crate::{
    path::Path,
    pool::Pooled,
    protocol::resolver::{Auth, Referral},
    publisher,
    subscriber::DesiredAuth,
    tls, utils,
};
use anyhow::Result;
use log::debug;
use serde_json::from_str;
use std::{
    cmp::min,
    collections::BTreeMap,
    convert::AsRef,
    convert::Into,
    env,
    fs::read_to_string,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    str,
};

/// The on disk format, encoded as JSON
mod file {
    use crate::chars::Chars;
    use std::{collections::BTreeMap, net::SocketAddr};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) enum Auth {
        Anonymous,
        Krb5(String),
        Local(String),
        Tls(String),
    }

    impl Into<crate::protocol::resolver::Auth> for Auth {
        fn into(self) -> crate::protocol::resolver::Auth {
            use crate::protocol::resolver::Auth as A;
            match self {
                Self::Anonymous => A::Anonymous,
                Self::Krb5(spn) => A::Krb5 { spn: Chars::from(spn) },
                Self::Local(path) => A::Local { path: Chars::from(path) },
                Self::Tls(name) => A::Tls { name: Chars::from(name) },
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct TlsIdentity {
        pub trusted: String,
        pub certificate: String,
        pub private_key: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Tls {
        #[serde(default)]
        pub default_identity: Option<String>,
        pub identities: BTreeMap<String, TlsIdentity>,
        #[serde(default)]
        pub askpass: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Config {
        pub(super) base: String,
        pub(super) addrs: Vec<(SocketAddr, Auth)>,
        #[serde(default)]
        pub(super) tls: Option<Tls>,
        #[serde(default)]
        pub(super) default_auth: super::DefaultAuthMech,
        #[serde(default)]
        pub(super) default_bind_config: Option<String>,
    }
}

#[derive(Debug, Clone)]
pub struct TlsIdentity {
    pub trusted: String,
    pub name: String,
    pub certificate: String,
    pub private_key: String,
}

#[derive(Debug, Clone)]
pub struct Tls {
    pub default_identity: String,
    pub identities: BTreeMap<String, TlsIdentity>,
    pub askpass: Option<String>,
}

impl Tls {
    // e.g. marketdata.architect.com => com.architect.marketdata.
    pub(crate) fn reverse_domain_name(name: &mut String) {
        const MAX: usize = 1024;
        let mut tmp = [0u8; MAX + 1];
        let mut i = 0;
        for part in name[0..min(name.len(), MAX)].split('.').rev() {
            tmp[i..i + part.len()].copy_from_slice(part.as_bytes());
            tmp[i + part.len()] = '.' as u8;
            i += part.len() + 1;
        }
        name.clear();
        name.push_str(str::from_utf8(&mut tmp[0..i]).unwrap());
    }

    pub(crate) fn default_identity(&self) -> &TlsIdentity {
        &self.identities[&self.default_identity]
    }

    fn load(t: file::Tls) -> Result<Self> {
        use std::fs;
        if t.identities.len() == 0 {
            bail!("at least one identity is required for tls authentication")
        }
        if let Some(askpass) = &t.askpass {
            if let Err(e) = fs::File::open(askpass) {
                bail!("{} askpass can't be read {}", askpass, e)
            }
        }
        let mut default_identity = match t.default_identity {
            Some(id) => {
                if !t.identities.contains_key(&id) {
                    bail!("the default identity must exist")
                }
                id
            }
            None => {
                if t.identities.len() > 1 {
                    bail!("default_identity must be specified")
                } else {
                    t.identities.keys().next().unwrap().clone()
                }
            }
        };
        Self::reverse_domain_name(&mut default_identity);
        let mut identities = BTreeMap::new();
        for (mut name, id) in t.identities {
            if let Err(e) = tls::load_certs(&id.trusted) {
                bail!("trusted certs {} cannot be read {}", id.trusted, e)
            }
            let cn = match tls::load_certs(&id.certificate) {
                Err(e) => bail!("certificate can't be read {}", e),
                Ok(certs) => {
                    if certs.len() == 0 || certs.len() > 1 {
                        bail!("certificate file should contain 1 cert")
                    }
                    match tls::get_common_name(&certs[0].0)? {
                        Some(name) => name,
                        None => bail!("certificate has no common name"),
                    }
                }
            };
            if let Err(e) = fs::File::open(&id.private_key) {
                bail!("{} private_key can't be read {}", name, e)
            }
            Self::reverse_domain_name(&mut name);
            identities.insert(
                name,
                TlsIdentity {
                    trusted: id.trusted,
                    name: cn,
                    certificate: id.certificate,
                    private_key: id.private_key,
                },
            );
        }
        Ok(Tls { askpass: t.askpass, default_identity, identities })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DefaultAuthMech {
    Anonymous,
    Local,
    Krb5,
    Tls,
}

impl Default for DefaultAuthMech {
    fn default() -> Self {
        DefaultAuthMech::Krb5
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base: Path,
    pub addrs: Vec<(SocketAddr, Auth)>,
    pub tls: Option<Tls>,
    pub default_auth: DefaultAuthMech,
    pub default_bind_config: publisher::BindCfg,
}

impl Config {
    pub fn parse(s: &str) -> Result<Config> {
        let cfg: file::Config = from_str(s)?;
        if cfg.addrs.is_empty() {
            bail!("you must specify at least one address");
        }
        match cfg.default_auth {
            DefaultAuthMech::Anonymous
            | DefaultAuthMech::Local
            | DefaultAuthMech::Krb5 => (),
            DefaultAuthMech::Tls => {
                if cfg.tls.is_none() {
                    bail!("tls identities require for tls auth")
                }
            }
        }
        let tls = match cfg.tls {
            Some(tls) => Some(Tls::load(tls)?),
            None => None,
        };
        for (addr, auth) in &cfg.addrs {
            use file::Auth as FAuth;
            utils::check_addr::<()>(addr.ip(), &[])?;
            match auth {
                FAuth::Anonymous | FAuth::Krb5(_) => (),
                FAuth::Tls(name) => match &tls {
                    None => bail!("tls auth requires a valid tls configuration"),
                    Some(tls) => {
                        let mut rev_name = name.clone();
                        Tls::reverse_domain_name(&mut rev_name);
                        if tls::get_match(&tls.identities, &rev_name).is_none() {
                            bail!(
                                "required identity for {} not found in tls identities",
                                name
                            )
                        }
                    }
                },
                FAuth::Local(_) => {
                    if !addr.ip().is_loopback() {
                        bail!("local auth is not allowed for remote servers")
                    }
                }
            }
        }
        if !cfg.addrs.iter().all(|(a, _)| a.ip().is_loopback())
            && !cfg.addrs.iter().all(|(a, _)| !a.ip().is_loopback())
        {
            bail!("can't mix loopback addrs with non loopback addrs")
        }
        Ok(Config {
            base: Path::from(cfg.base),
            addrs: cfg.addrs.into_iter().map(|(s, a)| (s, a.into())).collect(),
            tls,
            default_auth: cfg.default_auth,
            default_bind_config: match cfg.default_bind_config {
                None => publisher::BindCfg::default(),
                Some(s) => s.parse()?,
            },
        })
    }

    pub fn default_auth(&self) -> DesiredAuth {
        match self.default_auth {
            DefaultAuthMech::Anonymous => DesiredAuth::Anonymous,
            DefaultAuthMech::Local => DesiredAuth::Local,
            DefaultAuthMech::Krb5 => DesiredAuth::Krb5 { upn: None, spn: None },
            DefaultAuthMech::Tls => DesiredAuth::Tls { identity: None },
        }
    }

    /// Load the cluster config from the specified file.
    pub fn load<P: AsRef<FsPath>>(file: P) -> Result<Config> {
        Config::parse(&read_to_string(file)?)
    }

    pub fn to_referral(self) -> Referral {
        Referral { path: self.base, ttl: None, addrs: Pooled::orphan(self.addrs) }
    }

    /// This will try in order,
    ///
    /// * $NETIDX_CFG
    /// * ${dirs::config_dir}/netidx/client.json
    /// * ${dirs::home_dir}/.config/netidx/client.json
    /// * C:\netidx\client.json on windows
    /// * /etc/netidx/client.json on unix
    ///
    /// It will load the first file that exists, if that file fails to
    /// load then Err will be returned.
    pub fn load_default() -> Result<Config> {
        if let Some(cfg) = env::var_os("NETIDX_CFG") {
            let cfg = PathBuf::from(cfg);
            if cfg.is_file() {
                debug!("loading {}", cfg.to_string_lossy());
                return Config::load(cfg);
            }
        }
        if let Some(mut cfg) = dirs::config_dir() {
            cfg.push("netidx");
            cfg.push("client.json");
            if cfg.is_file() {
                debug!("loading {}", cfg.to_string_lossy());
                return Config::load(cfg);
            }
        }
        if let Some(mut home) = dirs::home_dir() {
            home.push(".config");
            home.push("netidx");
            home.push("client.json");
            if home.is_file() {
                debug!("loading {}", home.to_string_lossy());
                return Config::load(home);
            }
        }
        let dir = if cfg!(windows) {
            PathBuf::from("C:\\netidx\\client.json")
        } else {
            PathBuf::from("/etc/netidx/client.json")
        };
        if dir.is_file() {
            debug!("loading {}", dir.to_string_lossy());
            return Config::load(dir);
        }
        bail!("no default config file was found")
    }
}
