// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(plugin)]
#![cfg_attr(feature = "dev", plugin(clippy))]
#![cfg_attr(not(feature = "dev"), allow(unknown_lints))]
#![allow(needless_pass_by_value)]

extern crate tikv;
extern crate clap;
extern crate protobuf;
extern crate kvproto;
extern crate rocksdb;
#[cfg(test)]
extern crate tempdir;
extern crate rustc_serialize;

use std::{str, u64};
use clap::{App, Arg, SubCommand};
use rustc_serialize::hex::{FromHex, ToHex};
use protobuf::Message;
use kvproto::raft_cmdpb::RaftCmdRequest;
use kvproto::raft_serverpb::{PeerState, RaftApplyState, RaftLocalState, RegionLocalState};
use kvproto::eraftpb::Entry;
use rocksdb::{ReadOptions, SeekKey, DB};
use tikv::util::{self, escape, unescape};
use tikv::util::codec::bytes::encode_bytes;
use tikv::raftstore::store::keys;
use tikv::raftstore::store::engine::{IterOption, Iterable, Peekable};
use tikv::storage::{CfName, ALL_CFS, CF_DEFAULT, CF_LOCK, CF_RAFT, CF_WRITE};
use tikv::storage::mvcc::{Lock, Write};
use tikv::storage::types::Key;

fn main() {
    let mut app = App::new("TiKV Ctl")
        .author("PingCAP")
        .about(
            "Distributed transactional key value database powered by Rust and Raft",
        )
        .arg(
            Arg::with_name("db")
                .short("d")
                .takes_value(true)
                .help("set rocksdb path"),
        )
        .arg(
            Arg::with_name("raftdb")
                .short("r")
                .takes_value(true)
                .help("set raft rocksdb path"),
        )
        .arg(
            Arg::with_name("hex-to-escaped")
                .short("h")
                .takes_value(true)
                .help("convert hex key to escaped key"),
        )
        .arg(
            Arg::with_name("escaped-to-hex")
                .short("e")
                .takes_value(true)
                .help("convert escaped key to hex key"),
        )
        .subcommand(
            SubCommand::with_name("raft")
                .about("print raft log entry")
                .subcommand(
                    SubCommand::with_name("log")
                        .about("print the raft log entry info")
                        .arg(
                            Arg::with_name("region")
                                .short("r")
                                .takes_value(true)
                                .help("set the region id"),
                        )
                        .arg(
                            Arg::with_name("index")
                                .short("i")
                                .takes_value(true)
                                .help("set the raft log index"),
                        )
                        .arg(
                            Arg::with_name("key")
                                .short("k")
                                .takes_value(true)
                                .help("set the raw key"),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("region")
                        .about("print region info")
                        .arg(
                            Arg::with_name("region")
                                .short("r")
                                .takes_value(true)
                                .help("set the region id, if not specified, print all regions."),
                        )
                        .arg(
                            Arg::with_name("skip-tombstone")
                                .short("s")
                                .takes_value(false)
                                .help("skip tombstone region."),
                        ),
                ),
        )
        .subcommand(
            SubCommand::with_name("size")
                .about("print region size")
                .arg(
                    Arg::with_name("region")
                        .short("r")
                        .takes_value(true)
                        .help("set the region id, if not specified, print all regions."),
                )
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .help("set the cf name, if not specified, print all cf."),
                ),
        )
        .subcommand(
            SubCommand::with_name("scan")
                .about("print the range db range")
                .arg(
                    Arg::with_name("from")
                        .short("f")
                        .takes_value(true)
                        .help("set the scan from raw key, in escaped format"),
                )
                .arg(
                    Arg::with_name("to")
                        .short("t")
                        .takes_value(true)
                        .help("set the scan end raw key, in escaped format"),
                )
                .arg(
                    Arg::with_name("limit")
                        .short("l")
                        .takes_value(true)
                        .help("set the scan limit"),
                )
                .arg(
                    Arg::with_name("start_ts")
                        .short("s")
                        .takes_value(true)
                        .help("set the scan start_ts as filter"),
                )
                .arg(
                    Arg::with_name("commit_ts")
                        .takes_value(true)
                        .help("set the scan commit_ts as filter"),
                )
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .help("column family name"),
                ),
        )
        .subcommand(
            SubCommand::with_name("print")
                .about("print the raw value")
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .help("column family name"),
                )
                .arg(
                    Arg::with_name("key")
                        .short("k")
                        .takes_value(true)
                        .help("set the query raw key, in escaped form"),
                ),
        )
        .subcommand(
            SubCommand::with_name("mvcc")
                .about("print the mvcc value")
                .arg(
                    Arg::with_name("cf")
                        .short("c")
                        .takes_value(true)
                        .help("column family name, only can be default/lock/write"),
                )
                .arg(
                    Arg::with_name("key")
                        .short("key")
                        .takes_value(true)
                        .help("the query key"),
                )
                .arg(
                    Arg::with_name("encoded")
                        .short("e")
                        .takes_value(false)
                        .help("set it when the key is already encoded."),
                )
                .arg(
                    Arg::with_name("start_ts")
                        .short("s")
                        .takes_value(true)
                        .help("set start_ts as filter"),
                )
                .arg(
                    Arg::with_name("commit_ts")
                        .takes_value(true)
                        .help("set commit_ts as filter"),
                ),
        )
        .subcommand(
            SubCommand::with_name("diff")
                .about("diff two region keys")
                .arg(
                    Arg::with_name("to")
                        .short("t")
                        .takes_value(true)
                        .help("to which db"),
                )
                .arg(
                    Arg::with_name("region")
                        .short("r")
                        .takes_value(true)
                        .help("specify region id"),
                ),
        );
    let matches = app.clone().get_matches();

    let hex_key = matches.value_of("hex-to-escaped");
    let escaped_key = matches.value_of("escaped-to-hex");
    match (hex_key, escaped_key) {
        (None, None) => {}
        (Some(_), Some(_)) => panic!("hex and escaped can not be passed together!"),
        (Some(hex), None) => {
            println!("{}", escape(&from_hex(hex)));
            return;
        }
        (None, Some(escaped)) => {
            println!("{}", &unescape(escaped).to_hex().to_uppercase());
            return;
        }
    };
    let db_path = matches.value_of("db").unwrap();
    let db = util::rocksdb::open(db_path, ALL_CFS).unwrap();
    let raft_db = if let Some(raftdb_path) = matches.value_of("raftdb") {
        util::rocksdb::open(raftdb_path, &[CF_DEFAULT]).unwrap()
    } else {
        let raftdb_path = db_path.to_owned() + "../raft";
        util::rocksdb::open(&raftdb_path, &[CF_DEFAULT]).unwrap()
    };

    if let Some(matches) = matches.subcommand_matches("print") {
        let cf_name = matches.value_of("cf").unwrap_or(CF_DEFAULT);
        let key = String::from(matches.value_of("key").unwrap());
        dump_raw_value(db, cf_name, key);
    } else if let Some(matches) = matches.subcommand_matches("raft") {
        if let Some(matches) = matches.subcommand_matches("log") {
            let key = match matches.value_of("key") {
                None => {
                    let region = String::from(matches.value_of("region").unwrap());
                    let index = String::from(matches.value_of("index").unwrap());
                    keys::raft_log_key(region.parse().unwrap(), index.parse().unwrap())
                }
                Some(k) => unescape(k),
            };
            dump_raft_log_entry(raft_db, &key);
        } else if let Some(matches) = matches.subcommand_matches("region") {
            let skip_tombstone = matches.is_present("skip-tombstone");
            match matches.value_of("region") {
                Some(id) => {
                    dump_region_info(&db, &raft_db, id.parse().unwrap(), skip_tombstone);
                }
                None => {
                    dump_all_region_info(&db, &raft_db, skip_tombstone);
                }
            }
        } else {
            panic!("Currently only support raft log entry and scan.")
        }
    } else if let Some(matches) = matches.subcommand_matches("size") {
        let cf_name = matches.value_of("cf");
        match matches.value_of("region") {
            Some(id) => {
                dump_region_size(&db, id.parse().unwrap(), cf_name);
            }
            None => dump_all_region_size(&db, cf_name),
        }
    } else if let Some(matches) = matches.subcommand_matches("scan") {
        let from = String::from(matches.value_of("from").unwrap());
        let to = matches.value_of("to").map(String::from);
        let limit = matches.value_of("limit").map(|s| s.parse().unwrap());
        let cf_name = matches.value_of("cf").unwrap_or(CF_DEFAULT);
        let start_ts = matches.value_of("start_ts").map(|s| s.parse().unwrap());
        let commit_ts = matches.value_of("commit_ts").map(|s| s.parse().unwrap());
        if let Some(ref to) = to {
            if to <= &from {
                panic!("The region's start pos must greater than the end pos.")
            }
        }
        dump_range(db, from, to, limit, cf_name, start_ts, commit_ts);
    } else if let Some(matches) = matches.subcommand_matches("mvcc") {
        let cf_name = matches.value_of("cf").unwrap_or(CF_DEFAULT);
        let key = matches.value_of("key").unwrap();
        let key_encoded = matches.is_present("encoded");
        let start_ts = matches.value_of("start_ts").map(|s| s.parse().unwrap());
        let commit_ts = matches.value_of("commit_ts").map(|s| s.parse().unwrap());
        println!("You are searching Key {}: ", key);
        match cf_name {
            CF_DEFAULT => {
                dump_mvcc_default(&db, key, key_encoded, start_ts);
            }
            CF_LOCK => {
                dump_mvcc_lock(&db, key, key_encoded, start_ts);
            }
            CF_WRITE => {
                dump_mvcc_write(&db, key, key_encoded, start_ts, commit_ts);
            }
            "all" => {
                dump_mvcc_default(&db, key, key_encoded, start_ts);
                dump_mvcc_lock(&db, key, key_encoded, start_ts);
                dump_mvcc_write(&db, key, key_encoded, start_ts, commit_ts);
            }
            _ => {
                println!("The cf: {} cannot be dumped", cf_name);
                let _ = app.print_help();
            }
        }
    } else if let Some(matches) = matches.subcommand_matches("diff") {
        let region_id: u64 = matches.value_of("region").unwrap().parse().unwrap();
        let db_path2 = matches.value_of("to").unwrap();
        let db2 = util::rocksdb::open(db_path2, ALL_CFS).unwrap();
        dump_diff(&db, &db2, region_id);
    } else {
        let _ = app.print_help();
    }

}

pub trait MvccDeserializable: PartialEq {
    fn deserialize(bytes: &[u8]) -> Self;
}

impl MvccDeserializable for Write {
    fn deserialize(bytes: &[u8]) -> Self {
        Write::parse(bytes).unwrap()
    }
}

impl MvccDeserializable for Lock {
    fn deserialize(bytes: &[u8]) -> Self {
        Lock::parse(bytes).unwrap()
    }
}

impl MvccDeserializable for Vec<u8> {
    fn deserialize(bytes: &[u8]) -> Self {
        bytes.to_vec()
    }
}

#[derive(PartialEq, Debug)]
pub struct MvccKv<T> {
    key: Key,
    value: T,
}

fn from_hex(key: &str) -> Vec<u8> {
    const HEX_PREFIX: &str = "0x";
    let mut s = String::from(key);
    if s.starts_with(HEX_PREFIX) {
        let len = s.len();
        let new_len = len.saturating_sub(HEX_PREFIX.len());
        s.truncate(new_len);
    }
    s.as_str().from_hex().unwrap()
}

pub fn gen_mvcc_iter<T: MvccDeserializable>(
    db: &DB,
    key_prefix: &str,
    prefix_is_encoded: bool,
    mvcc_type: CfName,
) -> Vec<MvccKv<T>> {
    let encoded_prefix = if prefix_is_encoded {
        unescape(key_prefix)
    } else {
        encode_bytes(unescape(key_prefix).as_slice())
    };
    let iter_opt = IterOption::new(None, false);
    let mut iter = db.new_iterator_cf(mvcc_type, iter_opt).unwrap();
    iter.seek(keys::data_key(&encoded_prefix).as_slice().into());
    if !iter.valid() {
        vec![]
    } else {
        let kvs = iter.map(|s| {
            let key = &keys::origin_key(&s.0);
            MvccKv {
                key: Key::from_encoded(key.to_vec()),
                value: T::deserialize(&s.1),
            }
        });
        kvs.take_while(|s| s.key.encoded().starts_with(&encoded_prefix))
            .collect()
    }
}


fn dump_mvcc_default(db: &DB, key: &str, encoded: bool, start_ts: Option<u64>) {
    let kvs: Vec<MvccKv<Vec<u8>>> = gen_mvcc_iter(db, key, encoded, CF_DEFAULT);
    for kv in kvs {
        let ts = kv.key.decode_ts().unwrap();
        let key = kv.key.truncate_ts().unwrap();
        if start_ts.is_none() || start_ts.unwrap() == ts {
            let v = key.encoded();
            println!("Key: {:?}", escape(v));
            println!("Value: {:?}", escape(kv.value.as_slice()));
            println!("Start_ts: {:?}", ts);
            println!("");
        }
    }
}

fn dump_mvcc_lock(db: &DB, key: &str, encoded: bool, start_ts: Option<u64>) {
    let kvs: Vec<MvccKv<Lock>> = gen_mvcc_iter(db, key, encoded, CF_LOCK);
    for kv in kvs {
        let lock = &kv.value;
        if start_ts.is_none() || start_ts.unwrap() == lock.ts {
            println!("Key: {:?}", escape(kv.key.encoded()));
            println!("Primary: {:?}", escape(lock.primary.as_slice()));
            println!("Type: {:?}", lock.lock_type);
            println!("Start_ts: {:?}", lock.ts);
            println!("");
        }
    }
}

fn dump_mvcc_write(
    db: &DB,
    key: &str,
    encoded: bool,
    start_ts: Option<u64>,
    commit_ts: Option<u64>,
) {
    let kvs: Vec<MvccKv<Write>> = gen_mvcc_iter(db, key, encoded, CF_WRITE);
    for kv in kvs {
        let write = &kv.value;
        let cmt_ts = kv.key.decode_ts().unwrap();
        let key = kv.key.truncate_ts().unwrap();
        if (start_ts.is_none() || start_ts.unwrap() == write.start_ts) &&
            (commit_ts.is_none() || commit_ts.unwrap() == cmt_ts)
        {
            println!("Key: {:?}", escape(key.encoded()));
            println!("Type: {:?}", write.write_type);
            println!("Start_ts: {:?}", write.start_ts);
            println!("Commit_ts: {:?}", cmt_ts);
            println!("Short value: {:?}", write.short_value);
            println!("");
        }
    }
}

fn dump_raw_value(db: DB, cf: &str, key: String) {
    let key = unescape(&key);
    let value = db.get_value_cf(cf, &key).unwrap();
    println!("value: {}", value.map_or("None".to_owned(), |v| escape(&v)));
}

fn dump_raft_log_entry(raft_db: DB, idx_key: &[u8]) {
    let (region_id, idx) = keys::decode_raft_log_key(idx_key).unwrap();
    println!("idx_key: {}", escape(idx_key));
    println!("region: {}", region_id);
    println!("log index: {}", idx);
    let mut ent: Entry = raft_db.get_msg(idx_key).unwrap().unwrap();
    let data = ent.take_data();
    println!("entry {:?}", ent);
    let mut msg = RaftCmdRequest::new();
    msg.merge_from_bytes(&data).unwrap();
    println!("msg len: {}", data.len());
    println!("{:?}", msg);
}

fn dump_diff(db: &DB, db2: &DB, region_id: u64) {
    println!("region id: {}", region_id);
    let region_state_key = keys::region_state_key(region_id);
    let region_state: RegionLocalState =
        db.get_msg_cf(CF_RAFT, &region_state_key).unwrap().unwrap();
    println!("db1 region state: {:?}", region_state);
    let region_state2: RegionLocalState =
        db2.get_msg_cf(CF_RAFT, &region_state_key).unwrap().unwrap();
    println!("db2 region state: {:?}", region_state2);

    let raft_state_key = keys::apply_state_key(region_id);

    let apply_state: RaftApplyState = db.get_msg_cf(CF_RAFT, &raft_state_key).unwrap().unwrap();
    println!("db1 apply state: {:?}", apply_state);

    let apply_state: RaftApplyState = db2.get_msg_cf(CF_RAFT, &raft_state_key).unwrap().unwrap();
    println!("db2 apply state: {:?}", apply_state);

    let region = region_state.get_region();
    let start_key = &keys::data_key(region.get_start_key());
    let end_key = &keys::data_end_key(region.get_end_key());
    for cf in ALL_CFS {
        let handle = db.cf_handle(cf).unwrap();
        let handle2 = db2.cf_handle(cf).unwrap();
        println!("cf: {}", cf);
        let mut ropt = ReadOptions::new();
        ropt.set_iterate_upper_bound(end_key);
        let mut iter = db.iter_cf_opt(handle, ropt);
        let mut ropt = ReadOptions::new();
        ropt.set_iterate_upper_bound(end_key);
        let mut iter2 = db2.iter_cf_opt(handle2, ropt);
        iter.seek(SeekKey::Key(start_key));
        iter2.seek(SeekKey::Key(start_key));
        let mut has_diff = false;
        let mut common_head_len = 0;
        while iter.valid() && iter2.valid() {
            if iter.key() != iter2.key() {
                if iter.key() > iter2.key() {
                    has_diff = true;
                    println!("only db2 has : {}", escape(iter2.key()));
                    if cf == &CF_DEFAULT || cf == &CF_WRITE {
                        println!(
                            "timestamp: {}",
                            Key::from_encoded(iter2.key().to_vec()).decode_ts().unwrap()
                        );
                    }
                    iter2.next();
                    continue;
                }
                if iter.key() < iter2.key() {
                    has_diff = true;
                    println!("only db1 has : {}", escape(iter.key()));
                    if cf == &CF_DEFAULT || cf == &CF_WRITE {
                        println!(
                            "timestamp: {}",
                            Key::from_encoded(iter.key().to_vec()).decode_ts().unwrap()
                        );
                    }
                    iter.next();
                    continue;
                }
            }
            if !has_diff {
                common_head_len += 1;
            }
            iter.next();
            iter2.next();
        }
        println!("head have {} same keys", common_head_len);

        if !iter.valid() && iter2.valid() {
            println!("iter1 invalid but iter2 valid!");
            while iter2.valid() {
                println!("only db2 has : {:?}", escape(iter2.key()));
                iter2.next();
            }
        }
        if iter.valid() && !iter2.valid() {
            println!("iter2 invalid but iter1 valid!");
            while iter.valid() {
                println!("only db1 has : {:?}", escape(iter.key()));
                iter.next();
            }
        }
    }
}

fn dump_region_info(db: &DB, raft_db: &DB, region_id: u64, skip_tombstone: bool) {
    let region_state_key = keys::region_state_key(region_id);
    let region_state: Option<RegionLocalState> = db.get_msg_cf(CF_RAFT, &region_state_key).unwrap();
    if skip_tombstone &&
        region_state
            .as_ref()
            .map_or(false, |s| s.get_state() == PeerState::Tombstone)
    {
        return;
    }
    println!("region state key: {}", escape(&region_state_key));
    println!("region state: {:?}", region_state);

    let raft_state_key = keys::raft_state_key(region_id);
    println!("raft state key: {}", escape(&raft_state_key));
    let raft_state: Option<RaftLocalState> = raft_db.get_msg(&raft_state_key).unwrap();
    println!("raft state: {:?}", raft_state);

    let apply_state_key = keys::apply_state_key(region_id);
    println!("apply state key: {}", escape(&apply_state_key));
    let apply_state: Option<RaftApplyState> = db.get_msg_cf(CF_RAFT, &apply_state_key).unwrap();
    println!("apply state: {:?}", apply_state);
}

fn convert_gbmb(mut bytes: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < MB {
        return bytes.to_string();
    }
    let mb = if bytes % GB == 0 {
        String::from("")
    } else {
        format!("{:.3} MB ", (bytes % GB) as f64 / MB as f64)
    };
    bytes /= GB;
    let gb = if bytes == 0 {
        String::from("")
    } else {
        format!("{} GB ", bytes)
    };
    format!("{}{}", gb, mb)
}

fn dump_region_size(db: &DB, region_id: u64, cf: Option<&str>) {
    println!("region id: {}", region_id);
    if let Some(cf_name) = cf {
        println!("cf_name: {}", cf_name);
    }
    let size = get_region_size(db, region_id, cf);
    println!("region size: {}", convert_gbmb(size));
}

fn dump_all_region_info(db: &DB, raft_db: &DB, skip_tombstone: bool) {
    let region_ids = get_all_region_ids(db);
    for region_id in region_ids {
        dump_region_info(db, raft_db, region_id, skip_tombstone);
    }
}

fn dump_all_region_size(db: &DB, cf: Option<&str>) {
    let mut region_ids = get_all_region_ids(db);
    let mut region_sizes: Vec<u64> = region_ids
        .iter()
        .map(|&region_id| get_region_size(db, region_id, cf))
        .collect();
    let region_number = region_ids.len();
    let total_size = region_sizes.iter().sum();
    let mut v: Vec<(u64, u64)> = region_sizes.drain(..).zip(region_ids.drain(..)).collect();
    v.sort();
    v.reverse();
    println!("total region number: {}", region_number);
    println!("total region size: {}", convert_gbmb(total_size));
    for (size, id) in v {
        println!("region_id: {}", id);
        println!("region size: {}", convert_gbmb(size));
    }
}

fn get_all_region_ids(db: &DB) -> Vec<u64> {
    let start_key = keys::REGION_META_MIN_KEY;
    let end_key = keys::REGION_META_MAX_KEY;
    let mut region_ids = vec![];
    db.scan_cf(CF_RAFT, start_key, end_key, false, &mut |key, _| {
        let (region_id, suffix) = keys::decode_region_meta_key(key)?;
        if suffix != keys::REGION_STATE_SUFFIX {
            return Ok(true);
        }
        region_ids.push(region_id);
        Ok(true)
    }).unwrap();
    region_ids
}

fn get_region_size(db: &DB, region_id: u64, cf: Option<&str>) -> u64 {
    let region_state_key = keys::region_state_key(region_id);
    let region_state: RegionLocalState =
        db.get_msg_cf(CF_RAFT, &region_state_key).unwrap().unwrap();
    let region = region_state.get_region();
    let start_key = &keys::data_key(region.get_start_key());
    let end_key = &keys::data_end_key(region.get_end_key());
    let mut size: u64 = 0;
    let cf_arr = match cf {
        Some(s) => vec![s],
        None => vec![CF_DEFAULT, CF_WRITE, CF_LOCK],
    };
    for cf in &cf_arr {
        db.scan_cf(cf, start_key, end_key, true, &mut |_, v| {
            size += v.len() as u64;
            Ok(true)
        }).unwrap();
    }
    size
}

fn parse_ts_key_from_key(encode_key: Vec<u8>) -> (u64, Vec<u8>) {
    let item_key = Key::from_encoded(encode_key);
    let ts = item_key.decode_ts().unwrap();
    let item_key = item_key.truncate_ts().unwrap();
    let key = item_key.encoded();
    (ts, key.clone())
}

fn dump_range(
    db: DB,
    from: String,
    to: Option<String>,
    limit: Option<u64>,
    cf: &str,
    start_ts: Option<u64>,
    commit_ts: Option<u64>,
) {
    let from = unescape(&from);
    let to = to.map_or_else(|| vec![0xff], |s| unescape(&s));
    let limit = limit.unwrap_or(u64::MAX);

    if limit == 0 {
        return;
    }
    let mut cnt = 0;
    db.scan_cf(cf, &from, &to, true, &mut |k, v| {
        let mut right_key = true;
        match cf {
            CF_DEFAULT => {
                let (ts, _) = parse_ts_key_from_key(escape(k).into_bytes());
                right_key = start_ts.is_none() || ts == start_ts.unwrap();
            }
            CF_WRITE => {
                let value = Write::deserialize(v.as_ref());
                let (cmt_ts, _) = parse_ts_key_from_key(escape(k).into_bytes());
                right_key = (start_ts.is_none() || value.start_ts == start_ts.unwrap()) &&
                    (commit_ts.is_none() || cmt_ts == commit_ts.unwrap());
            }
            CF_LOCK => {
                let value = Lock::deserialize(v.as_ref());
                right_key = start_ts.is_none() || value.ts == start_ts.unwrap();
            }
            _ => {}
        }

        if right_key {
            println!("key: {}, value len: {}", escape(k), v.len());
            println!("{}", escape(v));
            cnt += 1;
        }
        Ok(cnt < limit)
    }).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::Writable;
    use tikv::util::codec::bytes::encode_bytes;
    use tikv::util::codec::number::NumberEncoder;
    use tikv::raftstore::store::keys;
    use tikv::storage::{ALL_CFS, CF_DEFAULT, CF_LOCK, CF_WRITE};
    use tikv::storage::mvcc::{Lock, LockType, Write, WriteType};
    use tempdir::TempDir;
    use tikv::util::rocksdb::new_engine;
    use tikv::storage::types::Key;
    use tikv::util::escape;

    const PREFIX: &'static [u8] = b"k";

    #[test]
    fn test_ctl_mvcc() {
        let tmp_dir = TempDir::new("mvcc_tmp").expect("create mvcc_tmp dir");
        //        let file_path = tmp_dir.path().join("tmp_db");
        let db = new_engine(tmp_dir.path().to_str().unwrap(), ALL_CFS).unwrap();
        let test_data = vec![(PREFIX, b"v", 5), (PREFIX, b"x", 10), (PREFIX, b"y", 15)];
        for &(k, v, ts) in &test_data {
            let key = keys::data_key(Key::from_raw(k).append_ts(ts).encoded().as_slice());
            db.put(key.as_slice(), v).unwrap();
        }
        let kvs_gen: Vec<MvccKv<Vec<u8>>> = gen_mvcc_iter(&db, "k", false, CF_DEFAULT);
        assert_eq!(
            kvs_gen,
            gen_mvcc_iter(&db, &escape(&encode_bytes(b"k")), true, CF_DEFAULT)
        );
        let mut test_iter = test_data.clone();
        assert_eq!(test_iter.len(), kvs_gen.len());
        for kv in kvs_gen {
            let ts = kv.key.decode_ts().unwrap();
            let key = kv.key.truncate_ts().unwrap().raw().unwrap();
            let value = kv.value.as_slice();
            test_iter.retain(|&s| {
                !(&s.0[..] == key.as_slice() && &s.1[..] == value && s.2 == ts)
            });
        }
        assert_eq!(test_iter.len(), 0);

        // Test MVCC Lock
        let test_data_lock = vec![
            (b"kv", LockType::Put, b"v", 5),
            (b"kx", LockType::Lock, b"x", 10),
            (b"kz", LockType::Delete, b"y", 15),
        ];
        let keys: Vec<_> = test_data_lock
            .iter()
            .map(|data| {
                let encoded = encode_bytes(&data.0[..]);
                keys::data_key(encoded.as_slice())
            })
            .collect();
        let lock_value: Vec<_> = test_data_lock
            .iter()
            .map(|data| {
                Lock::new(data.1, data.2.to_vec(), data.3, 0, None).to_bytes()
            })
            .collect();
        let kvs = keys.iter().zip(lock_value.iter());
        let lock_cf = db.cf_handle(CF_LOCK).unwrap();
        for (k, v) in kvs {
            db.put_cf(lock_cf, k.as_slice(), v.as_slice()).unwrap();
        }
        fn assert_iter(kvs_gen: &[MvccKv<Lock>], test_data: (&[u8; 2], LockType, &[u8; 1], u64)) {
            assert_eq!(kvs_gen.len(), 1);
            let kv = &kvs_gen[0];
            let lock = &kv.value;
            let key = kv.key.raw().unwrap();
            assert!(
                &key[..] == test_data.0 && test_data.1 == lock.lock_type &&
                    test_data.2 == lock.primary.as_slice() &&
                    test_data.3 == lock.ts
            );
        }
        assert_iter(&gen_mvcc_iter(&db, "kv", false, CF_LOCK), test_data_lock[0]);
        assert_iter(&gen_mvcc_iter(&db, "kx", false, CF_LOCK), test_data_lock[1]);
        assert_iter(&gen_mvcc_iter(&db, "kz", false, CF_LOCK), test_data_lock[2]);

        // Test MVCC Write
        let test_data_write = vec![
            (PREFIX, WriteType::Delete, 5, 10),
            (PREFIX, WriteType::Lock, 15, 20),
            (PREFIX, WriteType::Put, 25, 30),
            (PREFIX, WriteType::Rollback, 35, 40),
        ];
        let keys: Vec<_> = test_data_write
            .iter()
            .map(|data| {
                let encoded = encode_bytes(&data.0[..]);
                let mut d = keys::data_key(encoded.as_slice());
                let _ = d.encode_u64_desc(data.3);
                d
            })
            .collect();
        let write_value: Vec<_> = test_data_write
            .iter()
            .map(|data| Write::new(data.1, data.2, None).to_bytes())
            .collect();
        let kvs = keys.iter().zip(write_value.iter());
        let write_cf = db.cf_handle(CF_WRITE).unwrap();
        for (k, v) in kvs {
            db.put_cf(write_cf, k.as_slice(), v.as_slice()).unwrap();
        }
        let kvs_gen: Vec<MvccKv<Write>> = gen_mvcc_iter(&db, "k", false, CF_WRITE);
        assert_eq!(
            kvs_gen,
            gen_mvcc_iter(&db, &escape(&encode_bytes(b"k")), true, CF_WRITE)
        );
        let mut test_iter = test_data_write.clone();
        assert_eq!(test_iter.len(), kvs_gen.len());
        for kv in kvs_gen {
            let write = &kv.value;
            let ts = kv.key.decode_ts().unwrap();
            let key = kv.key.truncate_ts().unwrap().raw().unwrap();
            test_iter.retain(|&s| {
                !(&s.0[..] == key.as_slice() && s.1 == write.write_type &&
                    s.2 == write.start_ts && s.3 == ts)
            });
        }
        assert_eq!(test_iter.len(), 0);
    }
}
