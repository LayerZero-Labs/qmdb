use rand::Rng;

pub enum SwarmFeatures {
    /// Trigger on and off trickfs to return ENOSPC.
    ///
    /// Will be used only when the assigned memory is smaller than
    /// `TRICKFS_MEMORY_THRESHOLD`.
    TrickfsENOSPC,
    /// Trigger on and off trickfs to inject latencies in every response.
    ///
    /// Will be used only when the assigned memory is smaller than
    /// `TRICKFS_MEMORY_THRESHOLD`.
    TrickfsLatencyInjection,
    /// Ensure that the changeset was correctly applied
    EnsureChangeset,
    /// Randomly sample the state after every crash or rollback to check the
    /// correctness of the state of the database.
    SampleSnapshot,
    /// Whether merkle page fetches should be warmed up while sessions are ongoing.
    WarmUp,
    /// Whether to preallocate the hashtable file.
    PreallocateHt,
    /// Whether each commit should perform a bunch of reads before applying a changeset.
    Read,
    /// Whether rollback should be performed.
    Rollback,
    /// Whether rollback crash should be exercised.
    RollbackCrash,
    /// Whether commit crash should be exercised.
    CommitCrash,
    /// Whether to prepopulate the upper levels of the page cache on startup.
    PrepopulatePageCache,
    /// Whether new keys should be inserted during commits.
    NewKeys,
    /// Whether keys should be deleted during commits.
    DeleteKeys,
    /// Whether keys should be updated during commits.
    UpdateKeys,
    /// Whether inserted values should be overflow ones.
    OverflowValues,
}

pub fn new_features_set(rng: &mut rand_pcg::Pcg64) -> Vec<SwarmFeatures> {
    let mut features = vec![
        SwarmFeatures::EnsureChangeset,
        SwarmFeatures::SampleSnapshot,
        SwarmFeatures::WarmUp,
        SwarmFeatures::PreallocateHt,
        SwarmFeatures::Read,
        SwarmFeatures::Rollback,
        SwarmFeatures::RollbackCrash,
        SwarmFeatures::CommitCrash,
        SwarmFeatures::PrepopulatePageCache,
        SwarmFeatures::NewKeys,
        SwarmFeatures::DeleteKeys,
        SwarmFeatures::UpdateKeys,
        SwarmFeatures::OverflowValues,
    ];

    // Features removal mechanism -> coin tossing for almost every feature.
    for idx in (0..features.len()).rev() {
        if rng.gen_bool(0.5) {
            features.remove(idx);
        }
    }

    // Trickfs related features are a little bit treated differently.
    // Trickfs rely entirely on memory thus features related to it gets executed
    // less often, in particular they will follow a bias coin tossing with
    // `p = 0.052` being the probability of being added to the set of features.
    //
    // The probability of using Trickfs is 10% (= p*p + 2 * (p * (1-p))).
    let p = 0.052;
    if rng.gen_bool(p) {
        features.push(SwarmFeatures::TrickfsLatencyInjection);
    }
    if rng.gen_bool(p) {
        features.push(SwarmFeatures::TrickfsENOSPC);
    }

    features
}
