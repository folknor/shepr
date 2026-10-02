//! Validated Git names. `pub` because `GitHeadIdentity` and
//! `GitUpstreamIdentity`, reachable through `GitStatusCacheEntry`, carry them;
//! only this crate constructs or reads them.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Oid(String);

impl Oid {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        (matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FullRefName(String);

impl FullRefName {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        (value.starts_with("refs/")
            && !value.ends_with('.')
            && !value.contains("..")
            && !value.contains("@{")
            && value
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
            && !value
                .bytes()
                .any(|byte| byte <= b' ' || byte == 0x7f || b"~^:?*[\\".contains(&byte)))
        .then(|| Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn branch_name(&self) -> Option<BranchName> {
        self.as_str()
            .strip_prefix("refs/heads/")
            .filter(|name| !name.is_empty())
            .map(|name| BranchName(name.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchName(String);

impl BranchName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn full_ref(&self) -> FullRefName {
        FullRefName(format!("refs/heads/{}", self.as_str()))
    }
}
