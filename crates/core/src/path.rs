use crate::PathId;

/// A path relative to a folder root, as peers name files on the wire.
///
/// Validated on construction, which every decoder goes through: no absolute
/// paths, no `.` or `..`, no empty components, no NUL. This is the only thing
/// standing between a peer and `../../etc/passwd`, so there is no unchecked
/// constructor.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

pub const MAX_REL_PATH: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    Empty,
    TooLong,
    Absolute,
    BadComponent,
    Nul,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PathError::Empty => "empty path",
            PathError::TooLong => "path too long",
            PathError::Absolute => "absolute path",
            PathError::BadComponent => "empty, `.` or `..` component",
            PathError::Nul => "NUL in path",
        })
    }
}

impl std::error::Error for PathError {}

impl RelPath {
    pub fn new(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.len() > MAX_REL_PATH {
            return Err(PathError::TooLong);
        }
        if s.starts_with('/') {
            return Err(PathError::Absolute);
        }
        if s.contains('\0') {
            return Err(PathError::Nul);
        }
        if s.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
            return Err(PathError::BadComponent);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn parent(&self) -> Option<RelPath> {
        self.0.rsplit_once('/').map(|(p, _)| RelPath(p.to_owned()))
    }

    pub fn join(&self, name: &str) -> Result<RelPath, PathError> {
        RelPath::new(&format!("{}/{name}", self.0))
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PathId {
    /// The id every device derives for `rel` in `folder`, so peers agree on it
    /// without coordination. A rename is a new id: a tombstone plus a new file,
    /// whose chunks are already present and never re-sent.
    pub fn of(folder: &str, rel: &RelPath) -> PathId {
        let mut h = blake3::Hasher::new();
        h.update(&(folder.len() as u64).to_le_bytes());
        h.update(folder.as_bytes());
        h.update(rel.as_str().as_bytes());
        let b = h.finalize();
        PathId(u64::from_le_bytes(
            b.as_bytes()[..8].try_into().expect("8 bytes"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escapes() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a/",
            "./a",
            "a/.",
            "a\0b",
        ] {
            assert!(RelPath::new(bad).is_err(), "{bad:?} accepted");
        }
        let long = "a".repeat(MAX_REL_PATH + 1);
        assert_eq!(RelPath::new(&long), Err(PathError::TooLong));
    }

    #[test]
    fn accepts_ordinary_paths() {
        let p = RelPath::new("notes/2026/..hidden.md").unwrap();
        assert_eq!(p.file_name(), "..hidden.md");
        assert_eq!(p.parent().unwrap().as_str(), "notes/2026");
        assert_eq!(p.components().count(), 3);
        assert!(RelPath::new("a").unwrap().parent().is_none());
    }

    #[test]
    fn path_ids_agree_and_separate_folders() {
        let p = RelPath::new("a/b.txt").unwrap();
        assert_eq!(PathId::of("f", &p), PathId::of("f", &p));
        assert_ne!(PathId::of("f", &p), PathId::of("g", &p));
        assert_ne!(
            PathId::of("f", &p),
            PathId::of("f", &RelPath::new("a/c.txt").unwrap())
        );
    }
}
