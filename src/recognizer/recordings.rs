use anyhow::Result;
use gtk::{
    gio,
    glib::{self, clone},
    prelude::*,
    subclass::prelude::*,
};
use heed::types::{SerdeBincode, Str};
use indexmap::IndexMap;
use once_cell::unsync::OnceCell;

use std::cell::RefCell;

use super::Recording;
use crate::{db::RECORDINGS_DB_NAME, debug_assert_or_log, utils};

const RECORDING_NOTIFY_HANDLER_ID_KEY: &str = "mousai-recording-notify-handler-id";

type RecordingDatabase = heed::Database<Str, SerdeBincode<Recording>>;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct Recordings {
        pub(super) list: RefCell<IndexMap<String, Recording>>,

        pub(super) db: OnceCell<(heed::Env, RecordingDatabase)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Recordings {
        const NAME: &'static str = "MsaiRecordings";
        type Type = super::Recordings;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for Recordings {}

    impl ListModelImpl for Recordings {
        fn item_type(&self) -> glib::Type {
            Recording::static_type()
        }

        fn n_items(&self) -> u32 {
            self.list.borrow().len() as u32
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            self.list
                .borrow()
                .get_index(position as usize)
                .map(|(_, v)| v.upcast_ref::<glib::Object>())
                .cloned()
        }
    }
}

glib::wrapper! {
    pub struct Recordings(ObjectSubclass<imp::Recordings>)
        @implements gio::ListModel;
}

impl Recordings {
    /// Load from the `saved_recordings` table in the database
    pub fn load_from_env(env: heed::Env) -> Result<Self> {
        let now = std::time::Instant::now();

        let mut wtxn = env.write_txn()?;
        let db: RecordingDatabase = env.create_database(&mut wtxn, Some(RECORDINGS_DB_NAME))?;
        let recordings = db
            .iter(&wtxn)?
            .map(|item| item.map(|(id, recording)| (id.to_string(), recording)))
            .collect::<Result<IndexMap<_, _>, _>>()?;
        wtxn.commit()?;

        tracing::debug!(
            "Loaded {} saved recordings in {:?}",
            recordings.len(),
            now.elapsed()
        );

        let this = glib::Object::new::<Self>();

        for (recording_id, recording) in recordings.iter() {
            this.bind_recording_to_items_changed_and_db(recording_id, recording);
        }

        let imp = this.imp();
        imp.list.replace(recordings);
        imp.db.set((env, db)).unwrap();

        Ok(this)
    }

    pub fn insert(&self, recording: Recording) {
        let recording_id = utils::generate_unique_id();

        let (env, db) = self.db();
        let mut wtxn = env.write_txn().unwrap();
        db.put(&mut wtxn, &recording_id, &recording).unwrap();
        wtxn.commit().unwrap();

        self.bind_recording_to_items_changed_and_db(&recording_id, &recording);

        let (position, last_value) = self
            .imp()
            .list
            .borrow_mut()
            .insert_full(recording_id, recording);
        debug_assert_or_log!(last_value.is_none());

        self.items_changed(position as u32, 0, 1);
    }

    pub fn peek_filtered(&self, filter_func: impl Fn(&Recording) -> bool) -> Vec<Recording> {
        let imp = self.imp();

        imp.list
            .borrow()
            .iter()
            .filter(|(_, recording)| filter_func(recording))
            .map(|(_, recording)| recording.clone())
            .collect()
    }

    pub fn take_filtered(&self, filter_func: impl Fn(&Recording) -> bool) -> Vec<Recording> {
        let imp = self.imp();

        let mut to_take_ids = Vec::new();
        for (id, recording) in imp.list.borrow().iter() {
            if filter_func(recording) {
                to_take_ids.push(id.to_string());
            }
        }

        let (env, db) = self.db();
        let mut wtxn = env.write_txn().unwrap();
        for key in &to_take_ids {
            let existed = db.delete(&mut wtxn, key).unwrap();
            debug_assert_or_log!(existed);
        }
        wtxn.commit().unwrap();

        let mut taken = Vec::new();
        for id in &to_take_ids {
            let (index, _, recording) = imp
                .list
                .borrow_mut()
                .shift_remove_full(id.as_str())
                .expect("id exists");
            unbind_recording_to_items_changed_and_db(&recording);
            self.items_changed(index as u32, 1, 0); // TODO Optimize this
            taken.push(recording);
        }

        taken
    }

    pub fn is_empty(&self) -> bool {
        self.n_items() == 0
    }

    fn db(&self) -> &(heed::Env, RecordingDatabase) {
        self.imp().db.get().unwrap()
    }

    fn bind_recording_to_items_changed_and_db(&self, recording_id: &str, recording: &Recording) {
        unsafe {
            let recording_id = recording_id.to_string();
            let handler_id = recording.connect_notify_local(
                None,
                clone!(@weak self as obj => move |recording, _| {
                    let (env, db) = obj.db();
                    let mut wtxn = env.write_txn().unwrap();
                    db.put(&mut wtxn, &recording_id, recording).unwrap();
                    wtxn.commit().unwrap();

                    let index = obj
                        .imp()
                        .list
                        .borrow()
                        .get_index_of(&recording_id)
                        .unwrap();
                    obj.items_changed(index as u32, 1, 1);
                }),
            );
            recording.set_data(RECORDING_NOTIFY_HANDLER_ID_KEY, handler_id);
        }
    }
}

fn unbind_recording_to_items_changed_and_db(recording: &Recording) {
    unsafe {
        let handler_id = recording
            .steal_data::<glib::SignalHandlerId>(RECORDING_NOTIFY_HANDLER_ID_KEY)
            .unwrap();
        recording.disconnect(handler_id);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::rc::Rc;

    use crate::{
        core::DateTime,
        db,
        model::{Song, SongId},
        recognizer::recording::BoxedRecognizeResult,
    };

    fn new_test_recording(bytes: &'static [u8]) -> Recording {
        Recording::new(&glib::Bytes::from_static(bytes), &DateTime::now_local())
    }

    fn new_test_song(id: &str) -> Song {
        Song::builder(&SongId::for_test(id), id, id, id).build()
    }

    fn assert_n_items_and_db_count_eq(recordings: &Recordings, n: usize) {
        assert_eq!(recordings.n_items(), n as u32);

        let (env, db) = recordings.db();
        let rtxn = env.read_txn().unwrap();
        assert_eq!(db.len(&rtxn).unwrap(), n as u64);
    }

    #[track_caller]
    fn assert_equal_recognize_result_song_id(a: &Recording, b: &Recording) {
        match (a.recognize_result(), b.recognize_result()) {
            (Some(result_a), Some(result_b)) => assert_eq!(
                result_a.0.as_ref().unwrap().id(),
                result_b.0.as_ref().unwrap().id()
            ),
            (a, b) => assert_eq!(a, b),
        }
    }

    /// Must have exactly 2 recordings
    fn assert_synced_to_db(recordings: &Recordings) {
        assert_n_items_and_db_count_eq(recordings, 2);

        let (env, db) = recordings.db();

        // Test if the items are synced to the database
        let rtxn = env.read_txn().unwrap();
        for item in db.iter(&rtxn).unwrap() {
            let (_, recording) = item.unwrap();
            assert!(recording.recognize_result().is_none());
        }
        for (id, recording) in recordings.imp().list.borrow().iter() {
            assert_equal_recognize_result_song_id(&db.get(&rtxn, id).unwrap().unwrap(), recording);
        }
        drop(rtxn);

        recordings
            .item(0)
            .and_downcast::<Recording>()
            .unwrap()
            .set_recognize_result(Some(BoxedRecognizeResult(Ok(new_test_song("a")))));
        assert_n_items_and_db_count_eq(recordings, 2);

        // Test if the items are synced to the database even
        // after the recording is modified
        let rtxn = env.read_txn().unwrap();
        for (id, recording) in recordings.imp().list.borrow().iter() {
            assert_equal_recognize_result_song_id(&db.get(&rtxn, id).unwrap().unwrap(), recording);
        }
        drop(rtxn);

        recordings
            .item(1)
            .and_downcast::<Recording>()
            .unwrap()
            .set_recognize_result(Some(BoxedRecognizeResult(Ok(new_test_song("b")))));
        assert_n_items_and_db_count_eq(recordings, 2);

        let rtxn = env.read_txn().unwrap();
        for item in db.iter(&rtxn).unwrap() {
            let (_, recording) = item.unwrap();
            assert!(recording.recognize_result().is_some());
        }
        for (id, recording) in recordings.imp().list.borrow().iter() {
            assert_equal_recognize_result_song_id(&db.get(&rtxn, id).unwrap().unwrap(), recording);
        }
        drop(rtxn);

        for (_, recording) in recordings.imp().list.borrow().iter() {
            recording.set_recognize_result(None::<BoxedRecognizeResult>);
        }

        let rtxn = env.read_txn().unwrap();
        for item in db.iter(&rtxn).unwrap() {
            let (_, recording) = item.unwrap();
            assert!(recording.recognize_result().is_none());
        }
    }

    #[test]
    fn load_from_db() {
        let (env, _tempdir) = db::new_test_env();
        let mut wtxn = env.write_txn().unwrap();
        let db: RecordingDatabase = env
            .create_database(&mut wtxn, Some(RECORDINGS_DB_NAME))
            .unwrap();
        db.put(&mut wtxn, "a", &new_test_recording(b"A")).unwrap();
        db.put(&mut wtxn, "b", &new_test_recording(b"B")).unwrap();
        wtxn.commit().unwrap();

        let recordings = Recordings::load_from_env(env).unwrap();

        let items = recordings.peek_filtered(|_| true);
        assert!(items.iter().any(|i| i.bytes().as_ref() == b"A"));
        assert!(items.iter().any(|i| i.bytes().as_ref() == b"A"));

        assert_n_items_and_db_count_eq(&recordings, 2);
        assert_synced_to_db(&recordings);
    }

    #[test]
    fn insert() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();
        assert_n_items_and_db_count_eq(&recordings, 0);

        recordings.insert(new_test_recording(b"a"));
        assert_n_items_and_db_count_eq(&recordings, 1);
        assert_eq!(
            recordings
                .item(0)
                .and_downcast::<Recording>()
                .unwrap()
                .bytes()
                .as_ref(),
            b"a"
        );

        recordings.insert(new_test_recording(b"b"));
        assert_n_items_and_db_count_eq(&recordings, 2);
        assert_eq!(
            recordings
                .item(1)
                .and_downcast::<Recording>()
                .unwrap()
                .bytes()
                .as_ref(),
            b"b"
        );

        assert_synced_to_db(&recordings);
    }

    #[test]
    fn insert_items_changed() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();

        recordings.connect_items_changed(|_, index, removed, added| {
            assert_eq!(index, 0);
            assert_eq!(removed, 0);
            assert_eq!(added, 1);
        });

        recordings.insert(new_test_recording(b"a"));
    }

    #[test]
    fn peek_filtered() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();
        assert_n_items_and_db_count_eq(&recordings, 0);

        assert!(recordings.peek_filtered(|_| false).is_empty());
        assert!(recordings.peek_filtered(|_| true).is_empty());
        assert!(recordings
            .peek_filtered(|r| r.bytes().as_ref() == b"a")
            .is_empty());

        recordings.insert(new_test_recording(b"a"));
        assert!(recordings.peek_filtered(|_| false).is_empty());
        assert_eq!(recordings.peek_filtered(|_| true).len(), 1);
        assert_eq!(
            recordings
                .peek_filtered(|r| r.bytes().as_ref() == b"a")
                .len(),
            1,
        );
        assert_n_items_and_db_count_eq(&recordings, 1);

        recordings.insert(new_test_recording(b"b"));
        assert!(recordings.peek_filtered(|_| false).is_empty());
        assert_eq!(recordings.peek_filtered(|_| true).len(), 2);
        assert_eq!(
            recordings
                .peek_filtered(|r| r.bytes().as_ref() == b"a")
                .len(),
            1,
        );
        assert_n_items_and_db_count_eq(&recordings, 2);
    }

    #[test]
    fn peek_filtered_items_changed() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();

        let a_handler_id = recordings.connect_items_changed(|_, _, _, _| {
            panic!("Recordings::items_changed should not be emitted when peek_filtered is called");
        });
        recordings.peek_filtered(|_| true);
        recordings.peek_filtered(|_| false);

        recordings.disconnect(a_handler_id);
        recordings.insert(new_test_recording(b"a"));

        recordings.connect_items_changed(|_, _, _, _| {
            panic!("Recordings::items_changed should not be emitted when peek_filtered is called");
        });
        recordings.peek_filtered(|_| true);
        recordings.peek_filtered(|_| false);
    }

    #[test]
    fn take_filtered() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();

        assert_n_items_and_db_count_eq(&recordings, 0);
        assert!(recordings.take_filtered(|_| false).is_empty());
        assert!(recordings.take_filtered(|_| true).is_empty());
        assert!(recordings
            .take_filtered(|r| r.bytes().as_ref() == b"a")
            .is_empty());

        recordings.insert(new_test_recording(b"a"));
        assert_n_items_and_db_count_eq(&recordings, 1);
        assert!(recordings.take_filtered(|_| false).is_empty());
        assert_n_items_and_db_count_eq(&recordings, 1);
        assert_eq!(recordings.take_filtered(|_| true).len(), 1);
        assert_n_items_and_db_count_eq(&recordings, 0);

        recordings.insert(new_test_recording(b"a"));
        recordings.insert(new_test_recording(b"b"));
        assert_n_items_and_db_count_eq(&recordings, 2);
        assert!(recordings.take_filtered(|_| false).is_empty());
        assert_n_items_and_db_count_eq(&recordings, 2);

        let taken = recordings.take_filtered(|_| true);
        assert_eq!(taken.len(), 2);
        assert_n_items_and_db_count_eq(&recordings, 0);

        // Ensure that the removed recordings is not added back to the database
        for recording in taken {
            assert!(recording.recognize_result().is_none());
            recording.set_recognize_result(Some(BoxedRecognizeResult(Ok(new_test_song("a")))));
        }
        assert_n_items_and_db_count_eq(&recordings, 0);

        recordings.insert(new_test_recording(b"a"));
        recordings.insert(new_test_recording(b"b"));
        assert_eq!(
            recordings
                .take_filtered(|r| r.bytes().as_ref() == b"a")
                .len(),
            1,
        );
        assert_n_items_and_db_count_eq(&recordings, 1);
        assert_eq!(
            recordings
                .item(0)
                .and_downcast::<Recording>()
                .unwrap()
                .bytes()
                .as_ref(),
            b"b"
        );
    }

    #[test]
    fn take_filtered_items_changed() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();
        recordings.insert(new_test_recording(b"a"));
        recordings.insert(new_test_recording(b"b"));

        let calls_output = Rc::new(RefCell::new(Vec::new()));

        let calls_output_clone = Rc::clone(&calls_output);
        let handler_id = recordings.connect_items_changed(move |_, index, removed, added| {
            calls_output_clone
                .borrow_mut()
                .push((index, removed, added));
        });

        recordings.take_filtered(|_| false);
        assert!(calls_output.take().is_empty());

        recordings.take_filtered(|_| true);
        assert_eq!(calls_output.take(), vec![(0, 1, 0), (0, 1, 0)]);

        recordings.block_signal(&handler_id);
        recordings.insert(new_test_recording(b"a"));
        recordings.insert(new_test_recording(b"b"));
        recordings.unblock_signal(&handler_id);

        recordings.take_filtered(|r| r.bytes().as_ref() == b"a");
        assert_eq!(calls_output.take(), vec![(0, 1, 0)]);
    }

    #[test]
    fn recording_notify_items_changed() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();

        let recording_a = new_test_recording(b"a");
        recordings.insert(recording_a.clone());
        let recording_b = new_test_recording(b"b");
        recordings.insert(recording_b.clone());

        let calls_output = Rc::new(RefCell::new(Vec::new()));

        let calls_output_clone = Rc::clone(&calls_output);
        recordings.connect_items_changed(move |_, index, removed, added| {
            calls_output_clone
                .borrow_mut()
                .push((index, removed, added));
        });

        recording_a.set_recognize_result(Some(BoxedRecognizeResult(Ok(new_test_song("a")))));
        assert_eq!(calls_output.take(), vec![(0, 1, 1)]);

        recording_b.set_recognize_result(Some(BoxedRecognizeResult(Ok(new_test_song("a")))));
        assert_eq!(calls_output.take(), vec![(1, 1, 1)]);
    }

    #[test]
    fn is_empty() {
        let (env, _tempdir) = db::new_test_env();
        let recordings = Recordings::load_from_env(env).unwrap();

        assert!(recordings.is_empty());
        assert_n_items_and_db_count_eq(&recordings, 0);

        recordings.insert(new_test_recording(b"a"));
        assert!(!recordings.is_empty());
        assert_n_items_and_db_count_eq(&recordings, 1);
    }
}
