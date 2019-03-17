use std::str;
use caseless;
use ::protocol::{Resp, Array, BulkStr};

pub trait ThreadSafe: Send + Sync + 'static {}


#[derive(Debug)]
pub struct CmdParseError {}

pub fn has_flags(s: &str, delimiter: char, flag: &'static str) -> bool {
    s.split(delimiter)
        .find(|s| caseless::canonical_caseless_match_str(s, flag))
        .is_some()
}
