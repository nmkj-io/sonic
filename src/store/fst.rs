// Sonic
//
// Fast, lightweight and schema-less search backend
// Copyright: 2019, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use fst::automaton::AlwaysMatch;
use fst::set::Stream as FSTStream;
use fst::{
    Automaton, Error as FSTError, IntoStreamer, Set as FSTSet, SetBuilder as FSTSetBuilder,
    Streamer,
};
use fst_levenshtein::Levenshtein;
use fst_regex::Regex;
use hashbrown::{HashMap, HashSet};
use radix::RadixNum;
use regex_syntax::escape as regex_escape;
use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::SystemTime;

use super::generic::{
    StoreGeneric, StoreGenericActionBuilder, StoreGenericBuilder, StoreGenericPool,
};
use super::keyer::StoreKeyerHasher;
use crate::lexer::ranges::LexerRegexRange;
use crate::APP_CONF;

pub struct StoreFSTPool;
pub struct StoreFSTBuilder;

pub struct StoreFST {
    graph: FSTSet,
    target: StoreFSTKey,
    pending: StoreFSTPending,
    last_used: Arc<RwLock<SystemTime>>,
    last_consolidated: Arc<RwLock<SystemTime>>,
}

#[derive(Default)]
pub struct StoreFSTPending {
    pop: Arc<RwLock<HashSet<Vec<u8>>>>,
    push: Arc<RwLock<HashSet<Vec<u8>>>>,
}

pub struct StoreFSTActionBuilder;

pub struct StoreFSTAction {
    store: StoreFSTBox,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct StoreFSTKey {
    collection_hash: StoreFSTAtom,
    bucket_hash: StoreFSTAtom,
}

pub struct StoreFSTMisc;

#[derive(Copy, Clone)]
enum StoreFSTPathMode {
    Permanent,
    Temporary,
    Backup,
}

type StoreFSTAtom = u32;
type StoreFSTBox = Arc<StoreFST>;

const WORD_LIMIT_LENGTH: usize = 40;
const ATOM_HASH_RADIX: usize = 16;

lazy_static! {
    pub static ref GRAPH_ACCESS_LOCK: Arc<RwLock<bool>> = Arc::new(RwLock::new(false));
    static ref GRAPH_ACQUIRE_LOCK: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    static ref GRAPH_REBUILD_LOCK: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    static ref GRAPH_POOL: Arc<RwLock<HashMap<StoreFSTKey, StoreFSTBox>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref GRAPH_CONSOLIDATE: Arc<RwLock<HashSet<StoreFSTKey>>> =
        Arc::new(RwLock::new(HashSet::new()));
}

impl StoreFSTPathMode {
    fn extension(&self) -> &'static str {
        match self {
            StoreFSTPathMode::Permanent => ".fst",
            StoreFSTPathMode::Temporary => ".fst.tmp",
            StoreFSTPathMode::Backup => ".fst.bck",
        }
    }
}

impl StoreFSTPool {
    pub fn count() -> (usize, usize) {
        (
            GRAPH_POOL.read().unwrap().len(),
            GRAPH_CONSOLIDATE.read().unwrap().len(),
        )
    }

    pub fn acquire<'a, T: Into<&'a str>>(collection: T, bucket: T) -> Result<StoreFSTBox, ()> {
        let (collection_str, bucket_str) = (collection.into(), bucket.into());

        let pool_key = StoreFSTKey::from_str(collection_str, bucket_str);

        // Freeze acquire lock, and reference it in context
        // Notice: this prevents two graphs on the same collection to be opened at the same time.
        let _acquire = GRAPH_ACQUIRE_LOCK.lock().unwrap();

        // Acquire a thread-safe store pool reference in read mode
        let graph_pool_read = GRAPH_POOL.read().unwrap();

        if let Some(store_fst) = graph_pool_read.get(&pool_key) {
            Self::proceed_acquire_cache("fst", collection_str, pool_key, store_fst)
        } else {
            info!(
                "fst store not in pool for collection: {} <{:x?}> / bucket: {} <{:x?}>, opening it",
                collection_str, pool_key.collection_hash, bucket_str, pool_key.bucket_hash
            );

            // Important: we need to drop the read reference first, to avoid dead-locking \
            //   when acquiring the RWLock in write mode in this block.
            drop(graph_pool_read);

            Self::proceed_acquire_open("fst", collection_str, pool_key, &*GRAPH_POOL)
        }
    }

    pub fn janitor() {
        Self::proceed_janitor(
            "fst",
            &*GRAPH_POOL,
            APP_CONF.store.fst.pool.inactive_after,
            &*GRAPH_ACCESS_LOCK,
        )
    }

    pub fn backup(path: &Path) -> Result<(), io::Error> {
        debug!("backing up all fst stores to path: {:?}", path);

        // Create backup directory (full path)
        fs::create_dir_all(path)?;

        // Proceed dump action (backup)
        Self::dump_action(
            "backup",
            StoreFSTPathMode::Permanent,
            &*APP_CONF.store.fst.path,
            path,
            &Self::backup_item,
        )
    }

    pub fn restore(path: &Path) -> Result<(), io::Error> {
        debug!("restoring all fst stores from path: {:?}", path);

        // Proceed dump action (restore)
        Self::dump_action(
            "restore",
            StoreFSTPathMode::Backup,
            path,
            &*APP_CONF.store.fst.path,
            &Self::restore_item,
        )
    }

    pub fn consolidate(force: bool) {
        debug!("scanning for fst store pool items to consolidate");

        // Notice: we do not consolidate all items at each tick, we try to even out multiple \
        //   consolidation tasks over time. This lowers the overall HZ of the tasker system for \
        //   certain heavy tasks, which is better to spread out consolidation steps over time over \
        //   a large number of very active buckets.

        // Acquire rebuild lock, and reference it in context
        // Notice: this prevents two consolidate operations to be executed at the same time.
        let _rebuild = GRAPH_REBUILD_LOCK.lock().unwrap();

        // Exit trap: Register is empty? Abort there.
        if GRAPH_CONSOLIDATE.read().unwrap().is_empty() {
            info!("no fst store pool items to consolidate in register");

            return;
        }

        // Step 1: List keys to be consolidated
        let mut keys_consolidate: Vec<StoreFSTKey> = Vec::new();

        {
            // Acquire access lock (in blocking write mode), and reference it in context
            // Notice: this prevents store to be acquired from any context
            let _access = GRAPH_ACCESS_LOCK.write().unwrap();

            let (graph_pool_read, graph_consolidate_read) = (
                GRAPH_POOL.read().unwrap(),
                GRAPH_CONSOLIDATE.read().unwrap(),
            );

            for key in &*graph_consolidate_read {
                if let Some(store) = graph_pool_read.get(key) {
                    let not_consolidated_for = store
                        .last_consolidated
                        .read()
                        .unwrap()
                        .elapsed()
                        .unwrap()
                        .as_secs();

                    if force || not_consolidated_for >= APP_CONF.store.fst.graph.consolidate_after {
                        info!(
                            "fst key: {} not consolidated for: {} seconds, may consolidate",
                            key, not_consolidated_for
                        );

                        keys_consolidate.push(*key);
                    } else {
                        debug!(
                            "fst key: {} not consolidated for: {} seconds, no consolidate",
                            key, not_consolidated_for
                        );
                    }
                }
            }
        }

        // Exit trap: Nothing to consolidate yet? Abort there.
        if keys_consolidate.is_empty() {
            info!("no fst store pool items need to consolidate at the moment");

            return;
        }

        // Step 2: Clear keys to be consolidated from register
        {
            // Acquire access lock (in blocking write mode), and reference it in context
            // Notice: this prevents store to be acquired from any context
            let _access = GRAPH_ACCESS_LOCK.write().unwrap();

            let mut graph_consolidate_write = GRAPH_CONSOLIDATE.write().unwrap();

            for key in &keys_consolidate {
                graph_consolidate_write.remove(key);

                debug!("fst key: {} cleared from consolidate register", key);
            }
        }

        // Step 3: Consolidate FSTs, one-by-one (sequential locking; this avoids global locks)
        let (mut count_moved, mut count_pushed, mut count_popped) = (0, 0, 0);

        {
            for key in &keys_consolidate {
                {
                    // As we may be renaming the FST file, ensure no consumer out of this is \
                    //   trying to access the FST file as it gets processed. This also waits for \
                    //   current consumers to finish reading the FST, and prevents any new \
                    //   consumer from opening it while we are not done there.
                    let _access = GRAPH_ACCESS_LOCK.write().unwrap();

                    let do_close = if let Some(store) = GRAPH_POOL.read().unwrap().get(key) {
                        debug!("fst key: {} consolidate started", key);

                        let consolidate_counts = Self::consolidate_item(store);

                        count_moved += consolidate_counts.1;
                        count_pushed += consolidate_counts.2;
                        count_popped += consolidate_counts.3;

                        debug!("fst key: {} consolidate complete", key);

                        // Should close this FST?
                        consolidate_counts.0
                    } else {
                        false
                    };

                    // Nuke old opened FST?
                    // Notice: last consolidated date will be bumped to a new date in the future \
                    //   when a push or pop operation will be done, thus effectively scheduling \
                    //   a consolidation in the future properly.
                    // Notice: we remove this one early as to release write lock early
                    if do_close {
                        GRAPH_POOL.write().unwrap().remove(key);
                    }
                }

                // Give a bit of time to other threads before continuing (a consolidate operation \
                //   must not block all other threads until it completes); this method tells the \
                //   thread scheduler to give a bit of priority to other threads, and get back \
                //   to this thread's work when other threads are done. On large setups, this \
                //   loop can starve other threads due to the locks used (unfortunately they \
                //   are all necessary).
                thread::yield_now();
            }
        }

        info!(
            "done scanning for fst store pool items to consolidate (move: {}, push: {}, pop: {})",
            count_moved, count_pushed, count_popped
        );
    }

    fn dump_action(
        action: &str,
        path_mode: StoreFSTPathMode,
        read_path: &Path,
        write_path: &Path,
        fn_item: &dyn Fn(&Path, &Path, &str, &str) -> Result<(), io::Error>,
    ) -> Result<(), io::Error> {
        let fst_extension = path_mode.extension();
        let fst_extension_len = fst_extension.len();

        // Iterate on FST collections
        for collection in fs::read_dir(read_path)? {
            let collection = collection?;

            // Actual collection found?
            if let (Ok(collection_file_type), Some(collection_name)) =
                (collection.file_type(), collection.file_name().to_str())
            {
                if collection_file_type.is_dir() {
                    debug!("fst collection ongoing {}: {}", action, collection_name);

                    // Create write folder for collection
                    fs::create_dir_all(write_path.join(collection_name))?;

                    // Iterate on FST collection buckets
                    for bucket in fs::read_dir(read_path.join(collection_name))? {
                        let bucket = bucket?;

                        // Actual bucket found?
                        if let (Ok(bucket_file_type), Some(bucket_file_name)) =
                            (bucket.file_type(), bucket.file_name().to_str())
                        {
                            let bucket_file_name_len = bucket_file_name.len();

                            if bucket_file_type.is_file()
                                && bucket_file_name_len > fst_extension_len
                                && bucket_file_name.ends_with(fst_extension)
                            {
                                // Acquire bucket name (from full file name)
                                let bucket_name =
                                    &bucket_file_name[..(bucket_file_name_len - fst_extension_len)];

                                debug!(
                                    "fst bucket ongoing {}: {}/{}",
                                    action, collection_name, bucket_name
                                );

                                fn_item(write_path, &bucket.path(), collection_name, bucket_name)?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn backup_item(
        backup_path: &Path,
        _origin_path: &Path,
        collection_name: &str,
        bucket_name: &str,
    ) -> Result<(), io::Error> {
        // Acquire access lock (in blocking write mode), and reference it in context
        // Notice: this prevents store to be acquired from any context
        let _access = GRAPH_ACCESS_LOCK.write().unwrap();

        // Generate path to FST backup
        let fst_backup_path = backup_path.join(collection_name).join(format!(
            "{}{}",
            bucket_name,
            StoreFSTPathMode::Backup.extension()
        ));

        debug!(
            "fst bucket: {}/{} backing up to path: {:?}",
            collection_name, bucket_name, fst_backup_path
        );

        // Erase any previously-existing FST backup
        fs::remove_file(&fst_backup_path).ok();

        // Stream actual FST data to FST backup
        let backup_fst_file = File::create(&fst_backup_path)?;
        let mut backup_fst_writer = BufWriter::new(backup_fst_file);

        let mut count_words = 0;

        // Convert names to hashes (as names are hashes encoded as base-16 strings, but we need \
        //   them as proper integers)
        if let (Ok(collection_radix), Ok(bucket_radix)) = (
            RadixNum::from_str(collection_name, ATOM_HASH_RADIX),
            RadixNum::from_str(bucket_name, ATOM_HASH_RADIX),
        ) {
            if let (Ok(collection_hash), Ok(bucket_hash)) =
                (collection_radix.as_decimal(), bucket_radix.as_decimal())
            {
                let origin_fst = StoreFSTBuilder::open(
                    collection_hash as StoreFSTAtom,
                    bucket_hash as StoreFSTAtom,
                )
                .map_err(|_| io_error!("graph open failure"))?;

                let mut origin_fst_stream = origin_fst.stream();

                while let Some(word) = origin_fst_stream.next() {
                    count_words += 1;

                    // Write word, and append a new line
                    backup_fst_writer.write_all(word)?;
                    backup_fst_writer.write_all(b"\n")?;
                }

                info!(
                    "fst bucket: {}/{} backed up to path: {:?} ({} words)",
                    collection_name, bucket_name, fst_backup_path, count_words
                );
            }
        }

        Ok(())
    }

    fn restore_item(
        _backup_path: &Path,
        origin_path: &Path,
        collection_name: &str,
        bucket_name: &str,
    ) -> Result<(), io::Error> {
        // Acquire access lock (in blocking write mode), and reference it in context
        // Notice: this prevents store to be acquired from any context
        let _access = GRAPH_ACCESS_LOCK.write().unwrap();

        debug!(
            "fst bucket: {}/{} restoring from path: {:?}",
            collection_name, bucket_name, origin_path
        );

        // Convert names to hashes (as names are hashes encoded as base-16 strings, but we need \
        //   them as proper integers)
        if let (Ok(collection_radix), Ok(bucket_radix)) = (
            RadixNum::from_str(collection_name, ATOM_HASH_RADIX),
            RadixNum::from_str(bucket_name, ATOM_HASH_RADIX),
        ) {
            if let (Ok(collection_hash), Ok(bucket_hash)) =
                (collection_radix.as_decimal(), bucket_radix.as_decimal())
            {
                // Force a FST store close
                StoreFSTBuilder::close(
                    collection_hash as StoreFSTAtom,
                    bucket_hash as StoreFSTAtom,
                );

                // Generate path to FST
                let fst_path = StoreFSTBuilder::path(
                    StoreFSTPathMode::Permanent,
                    collection_hash as StoreFSTAtom,
                    Some(bucket_hash as StoreFSTAtom),
                );

                // Remove existing FST data?
                if fst_path.exists() {
                    fs::remove_file(&fst_path)?;
                }

                // Stream backup words to restored FST
                let fst_writer = BufWriter::new(File::create(&fst_path)?);
                let fst_backup_reader = BufReader::new(File::open(&origin_path)?);

                let mut fst_builder = FSTSetBuilder::new(fst_writer)
                    .map_err(|_| io_error!("graph restore builder failure"))?;

                for word in fst_backup_reader.lines() {
                    let word = word?;

                    fst_builder
                        .insert(word)
                        .map_err(|_| io_error!("graph restore word insert failure"))?;
                }

                fst_builder
                    .finish()
                    .map_err(|_| io_error!("graph restore finish failure"))?;

                info!(
                    "fst bucket: {}/{} restored to path: {:?} from backup: {:?}",
                    collection_name, bucket_name, fst_path, origin_path
                );
            }
        }

        Ok(())
    }

    fn consolidate_item(store: &StoreFSTBox) -> (bool, usize, usize, usize) {
        let (mut should_close, mut count_moved, mut count_pushed, mut count_popped) =
            (false, 0, 0, 0);

        // Acquire write references to pending sets
        let (mut pending_push_write, mut pending_pop_write) = (
            store.pending.push.write().unwrap(),
            store.pending.pop.write().unwrap(),
        );

        // Do consolidate? (any change committed)
        // Notice: if both pending sets are empty do not consolidate as there may have been a \
        //   push then a pop of this push, nulling out any committed change.
        if pending_push_write.len() > 0 || pending_pop_write.len() > 0 {
            // Read old FST (or default to empty FST)
            if let Ok(old_fst) =
                StoreFSTBuilder::open(store.target.collection_hash, store.target.bucket_hash)
            {
                // Initialize the new FST (temporary)
                let bucket_tmp_path = StoreFSTBuilder::path(
                    StoreFSTPathMode::Temporary,
                    store.target.collection_hash,
                    Some(store.target.bucket_hash),
                );

                let bucket_tmp_path_parent = bucket_tmp_path.parent().unwrap();

                if fs::create_dir_all(&bucket_tmp_path_parent).is_ok() {
                    // Erase any previously-existing temporary FST (eg. process stopped while \
                    //   writing the temporary FST); there is no guarantee this succeeds.
                    fs::remove_file(&bucket_tmp_path).ok();

                    if let Ok(tmp_fst_file) = File::create(&bucket_tmp_path) {
                        let tmp_fst_writer = BufWriter::new(tmp_fst_file);

                        // Create a builder that can be used to insert new key-value pairs.
                        if let Ok(mut tmp_fst_builder) = FSTSetBuilder::new(tmp_fst_writer) {
                            // Convert push keys to an ordered vector
                            // Notice: we must go from a Vec to a VecDeque as to sort values, \
                            //   which is a requirement for FST insertions.
                            let mut ordered_push_vec: Vec<&[u8]> =
                                Vec::from_iter(pending_push_write.iter().map(|item| item.as_ref()));

                            ordered_push_vec.sort();

                            let mut ordered_push: VecDeque<&[u8]> =
                                VecDeque::from_iter(ordered_push_vec);

                            // Append words not in pop list to new FST (ie. old words minus pop \
                            //   words)
                            let mut old_fst_stream = old_fst.stream();

                            'old: while let Some(old_fst_word) = old_fst_stream.next() {
                                // Append new words from front? (ie. push words)
                                // Notice: as an FST is ordered, inserts would fail if they are \
                                //   committed out-of-order. Thus, the only way to check for \
                                //   order is there.
                                // Notice: a quick check is done before engaging in the loop, to \
                                //   prevent any de-optimized jump instruction, as we may call \
                                //   this code block a lot on large FSTs, and the loop should not \
                                //   be engaged that often on stabilized FSTs (ie. mature FSTs).
                                if let Some(push_first_ref) = ordered_push.front() {
                                    // Engage the loop?
                                    if *push_first_ref <= old_fst_word {
                                        while let Some(push_front_ref) = ordered_push.front() {
                                            if *push_front_ref <= old_fst_word {
                                                // Pop front item and consume it
                                                // Notice: as we validated previously that there \
                                                //   is a front value, this unwrap is safe.
                                                let push_front = ordered_push.pop_front().unwrap();

                                                if StoreFSTMisc::check_over_limits(
                                                    tmp_fst_builder.bytes_written() as usize,
                                                    count_pushed + count_moved,
                                                ) {
                                                    // FST cannot accept more items (limits reached)
                                                    warn!("limit reached on new from old in fst");

                                                    // Important: stop the main loop (limit reached)
                                                    break 'old;
                                                }

                                                if let Err(err) = tmp_fst_builder.insert(push_front)
                                                {
                                                    // Could not insert word in FST
                                                    error!(
                                                        "failed inserting new from old in fst: {}",
                                                        err
                                                    );
                                                } else {
                                                    // Word inserted in FST
                                                    count_pushed += 1;
                                                }

                                                // Continue scanning next word (may also come \
                                                //   before this FST word in order)
                                                continue;
                                            }

                                            // Important: stop loop on next front item (always \
                                            //   the same)
                                            break;
                                        }
                                    }
                                }

                                // Restore old word (if not popped)
                                if !pending_pop_write.contains(old_fst_word) {
                                    if StoreFSTMisc::check_over_limits(
                                        tmp_fst_builder.bytes_written() as usize,
                                        count_pushed + count_moved,
                                    ) {
                                        // FST cannot accept more items (limits reached)
                                        warn!("limit reached on old word in fst");

                                        // Important: stop the main loop (limit reached)
                                        break 'old;
                                    }

                                    if let Err(err) = tmp_fst_builder.insert(old_fst_word) {
                                        // Could not move word to FST
                                        error!("failed inserting old word in fst: {}", err);
                                    } else {
                                        // Word moved to FST
                                        count_moved += 1;
                                    }
                                } else {
                                    count_popped += 1;
                                }
                            }

                            // Complete FST with last pushed items
                            // Notice: this is necessary if the FST was empty, or if we have push \
                            //   items that come after the last ordered word of the FST.
                            while let Some(push_front) = ordered_push.pop_front() {
                                if StoreFSTMisc::check_over_limits(
                                    tmp_fst_builder.bytes_written() as usize,
                                    count_pushed + count_moved,
                                ) {
                                    // FST cannot accept more items (limits reached)
                                    warn!("limit reached on new word from complete in fst");

                                    // Important: stop the main loop (limit reached)
                                    break;
                                }

                                if let Err(err) = tmp_fst_builder.insert(push_front) {
                                    // Could not insert word in FST
                                    error!(
                                        "failed inserting new word from complete in fst: {}",
                                        err
                                    );
                                } else {
                                    // Word inserted in FST
                                    count_pushed += 1;
                                }
                            }

                            // Finish building new FST
                            if tmp_fst_builder.finish().is_ok() {
                                // Should close open store reference to old FST
                                should_close = true;

                                // Replace old FST with new FST (this nukes the old FST)
                                // Notice: there is no need to re-open the new FST, as it will be \
                                //   automatically opened on its next access.
                                let bucket_final_path = StoreFSTBuilder::path(
                                    StoreFSTPathMode::Permanent,
                                    store.target.collection_hash,
                                    Some(store.target.bucket_hash),
                                );

                                // Proceed temporary FST to final FST path rename
                                if std::fs::rename(&bucket_tmp_path, &bucket_final_path).is_ok() {
                                    info!("done consolidate fst at path: {:?}", bucket_final_path);
                                } else {
                                    error!(
                                        "error consolidating fst at path: {:?}",
                                        bucket_final_path
                                    );
                                }
                            } else {
                                error!(
                                    "error finishing building temporary fst at path: {:?}",
                                    bucket_tmp_path
                                );
                            }
                        } else {
                            error!(
                                "error starting building temporary fst at path: {:?}",
                                bucket_tmp_path
                            );
                        }
                    } else {
                        error!(
                            "error initializing temporary fst at path: {:?}",
                            bucket_tmp_path
                        );
                    }
                } else {
                    error!(
                        "error initializing temporary fst directory at path: {:?}",
                        bucket_tmp_path_parent
                    );
                }
            } else {
                error!("error opening old fst");
            }

            // Reset all pending sets
            *pending_push_write = HashSet::new();
            *pending_pop_write = HashSet::new();
        }

        (should_close, count_moved, count_pushed, count_popped)
    }
}

impl StoreGenericPool<StoreFSTKey, StoreFST, StoreFSTBuilder> for StoreFSTPool {}

impl StoreFSTBuilder {
    fn open(collection_hash: StoreFSTAtom, bucket_hash: StoreFSTAtom) -> Result<FSTSet, FSTError> {
        debug!(
            "opening finite-state transducer graph for collection: <{:x?}> and bucket: <{:x?}>",
            collection_hash, bucket_hash
        );

        let collection_bucket_path = Self::path(
            StoreFSTPathMode::Permanent,
            collection_hash,
            Some(bucket_hash),
        );

        if collection_bucket_path.exists() {
            // Open graph at path for collection
            // Notice: this is unsafe, as loaded memory is a memory-mapped file, that cannot be \
            //   guaranteed not to be muted while we own a read handle to it. Though, we use \
            //   higher-level locking mechanisms on all callers of this method, so we are safe.
            unsafe { FSTSet::from_path(collection_bucket_path) }
        } else {
            // FST does not exist on disk, generate an empty FST for now; until a consolidation \
            //   task occurs and populates the on-disk-FST.
            let empty_iter: Vec<&str> = Vec::new();

            FSTSet::from_iter(empty_iter)
        }
    }

    fn close(collection_hash: StoreFSTAtom, bucket_hash: StoreFSTAtom) {
        debug!(
            "closing finite-state transducer graph for collection: <{:x?}> and bucket: <{:x?}>",
            collection_hash, bucket_hash
        );

        let bucket_target = StoreFSTKey::from_atom(collection_hash, bucket_hash);

        GRAPH_POOL.write().unwrap().remove(&bucket_target);
        GRAPH_CONSOLIDATE.write().unwrap().remove(&bucket_target);
    }

    fn path(
        mode: StoreFSTPathMode,
        collection_hash: StoreFSTAtom,
        bucket_hash: Option<StoreFSTAtom>,
    ) -> PathBuf {
        let mut final_path = APP_CONF
            .store
            .fst
            .path
            .join(format!("{:x?}", collection_hash));

        if let Some(bucket_hash) = bucket_hash {
            final_path = final_path.join(format!("{:x?}{}", bucket_hash, mode.extension()));
        }

        final_path
    }
}

impl StoreGenericBuilder<StoreFSTKey, StoreFST> for StoreFSTBuilder {
    fn build(pool_key: StoreFSTKey) -> Result<StoreFST, ()> {
        Self::open(pool_key.collection_hash, pool_key.bucket_hash)
            .map(|graph| {
                let now = SystemTime::now();

                StoreFST {
                    graph,
                    target: pool_key,
                    pending: StoreFSTPending::default(),
                    last_used: Arc::new(RwLock::new(now)),
                    last_consolidated: Arc::new(RwLock::new(now)),
                }
            })
            .map_err(|err| {
                error!("failed opening fst: {}", err);
            })
    }
}

impl StoreFST {
    pub fn cardinality(&self) -> usize {
        self.graph.len()
    }

    pub fn as_stream(&self) -> FSTStream<AlwaysMatch> {
        self.graph.into_stream()
    }

    pub fn lookup_begins(&self, word: &str) -> Result<FSTStream<Regex>, ()> {
        // Notice: this regex maps over an unicode range, for speed reasons at scale. \
        //   We found out that the 'match any' syntax ('.*') was super-slow. Using the restrictive \
        //   syntax below divided the cost of eg. a search query by 2. The regex below has been \
        //   found out to be nearly zero-cost to compile and execute, for whatever reason.
        // Regex format: '{escaped_word}([{unicode_range}]*)'
        let mut regex_str = regex_escape(word);

        regex_str.push('(');

        let write_result = LexerRegexRange::from(word)
            .unwrap_or_default()
            .write_to(&mut regex_str);

        regex_str.push_str("*)");

        // Regex write failed? (this should not happen)
        if let Err(err) = write_result {
            error!(
                "could not lookup word in fst via 'begins': {} because regex write failed: {}",
                word, err
            );

            return Err(());
        }

        // Proceed word lookup
        debug!(
            "looking-up word in fst via 'begins': {} with regex: {}",
            word, regex_str
        );

        if let Ok(regex) = Regex::new(&regex_str) {
            Ok(self.graph.search(regex).into_stream())
        } else {
            Err(())
        }
    }

    pub fn lookup_typos(
        &self,
        word: &str,
        max_factor: Option<u32>,
    ) -> Result<FSTStream<Levenshtein>, ()> {
        // Allow more typos in word as the word gets longer, up to a maximum limit
        let mut typo_factor = match word.len() {
            1 | 2 | 3 => 0,
            4 | 5 | 6 => 1,
            7 | 8 | 9 => 2,
            _ => 3,
        };

        // Cap typo factor to set maximum?
        if let Some(max_factor) = max_factor {
            if typo_factor > max_factor {
                typo_factor = max_factor;
            }
        }

        debug!(
            "looking-up word in fst via 'typos': {} with typo factor: {}",
            word, typo_factor
        );

        if let Ok(fuzzy) = Levenshtein::new(word, typo_factor) {
            Ok(self.graph.search(fuzzy).into_stream())
        } else {
            Err(())
        }
    }

    pub fn should_consolidate(&self) {
        // Check if not already scheduled
        if !GRAPH_CONSOLIDATE.read().unwrap().contains(&self.target) {
            // Schedule target for next consolidation tick (ie. collection + bucket tuple)
            GRAPH_CONSOLIDATE.write().unwrap().insert(self.target);

            // Bump 'last consolidated' time, effectively de-bouncing consolidation to a fixed \
            //   and predictable tick time in the future.
            let mut last_consolidated_value = self.last_consolidated.write().unwrap();

            *last_consolidated_value = SystemTime::now();

            // Perform an early drop of the lock (frees up write lock early)
            drop(last_consolidated_value);

            info!("graph consolidation scheduled on pool key: {}", self.target);
        } else {
            debug!(
                "graph consolidation already scheduled on pool key: {}",
                self.target
            );
        }
    }
}

impl StoreGeneric for StoreFST {
    fn ref_last_used(&self) -> &RwLock<SystemTime> {
        &self.last_used
    }
}

impl StoreFSTActionBuilder {
    pub fn access(store: StoreFSTBox) -> StoreFSTAction {
        Self::build(store)
    }

    pub fn erase<'a, T: Into<&'a str>>(collection: T, bucket: Option<T>) -> Result<u32, ()> {
        Self::dispatch_erase("fst", collection, bucket)
    }

    fn build(store: StoreFSTBox) -> StoreFSTAction {
        StoreFSTAction { store }
    }
}

impl StoreGenericActionBuilder for StoreFSTActionBuilder {
    fn proceed_erase_collection(collection_str: &str) -> Result<u32, ()> {
        let path_mode = StoreFSTPathMode::Permanent;

        let collection_atom = StoreKeyerHasher::to_compact(collection_str);
        let collection_path = StoreFSTBuilder::path(path_mode, collection_atom, None);

        // Force a FST graph close (on all contained buckets)
        // Notice: we first need to scan for opened buckets in-memory, as not all FSTs may be \
        //   committed to disk; thus some FST stores that exist in-memory may not exist on-disk.
        let mut bucket_atoms: Vec<StoreFSTAtom> = Vec::new();

        {
            let graph_pool_read = GRAPH_POOL.read().unwrap();

            for target_key in graph_pool_read.keys() {
                if target_key.collection_hash == collection_atom {
                    bucket_atoms.push(target_key.bucket_hash);
                }
            }
        }

        if !bucket_atoms.is_empty() {
            debug!(
                "will force-close {} fst buckets for collection: {}",
                bucket_atoms.len(),
                collection_str
            );

            let (mut graph_pool_write, mut graph_consolidate_write) = (
                GRAPH_POOL.write().unwrap(),
                GRAPH_CONSOLIDATE.write().unwrap(),
            );

            for bucket_atom in bucket_atoms {
                debug!(
                    "fst bucket graph force close for bucket: {}/<{:x?}>",
                    collection_str, bucket_atom
                );

                let bucket_target = StoreFSTKey::from_atom(collection_atom, bucket_atom);

                graph_pool_write.remove(&bucket_target);
                graph_consolidate_write.remove(&bucket_target);
            }
        }

        // Remove all FSTs on-disk
        if collection_path.exists() {
            debug!(
                "fst collection store exists, erasing: {}/* at path: {:?}",
                collection_str, &collection_path
            );

            // Remove FST graph storage from filesystem
            let erase_result = fs::remove_dir_all(&collection_path);

            if erase_result.is_ok() {
                debug!("done with fst collection erasure");

                Ok(1)
            } else {
                Err(())
            }
        } else {
            debug!(
                "fst collection store does not exist, consider already erased: {}/* at path: {:?}",
                collection_str, &collection_path
            );

            Ok(0)
        }
    }

    fn proceed_erase_bucket(collection_str: &str, bucket_str: &str) -> Result<u32, ()> {
        debug!(
            "sub-erase on fst bucket: {} for collection: {}",
            bucket_str, collection_str
        );

        let (collection_atom, bucket_atom) = (
            StoreKeyerHasher::to_compact(collection_str),
            StoreKeyerHasher::to_compact(bucket_str),
        );

        let bucket_path = StoreFSTBuilder::path(
            StoreFSTPathMode::Permanent,
            collection_atom,
            Some(bucket_atom),
        );

        // Force a FST graph close
        StoreFSTBuilder::close(collection_atom, bucket_atom);

        // Remove FST on-disk
        if bucket_path.exists() {
            debug!(
                "fst bucket graph exists, erasing: {}/{} at path: {:?}",
                collection_str, bucket_str, &bucket_path
            );

            // Remove FST graph storage from filesystem
            let erase_result = fs::remove_file(&bucket_path);

            if erase_result.is_ok() {
                debug!("done with fst bucket erasure");

                Ok(1)
            } else {
                Err(())
            }
        } else {
            debug!(
                "fst bucket graph does not exist, consider already erased: {}/{} at path: {:?}",
                collection_str, bucket_str, &bucket_path
            );

            Ok(0)
        }
    }
}

impl StoreFSTAction {
    pub fn push_word(&self, word: &str) -> bool {
        // Word over limit? (abort, the FST does not perform well over large words)
        if Self::word_over_limit(word) {
            return false;
        }

        let word_bytes = word.as_bytes();

        // Nuke word from 'pop' set? (void a previous un-consolidated commit)
        if self.store.pending.pop.read().unwrap().contains(word_bytes) {
            self.store.pending.pop.write().unwrap().remove(word_bytes);
        }

        // Add word in 'push' set? (only if word is not in FST)
        // Notice: also check whether FST is over limits or not from there, to avoid stacking \
        //   words that could never be consolidated to final FST anyway.
        let graph_fst = self.store.graph.as_fst();

        if !self.store.graph.contains(&word)
            && !self.store.pending.push.read().unwrap().contains(word_bytes)
            && self.store.pending.push.read().unwrap().len() < APP_CONF.store.fst.graph.max_words
            && !StoreFSTMisc::check_over_limits(graph_fst.size(), graph_fst.len())
        {
            self.store
                .pending
                .push
                .write()
                .unwrap()
                .insert(word_bytes.to_vec());

            self.store.should_consolidate();

            // Pushed
            true
        } else {
            // Not pushed
            false
        }
    }

    pub fn pop_word(&self, word: &str) -> bool {
        // Word over limit? (abort, the FST does not perform well over large words)
        if Self::word_over_limit(word) {
            return false;
        }

        let word_bytes = word.as_bytes();

        // Nuke word from 'push' set? (void a previous un-consolidated commit)
        if self.store.pending.push.read().unwrap().contains(word_bytes) {
            self.store.pending.push.write().unwrap().remove(word_bytes);
        }

        // Add word in 'pop' set? (only if word is in FST)
        if self.store.graph.contains(word_bytes)
            && !self.store.pending.pop.read().unwrap().contains(word_bytes)
        {
            self.store
                .pending
                .pop
                .write()
                .unwrap()
                .insert(word_bytes.to_vec());

            self.store.should_consolidate();

            // Popped
            true
        } else {
            // Not popped
            false
        }
    }

    pub fn suggest_words(
        &self,
        from_word: &str,
        limit: usize,
        max_typo_factor: Option<u32>,
    ) -> Option<Vec<String>> {
        // Word over limit? (abort, the FST does not perform well over large words)
        if Self::word_over_limit(from_word) {
            return None;
        }

        let mut found_words = Vec::with_capacity(limit);

        // Try to complete provided word
        if let Ok(stream) = self.store.lookup_begins(from_word) {
            debug!("looking up for word: {} in 'begins' fst stream", from_word);

            Self::find_words_stream(stream, &mut found_words, limit);
        }

        // Try to fuzzy-suggest other words? (eg. correct typos)
        if found_words.len() < limit {
            if let Ok(stream) = self.store.lookup_typos(from_word, max_typo_factor) {
                debug!("looking up for word: {} in 'typos' fst stream", from_word);

                Self::find_words_stream(stream, &mut found_words, limit);
            }
        }

        if !found_words.is_empty() {
            Some(found_words)
        } else {
            None
        }
    }

    pub fn list_words(&self, limit: usize, offset: usize) -> Result<Vec<String>, ()> {
        let stream = self.store.as_stream();

        // Enumerate words from FST stream
        match stream
            .into_strs()
            .map(|words| words.into_iter().skip(offset).take(limit).collect())
        {
            Err(err) => {
                debug!("conversion of stream failed: {}", err.to_string());
                Err(())
            }
            Ok(words) => Ok(words),
        }
    }

    pub fn count_words(&self) -> usize {
        self.store.cardinality()
    }

    fn word_over_limit(word: &str) -> bool {
        if word.len() > WORD_LIMIT_LENGTH {
            debug!("got over-limit fst word: {}", word);

            true
        } else {
            false
        }
    }

    fn find_words_stream<A: Automaton>(
        mut stream: FSTStream<A>,
        found_words: &mut Vec<String>,
        limit: usize,
    ) {
        while let Some(word) = stream.next() {
            if let Ok(word_str) = str::from_utf8(word) {
                let word_string = word_str.to_string();

                if !found_words.contains(&word_string) {
                    found_words.push(word_string);

                    // Requested limit reached? Stop there.
                    if found_words.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
}

impl StoreFSTMisc {
    pub fn count_collection_buckets<'a, T: Into<&'a str>>(collection: T) -> Result<usize, ()> {
        let mut count = 0;

        let path_mode = StoreFSTPathMode::Permanent;

        let collection_atom = StoreKeyerHasher::to_compact(collection.into());
        let collection_path = StoreFSTBuilder::path(path_mode, collection_atom, None);

        if collection_path.exists() {
            // Scan collection directory for contained buckets (count them)
            if let Ok(entries) = fs::read_dir(&collection_path) {
                let fst_extension = path_mode.extension();
                let fst_extension_len = fst_extension.len();

                for entry in entries.flatten() {
                    if let Some(entry_name) = entry.file_name().to_str() {
                        let entry_name_len = entry_name.len();

                        // FST file found? This is a bucket.
                        if entry_name_len > fst_extension_len && entry_name.ends_with(fst_extension)
                        {
                            count += 1;
                        }
                    }
                }
            } else {
                error!("failed reading directory for count: {:?}", collection_path);

                return Err(());
            }
        }

        Ok(count)
    }

    fn check_over_limits(bytes_count: usize, words_count: usize) -> bool {
        // Over bytes limit?
        let max_size = APP_CONF.store.fst.graph.max_size * 1024;

        if bytes_count >= max_size {
            info!(
                "fst has exceeded maximum allowed bytes: {} over limit: {}",
                bytes_count, max_size
            );

            return true;
        }

        // Over words limit?
        if words_count >= APP_CONF.store.fst.graph.max_words {
            info!(
                "fst has exceeded maximum allowed words: {} over limit: {}",
                words_count, APP_CONF.store.fst.graph.max_words
            );

            return true;
        }

        // Not over limit
        false
    }
}

impl StoreFSTKey {
    pub fn from_atom(collection_hash: StoreFSTAtom, bucket_hash: StoreFSTAtom) -> StoreFSTKey {
        StoreFSTKey {
            collection_hash,
            bucket_hash,
        }
    }

    pub fn from_str(collection_str: &str, bucket_str: &str) -> StoreFSTKey {
        StoreFSTKey {
            collection_hash: StoreKeyerHasher::to_compact(collection_str),
            bucket_hash: StoreKeyerHasher::to_compact(bucket_str),
        }
    }
}

impl fmt::Display for StoreFSTKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<{:x?}>/<{:x?}>", self.collection_hash, self.bucket_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_acquires_graph() {
        assert!(StoreFSTPool::acquire("c:test:1", "b:test:1").is_ok());
    }

    #[test]
    fn it_janitors_graph() {
        StoreFSTPool::janitor();
    }

    #[test]
    fn it_proceeds_primitives() {
        let store = StoreFSTPool::acquire("c:test:2", "b:test:2").unwrap();

        assert!(store.lookup_typos("valerien", None).is_ok());
    }
}
