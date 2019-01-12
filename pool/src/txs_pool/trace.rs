use faketime::unix_time_as_millis;
use lru_cache::LruCache;
use numext_fixed_hash::H256;
use serde_derive::Serialize;
use std::fmt;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Hash)]
pub enum Action {
    AddPending,
    Proposed,
    AddCommit,
    Timeout,
    AddOrphan,
    Committed,
}

#[derive(Clone, Eq, PartialEq, Serialize, Hash)]
pub struct TxTrace {
    pub action: Action,
    pub info: String,
    pub time: u64,
}

impl TxTrace {
    pub fn new(action: Action, info: String, time: u64) -> TxTrace {
        TxTrace { action, info, time }
    }
}

impl fmt::Debug for TxTrace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "TxTrace {{ action: {:?}, info: {}, time: {} }}",
            self.action, self.info, self.time
        )
    }
}

impl fmt::Display for TxTrace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self, f)
    }
}

macro_rules! define_method {
    ($name:ident, $action:expr) => {
        pub fn $name<S: ToString>(&mut self, hash: &H256, info: S) {
            self.inner.get_mut(hash).map(|v| {
                v.push(TxTrace::new(
                    $action,
                    info.to_string(),
                    unix_time_as_millis(),
                ))
            });
        }
    };
}

#[derive(Clone, Debug)]
pub struct TxTraceMap {
    inner: LruCache<H256, Vec<TxTrace>>,
}

impl TxTraceMap {
    pub fn new(capacity: usize) -> Self {
        TxTraceMap {
            inner: LruCache::new(capacity),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn add_pending<S: ToString>(&mut self, hash: &H256, info: S) {
        self.inner
            .entry(hash.clone())
            .or_insert_with(Vec::new)
            .push(TxTrace::new(
                Action::AddPending,
                info.to_string(),
                unix_time_as_millis(),
            ));
    }

    pub fn get(&self, hash: &H256) -> Option<&Vec<TxTrace>> {
        self.inner.get(hash)
    }

    define_method!(proposed, Action::Proposed);
    define_method!(add_commit, Action::AddCommit);
    define_method!(add_orphan, Action::AddOrphan);
    define_method!(timeout, Action::Timeout);
    define_method!(committed, Action::Committed);
}
