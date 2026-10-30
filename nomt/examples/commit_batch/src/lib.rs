use anyhow::Result;
use nomt::{
    hasher::Blake3Hasher, KeyReadWrite, Nomt, Options, Root, SessionParams, Witness, WitnessMode,
};
use sha2::Digest;

const NOMT_DB_FOLDER: &str = "nomt_db";

pub struct NomtDB;

impl NomtDB {
    pub fn commit_batch() -> Result<(Root, Root, Witness)> {
        // Define the options used to open NOMT
        let mut opts = Options::new();
        opts.path(NOMT_DB_FOLDER);
        opts.commit_concurrency(1);

        // Open NOMT database, it will create the folder if it does not exist
        let nomt = Nomt::<Blake3Hasher>::open(opts)?;

        // Create a new Session object
        //
        // During a session, the backend is responsible for returning read keys
        // and receiving hints about future writes
        //
        // Writes do not occur immediately, instead,
        // they are cached and applied all at once later on
        let session =
            nomt.begin_session(SessionParams::default().witness_mode(WitnessMode::read_write()));

        // Here we will move the data saved under b"key1" to b"key2" and deletes it
        //
        // NOMT expects keys to be uniformly distributed across the key space
        let key_path_1 = sha2::Sha256::digest(b"key1").into();
        let key_path_2 = sha2::Sha256::digest(b"key2").into();

        // First, read what is under key_path_1
        //
        // `read` will immediately return the value present in the database
        let value = session.read(key_path_1)?;

        // We are going to perform writes on both key-paths, so we have NOMT warm up the on-disk
        // data for both.
        session.warm_up(key_path_1);
        session.warm_up(key_path_2);

        // Retrieve the previous value of the root before committing changes
        let prev_root = nomt.root();

        // To commit the batch to the backend we need to collect every
        // performed actions into a vector where items are ordered by the key_path
        let mut actual_access: Vec<_> = vec![
            (key_path_1, KeyReadWrite::ReadThenWrite(value.clone(), None)),
            (key_path_2, KeyReadWrite::Write(value)),
        ];
        actual_access.sort_by_key(|(k, _)| *k);

        // The final step in handling a session involves committing all changes
        // to update the trie structure and obtaining the new root of the trie,
        // along with a witness and the witnessed operations.
        let mut finished = session.finish(actual_access).unwrap();

        // This field is set because the finished session was configured with
        // `WitnessMode::read_write`.
        let witness = finished.take_witness().unwrap();
        let root = finished.root();
        finished.commit(&nomt)?;

        Ok((prev_root, root, witness))
    }
}
