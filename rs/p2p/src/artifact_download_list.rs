//! A data structure that tracks the download process of artifacts.
//!
//! <h1>Overview</h1>
//!
//! Artifact download list. This data structure tracks artifacts being
//! currently downloaded by P2P. The tracking is done using 2 indices.
//!
//! a. The artifact index
//!
//!    Artifacts being downloaded can be looked up using their artifact
//!    IDs using this index. Artifact download schedule is determined
//!    by advert priority class i.e. higher priority adverts are
//!    scheduled for download ahead of lower priority adverts. Within
//!    the same priority class adverts are scheduled for download in a
//!    first come first serve order.
//!
//! b. Download Expiry Index
//!
//!    Artifacts downloads are allotted a finite time duration to
//!    conclude (artifact timeout). This duration is roughly based on
//!    the size of the artifact and the count of unique peers that may
//!    be contacted for the artifact. Artifact downloads thus expire at
//!    a future time instant called the expiry-instant. The expiry
//!    index orders the downloads in increasing order of their
//!    expiry-instant. Note: This index may contain multiple downloads
//!    expiring at a given expiry-instant.

use ic_interfaces::artifact_manager::ArtifactManager;
use ic_logger::{replica_logger::ReplicaLogger, warn};
use ic_protobuf::registry::subnet::v1::GossipConfig;
use ic_types::{
    artifact::ArtifactId,
    chunkable::Chunkable,
    crypto::CryptoHash,
    p2p::{GossipAdvert, MAX_ARTIFACT_TIMEOUT},
    NodeId,
};
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

/// The trait defines the behavior of the `ArtifactDownloadList` data structure.
pub(crate) trait ArtifactDownloadList: Send + Sync {
    /// ```text
    /// The method schedules a download.
    ///
    /// Admission control for the artifact download list. Given a
    /// advert checks if the download can be scheduled without
    /// violating the IC download constraints. Returns an artifact
    /// download tracker if the download scheduling succeeds.
    ///
    /// Parameters:
    ///
    ///     `peer_id`: peer from which download is to be initiated. This
    ///              peers quota will be charged for the download
    ///     `advert`:  advert for the artifact being downloaded
    ///     `download_config`: download configuration for the p2p instance
    ///
    ///     `max_peer`: estimated number of peers that can
    ///               be contacted for this download.
    ///     `artifact_manager`: AM associated with this p2p instance.
    /// ```
    fn schedule_download(
        &mut self,
        peer_id: NodeId,
        advert: &GossipAdvert,
        gossip_config: &GossipConfig,
        max_peers: u32,
        artifact_manager: &dyn ArtifactManager,
    ) -> Option<&ArtifactTracker>;

    /// The method removes and returns the expired artifact downloads from the
    /// list.
    ///
    /// Returns:
    ///    Vec<(ArtifactId, CryptoHash)>: Vector of ids associated with the
    /// given expired downloads.
    fn prune_expired_downloads(&mut self) -> Vec<(ArtifactId, CryptoHash)>;

    /// The method gets the artifact download tracker associated the given
    /// integrity hash.
    ///
    /// Parameters:
    ///     integrity_hash: integrity hash for the query
    ///
    /// Returns:
    ///     Option for the tracker
    fn get_tracker(&mut self, integrity_hash: &CryptoHash) -> Option<&mut ArtifactTracker>;

    /// The method removes the artifact download tracker for the given integrity
    /// hash if it exists.
    ///
    /// The method does nothing if the download tracker does not exist.
    ///
    /// Parameters:
    ///    integrity_hash: integrity hash for the download that needs to be
    /// removed.
    fn remove_tracker(&mut self, integrity_hash: &CryptoHash);
}

/// The artifact tracker.
pub(crate) struct ArtifactTracker {
    /// Artifact ID
    artifact_id: ArtifactId,
    /// Time limit for the artifact download.
    expiry_instant: Instant,
    /// The artifact, which implements the `Chunkable` interface.
    pub chunkable: Box<dyn Chunkable + Send + Sync>,
    /// The ID of the node whose quota is charged for this artifact.
    pub peer_id: NodeId,
    // Stores the e2e duration of downloading the artifact.
    duration: Instant,
}

impl ArtifactTracker {
    pub fn new(
        artifact_id: ArtifactId,
        expiry_instant: Instant,
        chunkable: Box<dyn Chunkable + Send + Sync>,
        peer_id: NodeId,
    ) -> Self {
        ArtifactTracker {
            artifact_id,
            expiry_instant,
            chunkable,
            peer_id,
            duration: Instant::now(),
        }
    }

    pub fn get_duration_sec(&mut self) -> f64 {
        self.duration.elapsed().as_secs_f64()
    }
}

/// The implementation of the `ArtifactDownloadList` trait.
pub(crate) struct ArtifactDownloadListImpl {
    /// A Hashmap is used for the artifacts because it provides constant lookup time complexity
    /// using the integrity hash.
    /// Artifacts to be downloaded with their corresponding trackers.
    artifacts: HashMap<CryptoHash, ArtifactTracker>,
    /// Expiry indices.
    expiry_index: BTreeMap<Instant, Vec<CryptoHash>>,
    /// The logger instance.
    log: ReplicaLogger,
}

impl ArtifactDownloadListImpl {
    /// The constructor creates an `ArtifactDownloadListImpl` instance.
    pub fn new(log: ReplicaLogger) -> Self {
        ArtifactDownloadListImpl {
            log,
            artifacts: Default::default(),
            expiry_index: Default::default(),
        }
    }
}

/// `ArtifactDownloadListImpl` implements the `ArtifactDownloadList` trait.
impl ArtifactDownloadList for ArtifactDownloadListImpl {
    /// The function checks if the download can be scheduled without violating
    /// the IC download constraints.
    fn schedule_download(
        &mut self,
        peer_id: NodeId,
        advert: &GossipAdvert,
        gossip_config: &GossipConfig,
        max_advertizing_peer: u32,
        artifact_manager: &dyn ArtifactManager,
    ) -> Option<&ArtifactTracker> {
        // Schedule a download of an artifact that is not currently being downloaded.
        if !self.artifacts.contains_key(&advert.integrity_hash) {
            let artifact_id = &advert.artifact_id;
            match artifact_manager.get_remaining_quota(artifact_id.into(), peer_id) {
                None => return None,
                Some(quota_size) if quota_size < advert.size => return None,
                Some(_) => { /* enough quota remaining */ }
            }

            if let Some(chunk_tracker) = artifact_manager.get_chunk_tracker(&advert.artifact_id) {
                let requested_instant = Instant::now();
                // Calculate the worst-case time estimate for the artifact download, which
                // assumes that all chunks for the artifact will time out for
                // each peer that has advertised the artifact.
                // In any case the worst-case estimate is bound to a constant.
                //
                // TODO: Revisit this in the context of subnets with many nodes: P2P-510
                let download_eta_ms = std::cmp::min(
                    std::cmp::max(advert.size as u64 / gossip_config.max_chunk_size as u64, 1)
                        * max_advertizing_peer as u64
                        * gossip_config.max_chunk_wait_ms as u64,
                    MAX_ARTIFACT_TIMEOUT.as_millis() as u64,
                );
                let expiry_instant = requested_instant + Duration::from_millis(download_eta_ms);
                self.artifacts.insert(
                    advert.integrity_hash.clone(),
                    ArtifactTracker::new(
                        artifact_id.clone(),
                        expiry_instant,
                        chunk_tracker,
                        peer_id,
                    ),
                );
                self.expiry_index
                    .entry(expiry_instant)
                    .and_modify(|expired_artifacts| {
                        expired_artifacts.push(advert.integrity_hash.clone())
                    })
                    .or_insert_with(|| (vec![advert.integrity_hash.clone()]));
            } else {
                warn!(self.log, "Chunk tracker not found for advert {:?}", advert);
            }
        }
        self.artifacts.get(&advert.integrity_hash)
    }

    ///The function removes and returns the expired artifact downloads from the
    /// artifact download list.
    fn prune_expired_downloads(&mut self) -> Vec<(ArtifactId, CryptoHash)> {
        let now_instant = Instant::now();
        // 2-phase pruning of the expired downloads:
        //
        // In the first phase, the list of time-ordered expired-instants is traversed,
        // collecting instants until the current time instant is passed.
        //
        // In the second phase, artifacts are extracted from the
        // expired-instants collected in the first phase.
        //
        // Finally, the expired-instant entries are deleted.

        // Collect the expired instances.
        let expired_instances: Vec<_> = self
            .expiry_index
            .iter()
            .take_while(|(expiry_instant, _)| **expiry_instant < now_instant)
            .map(|(expiry_instant, expired_artifacts)| (*expiry_instant, expired_artifacts.clone()))
            .collect();

        // Extract artifact IDs from the expired instances.
        let mut expired_artifacts = Vec::new();
        expired_instances
            .iter()
            .for_each(|(_expiry_instant, integrity_hashes)| {
                integrity_hashes.iter().for_each(|integrity_hash| {
                    let index_integrity_hash = integrity_hash.clone();
                    let id = self.artifacts.remove(integrity_hash).unwrap().artifact_id;
                    expired_artifacts.push((id, index_integrity_hash));
                });
            });

        // Remove the expired instances.
        expired_instances.into_iter().for_each(|(instant, _)| {
            self.expiry_index.remove(&instant);
        });
        expired_artifacts
    }

    /// The method returns the artifact tracker associated with the given
    /// integrity hash.
    fn get_tracker(&mut self, integrity_hash: &CryptoHash) -> Option<&mut ArtifactTracker> {
        self.artifacts.get_mut(integrity_hash)
    }

    /// The function removes the artifact download tracker if it exists in the
    /// download list.
    fn remove_tracker(&mut self, integrity_hash: &CryptoHash) {
        // Remove the integrity hash from the integrity hash index.
        if let Some(tracker) = self.artifacts.remove(integrity_hash) {
            // Remove the integrity hash from the expiry entry.
            if let Some(expiry_entry) = self.expiry_index.get_mut(&tracker.expiry_instant) {
                expiry_entry
                    .retain(|expired_artifacts_hash| expired_artifacts_hash != integrity_hash);
                // remove expiry entry if no more artifacts are expiring at that instant
                if expiry_entry.is_empty() {
                    self.expiry_index.remove(&tracker.expiry_instant);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download_management::tests::TestArtifactManager;
    use ic_test_utilities::p2p::p2p_test_setup_logger;
    use ic_test_utilities::types::ids::node_test_id;
    use ic_types::artifact::ArtifactAttribute;
    use ic_types::crypto::CryptoHash;
    use ic_types::p2p;
    use std::convert::TryFrom;

    /// The function schedules the download of the given number of adverts.
    fn try_begin_download(
        num_adverts: i32,
        gossip_config: &GossipConfig,
        artifact_manager: &dyn ArtifactManager,
        artifact_download_list: &mut dyn ArtifactDownloadList,
    ) -> std::time::Instant {
        // Insert and remove artifacts.
        let mut max_expiry = std::time::Instant::now();
        for advert_id in 0..num_adverts {
            let gossip_advert = GossipAdvert {
                artifact_id: ArtifactId::FileTreeSync(advert_id.to_string()),
                attribute: ArtifactAttribute::FileTreeSync(advert_id.to_string()),
                size: 0,
                integrity_hash: CryptoHash(vec![u8::try_from(advert_id).unwrap()]),
            };
            let tracker = artifact_download_list
                .schedule_download(
                    node_test_id(0),
                    &gossip_advert,
                    gossip_config,
                    1,
                    artifact_manager,
                )
                .unwrap();

            if tracker.expiry_instant > max_expiry {
                max_expiry = tracker.expiry_instant;
            }
        }
        max_expiry
    }

    /// The function tests that artifact download list is pruned correctly when
    /// artifact downloads expire.
    #[test]
    fn download_list_expire_test() {
        // Insert and time out artifacts.
        let artifact_manager = TestArtifactManager {
            quota: std::usize::MAX,
            num_chunks: 0,
        };
        let logger = p2p_test_setup_logger();
        let log: ReplicaLogger = logger.root.clone().into();
        let mut artifact_download_list = ArtifactDownloadListImpl::new(log);
        let mut gossip_config = p2p::build_default_gossip_config();
        gossip_config.max_chunk_wait_ms = 1000;
        let num_adverts = 30;
        let max_expiry = try_begin_download(
            num_adverts,
            &gossip_config,
            &artifact_manager,
            &mut artifact_download_list,
        );

        while std::time::Instant::now() < max_expiry {
            std::thread::sleep(std::time::Duration::from_millis(
                gossip_config.max_chunk_wait_ms as u64,
            ));
        }

        let expired = artifact_download_list.prune_expired_downloads();
        assert_eq!(expired.len(), num_adverts as usize);
        assert_eq!(artifact_download_list.artifacts.len(), 0);

        // Check that the expired artifact list is empty.
        let expired = artifact_download_list.prune_expired_downloads();
        assert_eq!(expired.len(), 0);
        assert_eq!(artifact_download_list.artifacts.len(), 0);
    }

    /// The function tests that artifact trackers can be removed from the
    /// artifact download list.
    #[test]
    fn download_list_remove_test() {
        let artifact_manager = TestArtifactManager {
            quota: std::usize::MAX,
            num_chunks: 0,
        };
        let logger = p2p_test_setup_logger();
        let log: ReplicaLogger = logger.root.clone().into();
        let mut artifact_download_list = ArtifactDownloadListImpl::new(log);
        let mut gossip_config = p2p::build_default_gossip_config();
        gossip_config.max_chunk_wait_ms = 1000;
        let num_adverts = 30;
        let _max_expiry = try_begin_download(
            num_adverts,
            &gossip_config,
            &artifact_manager,
            &mut artifact_download_list,
        );

        for num in 0..num_adverts {
            let integrity_hash = CryptoHash(vec![u8::try_from(num).unwrap()]);
            artifact_download_list.get_tracker(&integrity_hash).unwrap();
            artifact_download_list.remove_tracker(&integrity_hash);
        }
        assert_eq!(artifact_download_list.artifacts.len(), 0);
    }
}
