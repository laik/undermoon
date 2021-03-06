use crate::common::cluster::MigrationTaskMeta;
use crate::common::utils::{get_resp_bytes, get_resp_strings, get_slot, ThreadSafe};
use crate::protocol::{Array, BinSafeStr, BulkStr, RedisClientError, Resp, RespSlice, RespVec};
use crate::proxy::backend::CmdTask;
use crate::proxy::blocking::BlockingHintTask;
use crate::proxy::database::DBSendError;
use crate::replication::replicator::ReplicatorError;
use futures::Future;
use itertools::Itertools;
use std::error::Error;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::str;
use std::sync::atomic::{AtomicU16, Ordering};

#[derive(Debug)]
pub enum MgrSubCmd {
    PreCheck,
    PreSwitch,
    FinalSwitch,
}

impl MgrSubCmd {
    pub fn as_str(&self) -> &str {
        match self {
            Self::PreCheck => "PRECHECK",
            Self::PreSwitch => "PRESWITCH",
            Self::FinalSwitch => "FINALSWITCH",
        }
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum MigrationState {
    PreCheck = 0,
    PreBlocking = 1,
    PreSwitch = 2,
    Scanning = 3,
    FinalSwitch = 4,
    SwitchCommitted = 5,
}

#[derive(Debug)]
pub struct AtomicMigrationState {
    inner: AtomicU16,
}

impl AtomicMigrationState {
    pub fn new() -> Self {
        Self {
            inner: AtomicU16::new(MigrationState::PreCheck as u16),
        }
    }

    pub fn set_state(&self, state: MigrationState) {
        self.inner.store(state as u16, Ordering::SeqCst);
    }

    pub fn get_state(&self) -> MigrationState {
        match self.inner.load(Ordering::SeqCst) {
            0 => MigrationState::PreCheck,
            1 => MigrationState::PreBlocking,
            2 => MigrationState::PreSwitch,
            3 => MigrationState::Scanning,
            4 => MigrationState::FinalSwitch,
            _ => MigrationState::SwitchCommitted,
        }
    }
}

pub trait MigratingTask: ThreadSafe {
    type Task: CmdTask;

    fn start<'s>(&'s self)
        -> Pin<Box<dyn Future<Output = Result<(), MigrationError>> + Send + 's>>;
    fn stop<'s>(&'s self) -> Pin<Box<dyn Future<Output = Result<(), MigrationError>> + Send + 's>>;
    fn send(&self, cmd_task: Self::Task) -> Result<(), DBSendError<BlockingHintTask<Self::Task>>>;
    fn get_state(&self) -> MigrationState;
}

pub trait ImportingTask: ThreadSafe {
    type Task: CmdTask;

    fn start<'s>(&'s self)
        -> Pin<Box<dyn Future<Output = Result<(), MigrationError>> + Send + 's>>;
    fn stop<'s>(&'s self) -> Pin<Box<dyn Future<Output = Result<(), MigrationError>> + Send + 's>>;
    fn send(&self, cmd_task: Self::Task) -> Result<(), DBSendError<BlockingHintTask<Self::Task>>>;
    fn get_state(&self) -> MigrationState;
    fn handle_switch(
        &self,
        switch_arg: SwitchArg,
        sub_cmd: MgrSubCmd,
    ) -> Result<(), MigrationError>;
}

pub struct SwitchArg {
    pub version: String,
    pub meta: MigrationTaskMeta,
}

impl SwitchArg {
    pub fn into_strings(self) -> Vec<String> {
        let SwitchArg { version, meta } = self;
        let mut strs = vec![version];
        strs.extend(meta.into_strings().into_iter());
        strs
    }

    pub fn from_strings<It>(it: &mut It) -> Option<Self>
    where
        It: Iterator<Item = String>,
    {
        let version = it.next()?;
        let meta = MigrationTaskMeta::from_strings(it)?;
        Some(Self { version, meta })
    }
}

pub fn parse_switch_command(resp: &RespSlice) -> Option<SwitchArg> {
    let command = get_resp_strings(resp)?;
    let mut it = command.into_iter();
    // Skip UMCTL TMPSWITCH
    it.next()?;
    it.next()?;
    SwitchArg::from_strings(&mut it)
}

#[derive(Debug)]
pub enum MigrationError {
    IncompatibleVersion,
    AlreadyStarted,
    AlreadyEnded,
    Canceled,
    NotReady,
    ReplError(ReplicatorError),
    RedisClient(RedisClientError),
    Io(io::Error),
    Timeout,
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for MigrationError {
    fn description(&self) -> &str {
        "migration error"
    }

    fn cause(&self) -> Option<&dyn Error> {
        match self {
            MigrationError::Io(err) => Some(err),
            MigrationError::ReplError(err) => Some(err),
            MigrationError::RedisClient(err) => Some(err),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct SlotRangeArray {
    pub ranges: Vec<(usize, usize)>,
}

impl fmt::Display for SlotRangeArray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (i, range) in self.ranges.iter().enumerate() {
            write!(f, "{}-{}", range.0, range.1)?;
            if i + 1 != self.ranges.len() {
                write!(f, ",")?;
            }
        }
        Ok(())
    }
}

impl SlotRangeArray {
    pub fn is_key_inside(&self, key: &[u8]) -> bool {
        let slot = get_slot(key);
        for (start, end) in self.ranges.iter() {
            if slot >= *start && slot <= *end {
                return true;
            }
        }
        false
    }

    pub fn info(&self) -> String {
        self.ranges
            .iter()
            .map(|(start, end)| format!("{}-{}", start, end))
            .join(",")
    }
}

pub struct ScanResponse {
    pub next_index: u64,
    pub keys: Vec<BinSafeStr>,
}

impl ScanResponse {
    pub fn parse_scan(resp: RespVec) -> Option<ScanResponse> {
        match resp {
            Resp::Arr(Array::Arr(ref resps)) => {
                let index_data = resps.get(0).and_then(|resp| match resp {
                    Resp::Bulk(BulkStr::Str(ref s)) => Some(s.clone()),
                    Resp::Simple(ref s) => Some(s.clone()),
                    _ => None,
                })?;
                let next_index = str::from_utf8(index_data.as_slice()).ok()?.parse().ok()?;
                let keys = get_resp_bytes(resps.get(1)?)?;
                Some(ScanResponse { next_index, keys })
            }
            _ => None,
        }
    }
}
