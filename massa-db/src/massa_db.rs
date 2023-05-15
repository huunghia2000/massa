use crate::{
    MassaDBError, CF_ERROR, CHANGE_ID_DESER_ERROR, CHANGE_ID_KEY, CRUD_ERROR, METADATA_CF,
    OPEN_ERROR, STATE_CF, STATE_HASH_ERROR, STATE_HASH_INITIAL_BYTES, STATE_HASH_KEY,
};
use massa_hash::{Hash, HashSerializer};
use massa_models::{
    error::ModelsError,
    slot::{Slot, SlotDeserializer, SlotSerializer},
    streaming_step::StreamingStep,
};
use massa_serialization::{DeserializeError, Deserializer, Serializer};

use rocksdb::{
    checkpoint::Checkpoint, ColumnFamily, ColumnFamilyDescriptor, Direction, IteratorMode, Options,
    WriteBatch, DB,
};
use std::{
    collections::BTreeMap,
    format,
    ops::Bound::{self, Excluded, Included},
    path::PathBuf,
    println,
};

type Key = Vec<u8>;
type Value = Vec<u8>;
pub type MassaDB = RawMassaDB<Slot, SlotSerializer, SlotDeserializer>;

#[derive(Debug, Clone)]
pub struct MassaDBConfig {
    pub path: PathBuf,
    pub max_history_length: usize,
    pub thread_count: u8,
}

#[derive(Debug, Clone)]
pub struct StreamBatch<ChangeID: PartialOrd + Ord + PartialEq + Eq + Clone + std::fmt::Debug> {
    pub updates_on_previous_elements: BTreeMap<Key, Option<Value>>,
    pub new_elements: BTreeMap<Key, Value>,
    pub change_id: ChangeID,
}

/*#[derive(Debug)]
struct ChangeHistoryElement<ChangeID: PartialOrd + Ord + PartialEq + Eq + Clone + std::fmt::Debug> {
    change_id: ChangeID,
    changes: BTreeMap<Key, Option<Value>>, // None means that the entry was deleted
}*/

#[derive()]
pub struct RawMassaDB<
    ChangeID: PartialOrd + Ord + PartialEq + Eq + Clone + std::fmt::Debug,
    ChangeIDSerializer: Serializer<ChangeID>,
    ChangeIDDeserializer: Deserializer<ChangeID>,
> {
    pub db: DB,
    config: MassaDBConfig,
    pub change_history: BTreeMap<ChangeID, BTreeMap<Key, Option<Value>>>,
    // Here we keep it in memory, but maybe this should be only on disk?
    pub cur_change_id: ChangeID,
    change_id_serializer: ChangeIDSerializer,
    change_id_deserializer: ChangeIDDeserializer,
}

impl<ChangeID, ChangeIDSerializer, ChangeIDDeserializer> std::fmt::Debug
    for RawMassaDB<ChangeID, ChangeIDSerializer, ChangeIDDeserializer>
where
    ChangeID: PartialOrd + Ord + PartialEq + Eq + Clone + std::fmt::Debug,
    ChangeIDSerializer: Serializer<ChangeID>,
    ChangeIDDeserializer: Deserializer<ChangeID>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawMassaDB")
            .field("db", &self.db)
            .field("config", &self.config)
            .field("change_history", &self.change_history)
            .field("cur_change_id", &self.cur_change_id)
            .finish()
    }
}

impl<ChangeID, ChangeIDSerializer, ChangeIDDeserializer>
    RawMassaDB<ChangeID, ChangeIDSerializer, ChangeIDDeserializer>
where
    ChangeID: PartialOrd + Ord + PartialEq + Eq + Clone + std::fmt::Debug,
    ChangeIDSerializer: Serializer<ChangeID>,
    ChangeIDDeserializer: Deserializer<ChangeID>,
{
    /// Used for bootstrap servers (get a new batch to stream to the client)
    ///
    /// Returns a StreamBatch<ChangeID>
    pub fn get_batch_to_stream(
        &self,
        last_obtained: Option<(Vec<u8>, ChangeID)>,
    ) -> Result<StreamBatch<ChangeID>, MassaDBError> {
        let (updates_on_previous_elements, max_key) = if let Some((max_key, last_change_id)) =
            last_obtained
        {
            match last_change_id.cmp(&self.cur_change_id) {
                std::cmp::Ordering::Greater => {
                    println!(
                        "We are asked for change: {:?}, but we only have changes up to: {:?}",
                        &last_change_id, &self.cur_change_id
                    );
                    return Err(MassaDBError::TimeError(String::from(
                        "we don't have this change yet on this node (it's in the future for us)",
                    )));
                }
                std::cmp::Ordering::Equal => {
                    (BTreeMap::new(), Some(max_key)) // no new updates
                }
                std::cmp::Ordering::Less => {
                    // We should send all the new updates since last_change_id

                    let cursor = self
                        .change_history
                        .lower_bound(Bound::Excluded(&last_change_id));

                    if cursor.peek_prev().is_none() {
                        return Err(MassaDBError::TimeError(String::from(
                            "all our changes are strictly after last_change_id, we can't be sure we did not miss any",
                        )));
                    }

                    match cursor.key() {
                        Some(cursor_change_id) => {
                            // We have to send all the updates since cursor_change_id
                            let mut updates: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
                            let iter = self
                                .change_history
                                .range((Bound::Excluded(cursor_change_id), Bound::Unbounded));
                            for (_change_id, changes) in iter {
                                updates.extend(
                                    changes
                                        .range((
                                            Bound::<Vec<u8>>::Unbounded,
                                            Included(max_key.clone()),
                                        ))
                                        .map(|(k, v)| (k.clone(), v.clone())),
                                );
                            }
                            (updates, Some(max_key))
                        }
                        None => (BTreeMap::new(), Some(max_key)), // no new updates
                    }
                }
            }
        } else {
            (BTreeMap::new(), None) // we start from the beginning, so no updates on previous elements
        };

        let mut new_elements = BTreeMap::new();
        let handle = self.db.cf_handle(STATE_CF).expect(CF_ERROR);

        // Creates an iterator from the next element after the last if defined, otherwise initialize it at the first key.
        let db_iterator = match max_key {
            None => self.db.iterator_cf(handle, IteratorMode::Start),
            Some(max_key) => {
                let mut iter = self
                    .db
                    .iterator_cf(handle, IteratorMode::From(&max_key, Direction::Forward));
                iter.next();
                iter
            }
        };

        for (serialized_key, serialized_value) in db_iterator.flatten() {
            if new_elements.len() < self.config.max_history_length {
                new_elements.insert(serialized_key.to_vec(), serialized_value.to_vec());
            } else {
                break;
            }
        }

        Ok(StreamBatch {
            new_elements,
            updates_on_previous_elements,
            change_id: self.cur_change_id.clone(),
        })
    }

    /// Used for:
    /// - Bootstrap clients, to write on disk a new received Stream (reset_history: true)
    /// - Normal operations, to write changes associated to a given change_id (reset_history: false)
    ///
    pub fn write_changes(
        &mut self,
        changes: BTreeMap<Key, Option<Value>>,
        change_id: Option<ChangeID>,
        tracked_hash: Option<Hash>,
        reset_history: bool,
    ) -> Result<(), MassaDBError> {
        if let Some(change_id) = change_id.clone() {
            if change_id < self.cur_change_id {
                println!(
                    "/!\\ TRIED TO APPLY change_id {:?} < cur_change_id {:?} /!\\",
                    change_id, self.cur_change_id
                );
                // TODO: Uncomment and solve issue
                /*return Err(MassaDBError::InvalidChangeID(String::from(
                    "change_id should monotonically increase after every write",
                )));*/
            }
            self.cur_change_id = change_id; // TODO also write the change_id to the db
        }

        let handle_state = self.db.cf_handle(STATE_CF).expect(CF_ERROR);
        //let handle_monotree = self.db.cf_handle(STATE_CF).expect(CF_ERROR);
        let mut batch: rocksdb::WriteBatchWithTransaction<false> = WriteBatch::default();

        for (key, value) in changes.iter() {
            if let Some(value) = value {
                batch.put_cf(handle_state, key, value);
                //self.monotree.put(key, value);
            } else {
                batch.delete_cf(handle_state, key);
                //self.monotree.delete_cf(handle, key, value);
            }
        }

        if let Some(change_id) = change_id {
            self.set_change_id(change_id, &mut batch);
            //self.monotree.put(key, value);
        }

        //self.hash_tracker.write_batch(&changes);

        if let Some(tracked_hash) = tracked_hash {
            let metadata_handle = self.db.cf_handle(METADATA_CF).expect(CF_ERROR);
            let mut serialized_tracked_hash = Vec::new();
            HashSerializer::new()
                .serialize(&tracked_hash, &mut serialized_tracked_hash)
                .map_err(|e| {
                    MassaDBError::HashError(format!("Can't serialize tracked hash: {}", e))
                })?;
            batch.put_cf(metadata_handle, STATE_HASH_KEY, serialized_tracked_hash);
        }

        self.db
            .write(batch)
            .map_err(|e| MassaDBError::RocksDBError(format!("Can't write batch to disk: {}", e)))?; // note that None values have to be deleted

        self.change_history
            .entry(self.cur_change_id.clone())
            .and_modify(|map| map.extend(changes.clone().into_iter()))
            .or_insert(changes);

        if reset_history {
            self.change_history.clear();
        }

        while self.change_history.len() > self.config.max_history_length {
            self.change_history.pop_first();
        }

        Ok(())
    }

    fn set_change_id(&self, change_id: ChangeID, batch: &mut WriteBatch) {
        let db = &self.db;
        let handle = db.cf_handle(METADATA_CF).expect(CF_ERROR);
        let mut change_id_bytes = Vec::new();
        self.change_id_serializer
            .serialize(&change_id, &mut change_id_bytes)
            .unwrap();

        batch.put_cf(handle, CHANGE_ID_KEY, &change_id_bytes);
    }

    pub fn get_change_id(&self) -> Result<ChangeID, ModelsError> {
        let db = &self.db;
        let handle = db.cf_handle(METADATA_CF).expect(CF_ERROR);

        let Ok(Some(change_id_bytes)) = db.get_pinned_cf(handle, CHANGE_ID_KEY) else {
            return Err(ModelsError::BufferError(String::from("Could not recover change_id in database")));
        };

        let (_rest, change_id) = self
            .change_id_deserializer
            .deserialize::<DeserializeError>(&change_id_bytes)
            .expect(CHANGE_ID_DESER_ERROR);

        Ok(change_id)
    }

    pub fn write_batch_bootstrap_client(
        &mut self,
        stream_changes: StreamBatch<ChangeID>,
    ) -> Result<StreamingStep<Key>, MassaDBError> {
        let mut changes = BTreeMap::new();

        let new_cursor = match stream_changes.new_elements.last_key_value() {
            Some((k, _)) => StreamingStep::Ongoing(k.clone()),
            None => StreamingStep::Finished(None),
        };

        changes.extend(stream_changes.updates_on_previous_elements);
        changes.extend(
            stream_changes
                .new_elements
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone()))),
        );

        self.write_changes(changes, Some(stream_changes.change_id), None, true)?;

        Ok(new_cursor)
    }

    /// Get the current state hash
    pub fn get_db_hash(&self) -> Hash {
        let db = &self.db;
        let handle = db.cf_handle(METADATA_CF).expect(CF_ERROR);
        if let Some(state_hash_bytes) = db
            .get_cf(handle, STATE_HASH_KEY)
            .expect(CRUD_ERROR)
            .as_deref()
        {
            Hash::from_bytes(state_hash_bytes.try_into().expect(STATE_HASH_ERROR))
        } else {
            Hash::from_bytes(STATE_HASH_INITIAL_BYTES)
        }
    }
}

impl RawMassaDB<Slot, SlotSerializer, SlotDeserializer> {
    /// Returns a new `MassaDB` instance
    pub fn new(config: MassaDBConfig) -> Self {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        let db = DB::open_cf_descriptors(
            &db_opts,
            &config.path,
            vec![
                ColumnFamilyDescriptor::new(STATE_CF, Options::default()),
                ColumnFamilyDescriptor::new(METADATA_CF, Options::default()),
            ],
        )
        .expect(OPEN_ERROR);

        let change_id_deserializer = SlotDeserializer::new(
            (Included(u64::MIN), Included(u64::MAX)),
            (Included(0), Excluded(config.thread_count)),
        );

        Self {
            db,
            config,
            change_history: BTreeMap::new(),
            cur_change_id: Slot {
                period: 0,
                thread: 0,
            },
            change_id_serializer: SlotSerializer::new(),
            change_id_deserializer,
        }
    }

    /// Creates a new hard copy of the DB, for the given slot
    pub fn backup_db(&self, slot: Slot) {
        let db = &self.db;
        let mut subpath = String::from("backup_");
        subpath.push_str(slot.period.to_string().as_str());
        subpath.push('_');
        subpath.push_str(slot.thread.to_string().as_str());

        Checkpoint::new(db)
            .expect("Cannot init checkpoint")
            .create_checkpoint(db.path().join(subpath))
            .expect("Failed to create checkpoint");
    }

    /// Writes the batch to the DB
    pub fn write_batch(&mut self, batch: DBBatch, change_id: Option<Slot>) {
        self.write_changes(batch.changes, change_id, Some(batch.state_hash), false)
            .expect(CRUD_ERROR);

        /*let db = &self.db;
        let handle = db.cf_handle(METADATA_CF).expect(CF_ERROR);
        batch
            .write_batch
            .put_cf(handle, STATE_HASH_KEY, batch.state_hash.to_bytes());
        db.write(batch.write_batch).expect(CRUD_ERROR);*/
    }

    /// Utility function to put / update a key & value and perform the hash XORs
    pub fn put_or_update_entry_value(
        &self,
        handle: &ColumnFamily,
        batch: &mut DBBatch,
        key: Vec<u8>,
        value: &[u8],
    ) {
        let db = &self.db;

        if let Some(added_hash) = batch.aeh_list.get(&key) {
            batch.state_hash ^= *added_hash;
        } else if let Some(prev_bytes) = db.get_pinned_cf(handle, &key).expect(CRUD_ERROR) {
            batch.state_hash ^= Hash::compute_from(&[&key, &prev_bytes[..]].concat());
        }
        let hash = Hash::compute_from(&[&key, value].concat());
        batch.state_hash ^= hash;
        batch.aeh_list.insert(key.clone(), hash);

        //batch.write_batch.put_cf(handle, key, value);

        batch.changes.insert(key, Some(value.to_vec()));
    }

    /// Utility function to delete a key & value and perform the hash XORs
    pub fn delete_key(&self, handle: &ColumnFamily, batch: &mut DBBatch, key: Vec<u8>) {
        let db = &self.db;
        if let Some(added_hash) = batch.aeh_list.get(&key) {
            batch.state_hash ^= *added_hash;
        } else if let Some(prev_bytes) = db.get_pinned_cf(handle, &key).expect(CRUD_ERROR) {
            batch.state_hash ^= Hash::compute_from(&[&key, &prev_bytes[..]].concat());
        }

        //batch.write_batch.delete_cf(handle, key);

        batch.changes.insert(key, None);
    }

    /// Get the current state hash
    pub fn compute_hash_from_scratch(&self) -> Hash {
        let db = &self.db;

        let handle_state = db.cf_handle(STATE_CF).expect(CF_ERROR);
        let handle_metadata = db.cf_handle(METADATA_CF).expect(CF_ERROR);

        let mut hash = Hash::from_bytes(STATE_HASH_INITIAL_BYTES);

        for (serialized_key, serialized_value) in
            db.iterator_cf(handle_state, IteratorMode::Start).flatten()
        {
            hash ^= Hash::compute_from(&[serialized_key, serialized_value].concat());
        }

        if let Ok(Some(serialized_value)) = db.get_cf(handle_metadata, CHANGE_ID_KEY) {
            hash ^= Hash::compute_from(&[CHANGE_ID_KEY.to_vec(), serialized_value].concat());
        }

        hash
    }

    /// Utility function to delete all keys in a prefix
    pub fn delete_prefix(&mut self, prefix: &str, change_id: Option<Slot>) {
        let db = &self.db;

        let handle = db.cf_handle(STATE_CF).expect(CF_ERROR);
        let mut batch = DBBatch::new(self.get_db_hash());
        for (serialized_key, _) in db.prefix_iterator_cf(handle, prefix).flatten() {
            if !serialized_key.starts_with(prefix.as_bytes()) {
                break;
            }

            self.delete_key(handle, &mut batch, serialized_key.to_vec());
        }
        self.write_batch(batch, change_id);
    }

    /// TODO:
    /// Deserialize the entire DB and check the data. Useful to check after bootstrap.
    pub fn is_db_valid(&self) -> bool {
        todo!();
    }
}

/// Batch containing write operations to perform on disk and cache for hash computing
pub struct DBBatch {
    pub changes: BTreeMap<Key, Option<Value>>,
    // State hash in the current batch
    pub state_hash: Hash,
    // Added entry hashes in the current batch
    pub aeh_list: BTreeMap<Vec<u8>, Hash>,
}

impl DBBatch {
    pub fn new(state_hash: Hash) -> Self {
        Self {
            changes: BTreeMap::new(),
            state_hash,
            aeh_list: BTreeMap::new(),
        }
    }
}

/*
impl MonotreeDatabase for MassaDB {
    put() -> ajoute au current_batch_for_monotree {
        handle = get_handle(MONOTREE_CF)
    }
    get() -> en prio get dans le current_batch_for_monotree puis dans la db {
        handle = get_handle(MONOTREE_CF)
    }
    delete() -> ajoute le delete au current_batch_for_monotree {
        handle = get_handle(MONOTREE_CF)
    }
}

MassaDB {
    monotree: Monotree<Blake3, Self>
    batch: WriteBatch
}

impl MassaDB {
    write_change() {

        for all changes:
        {
            if change is a put {
                self.batch.put_cf()
                monotree.put(change.key, change.value)
            }
            if change is a delete {
                self.batch.delete_cf()
                monotree.delete(change.key)
            }
        }

        db.write(self.batch);

    }
}*/
