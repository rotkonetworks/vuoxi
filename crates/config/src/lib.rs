//! Device key and configuration file of a vuoxi node.
//!
//! The key is 32 raw bytes in a 0600 file; the config is TOML, written
//! atomically. Peers are named by the base32 form of their public key.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use vuoxi_core::PeerId;

pub const DEFAULT_PORT: u16 = 41641;

#[derive(Debug)]
pub enum Error {
    /// Malformed input: bad id, bad TOML, bad address.
    Invalid(String),
    /// A rule of the config was broken (duplicate, nested folder, ...).
    Conflict(String),
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(s) | Error::Conflict(s) => f.write_str(s),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

fn invalid<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error::Invalid(s.into()))
}

// ---- ids ---------------------------------------------------------------

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const ID_LEN: usize = 52;

/// Lowercase RFC 4648 base32 without padding: 52 characters.
pub fn encode_id(p: &PeerId) -> String {
    let mut out = String::with_capacity(ID_LEN);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in &p.0 {
        acc = acc << 8 | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[(acc >> bits & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[(acc << (5 - bits) & 31) as usize] as char);
    }
    out
}

/// Inverse of [`encode_id`]. Case-insensitive; surrounding whitespace is
/// ignored; anything else off-spec is rejected.
pub fn decode_id(s: &str) -> Result<PeerId, Error> {
    let s = s.trim().to_ascii_lowercase();
    if s.len() != ID_LEN {
        return invalid(format!("id must be {ID_LEN} characters, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    let (mut acc, mut bits, mut n) = (0u32, 0u32, 0usize);
    for c in s.bytes() {
        let Some(v) = B32.iter().position(|&x| x == c) else {
            return invalid(format!("invalid character {:?} in id", c as char));
        };
        acc = acc << 5 | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[n] = (acc >> bits) as u8;
            n += 1;
            acc &= (1 << bits) - 1;
        }
    }
    if acc != 0 {
        return invalid("id has nonzero trailing bits");
    }
    Ok(PeerId(out))
}

// ---- device key --------------------------------------------------------

pub struct DeviceKey {
    pub secret: [u8; 32],
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceKey(..)")
    }
}

impl DeviceKey {
    pub fn generate() -> DeviceKey {
        let mut secret = [0u8; 32];
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut secret))
            .expect("/dev/urandom is readable");
        DeviceKey { secret }
    }

    /// Load the key at `path`, creating it (0600) if absent. Refuses a
    /// symlink, a non-regular file, or any group/other permission bit.
    pub fn load_or_create(path: &Path) -> io::Result<DeviceKey> {
        match Self::load(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            r => return r,
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        let key = Self::generate();
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path);
        match created {
            Ok(mut f) => {
                f.write_all(&key.secret)?;
                f.sync_all()?;
                Ok(key)
            }
            // Lost a race with another creator: use theirs.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(e) => Err(e),
        }
    }

    fn load(path: &Path) -> io::Result<DeviceKey> {
        let bad = |m: String| io::Error::new(io::ErrorKind::PermissionDenied, m);
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(bad(format!("{}: is a symlink", path.display())));
        }
        if !meta.is_file() {
            return Err(bad(format!("{}: not a regular file", path.display())));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(bad(format!(
                "{}: mode {:04o} is too open; chmod 600",
                path.display(),
                meta.mode() & 0o7777
            )));
        }
        let mut buf = Vec::new();
        File::open(path)?.take(33).read_to_end(&mut buf)?;
        let secret: [u8; 32] = buf.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: key must be exactly 32 bytes", path.display()),
            )
        })?;
        Ok(DeviceKey { secret })
    }

    pub fn peer_id(&self) -> PeerId {
        let sk = ed25519_dalek::SigningKey::from_bytes(&self.secret);
        PeerId(sk.verifying_key().to_bytes())
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

// ---- config ------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderConfig {
    pub id: String,
    pub path: PathBuf,
    pub tag: Option<String>,
    /// Names of peers this folder is shared with.
    pub peers: Vec<String>,
    /// Of `peers`, those whose changes are not accepted here: they receive
    /// this folder but cannot change it on this device.
    pub read_only_peers: Vec<String>,
    /// Folder key for relays: they store this folder encrypted under it.
    /// Every device sharing the folder holds the same key; relays never do.
    pub key: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerConfig {
    pub name: String,
    pub id: PeerId,
    pub addrs: Vec<SocketAddr>,
    /// A relay: stores and serves this device's folders encrypted, never reads them.
    pub relay: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub listen: Vec<SocketAddr>,
    pub state_dir: PathBuf,
    pub watcher_socket: Option<PathBuf>,
    pub folders: Vec<FolderConfig>,
    pub peers: Vec<PeerConfig>,
    /// Days to keep files a peer replaced or deleted (`.vuoxi-trash`). 0 keeps none.
    pub trash_days: u32,
    /// Where the dashboard API listens. Loopback only by design.
    pub ui_listen: SocketAddr,
    /// Set on a relay: where it keeps the encrypted folders it serves.
    pub relay_dir: Option<PathBuf>,
}

pub const DEFAULT_TRASH_DAYS: u32 = 30;
pub const DEFAULT_UI_PORT: u16 = 41642;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_listen")]
    listen: Vec<SocketAddr>,
    #[serde(default = "default_state_dir")]
    state_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watcher_socket: Option<PathBuf>,
    #[serde(default, rename = "folder", skip_serializing_if = "Vec::is_empty")]
    folders: Vec<RawFolder>,
    #[serde(default, rename = "peer", skip_serializing_if = "Vec::is_empty")]
    peers: Vec<RawPeer>,
    #[serde(default = "default_trash_days")]
    trash_days: u32,
    #[serde(default = "default_ui_listen")]
    ui_listen: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFolder {
    id: String,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(default)]
    peers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    read_only_peers: Vec<String>,
    /// Hex; see [`FolderConfig::key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPeer {
    name: String,
    id: String,
    #[serde(default)]
    addrs: Vec<SocketAddr>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    relay: bool,
}

fn default_listen() -> Vec<SocketAddr> {
    vec![SocketAddr::from((
        std::net::Ipv6Addr::UNSPECIFIED,
        DEFAULT_PORT,
    ))]
}

fn default_trash_days() -> u32 {
    DEFAULT_TRASH_DAYS
}

fn default_ui_listen() -> SocketAddr {
    SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, DEFAULT_UI_PORT))
}

pub fn encode_key(k: &[u8; 32]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode_key(s: &str) -> Result<[u8; 32], Error> {
    let s = s.trim();
    if s.len() != 64 {
        return invalid("folder key must be 64 hex characters");
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| Error::Invalid("folder key must be hex".into()))?;
    }
    Ok(k)
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn home() -> PathBuf {
    env_dir("HOME").unwrap_or_else(|| PathBuf::from("."))
}

fn default_state_dir() -> PathBuf {
    env_dir("XDG_STATE_HOME")
        .unwrap_or_else(|| home().join(".local/state"))
        .join("vuoxi")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: default_listen(),
            state_dir: default_state_dir(),
            watcher_socket: None,
            folders: Vec::new(),
            peers: Vec::new(),
            trash_days: DEFAULT_TRASH_DAYS,
            ui_listen: default_ui_listen(),
            relay_dir: None,
        }
    }
}

fn valid_folder_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_peer_name(n: &str) -> bool {
    (1..=64).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn check_folder_path(p: &Path) -> Result<(), Error> {
    if !p.is_absolute() {
        return invalid(format!("folder path must be absolute: {}", p.display()));
    }
    if p.components().any(|c| c == Component::ParentDir) {
        return invalid(format!(
            "folder path must not contain `..`: {}",
            p.display()
        ));
    }
    Ok(())
}

impl Config {
    pub fn default_path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn key_path() -> PathBuf {
        config_dir().join("key")
    }

    /// The key file that lives next to a config file.
    pub fn key_path_beside(config: &Path) -> PathBuf {
        config.parent().unwrap_or(Path::new("")).join("key")
    }

    /// A missing file gives the defaults.
    pub fn load(path: &Path) -> Result<Config, Error> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.into()),
        };
        Self::parse(&text).map_err(|e| match e {
            Error::Invalid(m) => Error::Invalid(format!("{}: {m}", path.display())),
            e => e,
        })
    }

    pub fn parse(text: &str) -> Result<Config, Error> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut c = Config {
            listen: raw.listen,
            state_dir: raw.state_dir,
            watcher_socket: raw.watcher_socket,
            folders: Vec::new(),
            peers: Vec::new(),
            trash_days: raw.trash_days,
            ui_listen: raw.ui_listen,
            relay_dir: raw.relay_dir,
        };
        for p in raw.peers {
            let id =
                decode_id(&p.id).map_err(|e| Error::Invalid(format!("peer {}: {e}", p.name)))?;
            c.add_peer(PeerConfig {
                name: p.name,
                id,
                addrs: p.addrs,
                relay: p.relay,
            })?;
        }
        for f in raw.folders {
            let key = f
                .key
                .as_deref()
                .map(decode_key)
                .transpose()
                .map_err(|e| Error::Invalid(format!("folder {}: {e}", f.id)))?;
            c.add_folder(FolderConfig {
                id: f.id,
                path: f.path,
                tag: f.tag,
                peers: f.peers,
                read_only_peers: f.read_only_peers,
                key,
            })?;
        }
        Ok(c)
    }

    pub fn to_toml(&self) -> String {
        let raw = RawConfig {
            listen: self.listen.clone(),
            state_dir: self.state_dir.clone(),
            watcher_socket: self.watcher_socket.clone(),
            trash_days: self.trash_days,
            ui_listen: self.ui_listen,
            relay_dir: self.relay_dir.clone(),
            folders: self
                .folders
                .iter()
                .map(|f| RawFolder {
                    id: f.id.clone(),
                    path: f.path.clone(),
                    tag: f.tag.clone(),
                    peers: f.peers.clone(),
                    read_only_peers: f.read_only_peers.clone(),
                    key: f.key.as_ref().map(encode_key),
                })
                .collect(),
            peers: self
                .peers
                .iter()
                .map(|p| RawPeer {
                    name: p.name.clone(),
                    id: encode_id(&p.id),
                    addrs: p.addrs.clone(),
                    relay: p.relay,
                })
                .collect(),
        };
        toml::to_string_pretty(&raw).expect("config serializes")
    }

    /// Write to a temp file beside `path` (0600), fsync, rename over it.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
        if let Some(d) = dir {
            create_private_dir(d)?;
        }
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "config path has no file name")
        })?;
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(".tmp{}", std::process::id()));
        let tmp = path.with_file_name(tmp_name);
        let _ = fs::remove_file(&tmp);
        let res = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(self.to_toml().as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, path)?;
            if let Some(d) = dir {
                File::open(d)?.sync_all()?;
            }
            Ok(())
        })();
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }

    /// Insert, replacing any peer with the same id or the same name.
    pub fn add_peer(&mut self, p: PeerConfig) -> Result<(), Error> {
        if !valid_peer_name(&p.name) {
            return invalid(format!(
                "bad peer name {:?}: use [A-Za-z0-9._-]{{1,64}}",
                p.name
            ));
        }
        self.peers.retain(|q| q.id != p.id && q.name != p.name);
        self.peers.push(p);
        Ok(())
    }

    pub fn add_folder(&mut self, f: FolderConfig) -> Result<(), Error> {
        if !valid_folder_id(&f.id) {
            return invalid(format!("bad folder id {:?}: use [a-z0-9-]{{1,64}}", f.id));
        }
        check_folder_path(&f.path)?;
        if let Some(p) = f.read_only_peers.iter().find(|p| !f.peers.contains(p)) {
            return invalid(format!(
                "read-only peer {p} is not one of the folder's peers"
            ));
        }
        for g in &self.folders {
            if g.id == f.id {
                return Err(Error::Conflict(format!(
                    "folder id {} already exists",
                    f.id
                )));
            }
            if g.path == f.path {
                return Err(Error::Conflict(format!(
                    "path {} already used by folder {}",
                    f.path.display(),
                    g.id
                )));
            }
            if g.path.starts_with(&f.path) || f.path.starts_with(&g.path) {
                return Err(Error::Conflict(format!(
                    "{} is nested with folder {} ({})",
                    f.path.display(),
                    g.id,
                    g.path.display()
                )));
            }
        }
        self.folders.push(f);
        Ok(())
    }
}

fn config_dir() -> PathBuf {
    env_dir("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home().join(".config"))
        .join("vuoxi")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let d = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp"))
            .join(format!("config-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn folder(id: &str, path: &str) -> FolderConfig {
        FolderConfig {
            id: id.into(),
            path: path.into(),
            tag: None,
            peers: vec![],
            read_only_peers: vec![],
            key: None,
        }
    }

    #[test]
    fn id_round_trip_and_rejection() {
        for seed in [[0u8; 32], [0xff; 32], DeviceKey::generate().secret] {
            let p = PeerId(seed);
            let s = encode_id(&p);
            assert_eq!(s.len(), 52);
            assert!(s.bytes().all(|b| B32.contains(&b)));
            assert_eq!(decode_id(&s).unwrap(), p);
            assert_eq!(decode_id(&s.to_uppercase()).unwrap(), p);
            assert_eq!(decode_id(&format!(" {s}\n")).unwrap(), p);
        }
        let ok = encode_id(&PeerId([7; 32]));
        assert!(decode_id("").is_err());
        assert!(decode_id(&ok[1..]).is_err());
        assert!(decode_id(&format!("{ok}a")).is_err());
        assert!(decode_id(&format!("1{}", &ok[1..])).is_err());
        assert!(decode_id(&format!("{}=", &ok[..51])).is_err());
        // last char carries 4 padding bits that must be zero
        let mut bad = ok.clone();
        bad.replace_range(51.., "b");
        assert!(decode_id(&bad).is_err());
    }

    #[test]
    fn key_create_load_perms() {
        let d = tmp("key");
        let path = d.join("sub/key");
        let k = DeviceKey::load_or_create(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let again = DeviceKey::load_or_create(&path).unwrap();
        assert_eq!(k.secret, again.secret);
        assert_eq!(k.peer_id(), again.peer_id());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let e = DeviceKey::load_or_create(&path).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(DeviceKey::load_or_create(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(DeviceKey::load_or_create(&path).is_ok());
    }

    #[test]
    fn key_refuses_symlink_and_bad_length() {
        let d = tmp("key-sym");
        let real = d.join("real");
        DeviceKey::load_or_create(&real).unwrap();
        let link = d.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = DeviceKey::load_or_create(&link).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);

        let short = d.join("short");
        fs::write(&short, b"abc").unwrap();
        fs::set_permissions(&short, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            DeviceKey::load_or_create(&short).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn peer_id_is_ed25519_public_key() {
        // RFC 8032 test vector 1.
        let secret: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        let id = DeviceKey { secret }.peer_id();
        assert_eq!(id.0[..4], [0xd7, 0x5a, 0x98, 0x01]);
    }

    #[test]
    fn config_round_trip() {
        let d = tmp("rt");
        let path = d.join("c/config.toml");
        let mut c = Config {
            watcher_socket: Some("/run/vuoxi/root.sock".into()),
            ..Config::default()
        };
        c.add_peer(PeerConfig {
            name: "laptop".into(),
            id: PeerId([3; 32]),
            addrs: vec!["[2001:db8::1]:41641".parse().unwrap()],
            relay: false,
        })
        .unwrap();
        let mut f = folder("docs", "/data/docs");
        f.tag = Some("work".into());
        f.peers = vec!["laptop".into()];
        f.read_only_peers = vec!["laptop".into()];
        f.key = Some([7; 32]);
        c.add_folder(f).unwrap();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn load_missing_is_default_and_bad_toml_errors() {
        let d = tmp("load");
        let c = Config::load(&d.join("nope.toml")).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.listen, ["[::]:41641".parse::<SocketAddr>().unwrap()]);
        fs::write(d.join("bad.toml"), "listen = 3").unwrap();
        assert!(matches!(
            Config::load(&d.join("bad.toml")),
            Err(Error::Invalid(_))
        ));
        fs::write(d.join("typo.toml"), "lisen = []").unwrap();
        assert!(Config::load(&d.join("typo.toml")).is_err());
        fs::write(d.join("id.toml"), "[[peer]]\nname='a'\nid='zz'\n").unwrap();
        assert!(Config::load(&d.join("id.toml")).is_err());
    }

    #[test]
    fn add_folder_validation() {
        let mut c = Config::default();
        for bad in ["", "Docs", "a_b", "a b", &"x".repeat(65)] {
            assert!(
                matches!(c.add_folder(folder(bad, "/a")), Err(Error::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(c.add_folder(folder("a", "rel/path")).is_err());
        assert!(c.add_folder(folder("a", "/x/../y")).is_err());
        c.add_folder(folder("a", "/data/a")).unwrap();
        assert!(c.add_folder(folder("a", "/data/b")).is_err(), "dup id");
        assert!(c.add_folder(folder("b", "/data/a")).is_err(), "dup path");
        assert!(c.add_folder(folder("b", "/data/a/sub")).is_err(), "inside");
        assert!(c.add_folder(folder("b", "/data")).is_err(), "contains");
        c.add_folder(folder("b", "/data/ab")).unwrap(); // prefix of name, not of path
        assert_eq!(c.folders.len(), 2);
    }

    #[test]
    fn add_peer_replaces_by_id_or_name() {
        let mut c = Config::default();
        let p = |n: &str, b: u8| PeerConfig {
            name: n.into(),
            id: PeerId([b; 32]),
            addrs: vec![],
            relay: false,
        };
        c.add_peer(p("a", 1)).unwrap();
        c.add_peer(p("b", 2)).unwrap();
        c.add_peer(p("a", 3)).unwrap(); // same name
        c.add_peer(p("c", 2)).unwrap(); // same id
        let names: Vec<_> = c.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
        assert!(c.add_peer(p("bad name", 9)).is_err());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp() {
        let d = tmp("atomic");
        let path = d.join("config.toml");
        let mut c = Config::default();
        c.save(&path).unwrap();
        // A stale temp from a crashed writer must not block or leak.
        fs::write(
            d.join(format!(".config.toml.tmp{}", std::process::id())),
            "junk",
        )
        .unwrap();
        c.add_folder(folder("a", "/data/a")).unwrap();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["config.toml"]);
        // Failure leaves the old file untouched.
        let ro = d.join("missing/deeper");
        fs::write(d.join("missing"), "a file, not a dir").unwrap();
        assert!(c.save(&ro.join("config.toml")).is_err());
        assert_eq!(Config::load(&path).unwrap(), c);
    }
}
