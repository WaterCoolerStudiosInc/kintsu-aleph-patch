use std::{collections::HashSet, marker::PhantomData, mem::size_of};

use log::warn;
use parity_scale_codec::{Decode, Encode, Error as CodecError, Input as CodecInput};

use crate::{
    aleph_primitives::MAX_BLOCK_SIZE,
    network::GossipNetwork,
    sync::{Block, BlockIdFor, Justification, LOG_TARGET},
    Version,
};

/// The representation of the database state to be sent to other nodes.
/// In the first version this only contains the top justification.
#[derive(Clone, Debug, Encode, Decode)]
pub struct State<J: Justification> {
    top_justification: J::Unverified,
}

impl<J: Justification> State<J> {
    pub fn new(top_justification: J::Unverified) -> Self {
        State { top_justification }
    }

    pub fn top_justification(&self) -> J::Unverified {
        self.top_justification.clone()
    }
}

/// Additional information about the branch connecting the top finalized block
/// with a given one. All the variants are exhaustive and exclusive due to the
/// properties of the `Forest` structure.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub enum BranchKnowledge<J: Justification> {
    /// ID of the oldest known ancestor if none of them are imported.
    /// It must be different from the, imported by definition, root.
    LowestId(BlockIdFor<J>),
    /// ID of the top imported ancestor if any of them is imported.
    /// Since imported vertices are connected to the root, the oldest known
    /// ancestor is, implicitly, the root.
    TopImported(BlockIdFor<J>),
}

/// Request content.
#[derive(Clone, Debug, Encode, Decode)]
pub struct Request<J: Justification> {
    target_id: BlockIdFor<J>,
    branch_knowledge: BranchKnowledge<J>,
    state: State<J>,
}

impl<J: Justification> Request<J> {
    pub fn new(
        target_id: BlockIdFor<J>,
        branch_knowledge: BranchKnowledge<J>,
        state: State<J>,
    ) -> Self {
        Self {
            target_id,
            branch_knowledge,
            state,
        }
    }
}

impl<J: Justification> Request<J> {
    pub fn state(&self) -> &State<J> {
        &self.state
    }
}

/// Data to be sent over the network.
#[derive(Clone, Debug, Encode, Decode)]
pub enum NetworkDataV1<J: Justification> {
    /// A periodic state broadcast, so that neighbouring nodes can request what they are missing,
    /// send what we are missing, and sometimes just use the justifications to update their own
    /// state.
    StateBroadcast(State<J>),
    /// Response to a state broadcast. Contains at most two justifications that the peer will
    /// understand.
    StateBroadcastResponse(J::Unverified, Option<J::Unverified>),
    /// An explicit request for data, potentially a lot of it.
    Request(Request<J>),
    /// Response to the request for data. Currently consists only of justifications.
    RequestResponse(Vec<J::Unverified>),
}

/// Data to be sent over the network.
#[derive(Clone, Debug, Encode, Decode)]
pub enum NetworkData<B: Block, J: Justification> {
    /// A periodic state broadcast, so that neighbouring nodes can request what they are missing,
    /// send what we are missing, and sometimes just use the justifications to update their own
    /// state.
    StateBroadcast(State<J>),
    /// Response to a state broadcast. Contains at most two justifications that the peer will
    /// understand.
    StateBroadcastResponse(J::Unverified, Option<J::Unverified>),
    /// An explicit request for data, potentially a lot of it.
    Request(Request<J>),
    /// Response to the request for data.
    RequestResponse(Vec<J::Unverified>, Vec<J::Header>, Vec<B>),
}

impl<B: Block, J: Justification> From<NetworkDataV1<J>> for NetworkData<B, J> {
    fn from(data: NetworkDataV1<J>) -> Self {
        match data {
            NetworkDataV1::StateBroadcast(state) => NetworkData::StateBroadcast(state),
            NetworkDataV1::StateBroadcastResponse(justification, maybe_justification) => {
                NetworkData::StateBroadcastResponse(justification, maybe_justification)
            }
            NetworkDataV1::Request(request) => NetworkData::Request(request),
            NetworkDataV1::RequestResponse(justifications) => {
                NetworkData::RequestResponse(justifications, Vec::new(), Vec::new())
            }
        }
    }
}

impl<B: Block, J: Justification> From<NetworkData<B, J>> for NetworkDataV1<J> {
    fn from(data: NetworkData<B, J>) -> Self {
        match data {
            NetworkData::StateBroadcast(state) => NetworkDataV1::StateBroadcast(state),
            NetworkData::StateBroadcastResponse(justification, maybe_justification) => {
                NetworkDataV1::StateBroadcastResponse(justification, maybe_justification)
            }
            NetworkData::Request(request) => NetworkDataV1::Request(request),
            NetworkData::RequestResponse(justifications, _, _) => {
                NetworkDataV1::RequestResponse(justifications)
            }
        }
    }
}

/// Version wrapper around the network data.
#[derive(Clone, Debug)]
pub enum VersionedNetworkData<B: Block, J: Justification> {
    // Most likely from the future.
    Other(Version, Vec<u8>),
    V1(NetworkDataV1<J>),
    V2(NetworkData<B, J>),
}

// We need 32 bits, since blocks can be quite sizeable.
type ByteCount = u32;

// We want to be able to safely send at least 10 blocks at once, so this gives uss a bit of wiggle
// room.
const MAX_SYNC_MESSAGE_SIZE: u32 = MAX_BLOCK_SIZE * 11;

fn encode_with_version(version: Version, payload: &[u8]) -> Vec<u8> {
    let size = payload.len().try_into().unwrap_or(ByteCount::MAX);

    if size > MAX_SYNC_MESSAGE_SIZE {
        warn!(
            target: LOG_TARGET,
            "Versioned sync message v{:?} too big during Encode. Size is {:?}. Should be {:?} at max.",
            version,
            payload.len(),
            MAX_SYNC_MESSAGE_SIZE
        );
    }

    let mut result = Vec::with_capacity(version.size_hint() + size.size_hint() + payload.len());

    version.encode_to(&mut result);
    size.encode_to(&mut result);
    result.extend_from_slice(payload);

    result
}

impl<B: Block, J: Justification> Encode for VersionedNetworkData<B, J> {
    fn size_hint(&self) -> usize {
        use VersionedNetworkData::*;
        let version_size = size_of::<Version>();
        let byte_count_size = size_of::<ByteCount>();
        version_size
            + byte_count_size
            + match self {
                Other(_, payload) => payload.len(),
                V1(data) => data.size_hint(),
                V2(data) => data.size_hint(),
            }
    }

    fn encode(&self) -> Vec<u8> {
        use VersionedNetworkData::*;
        match self {
            Other(version, payload) => encode_with_version(*version, payload),
            V1(data) => encode_with_version(Version(1), &data.encode()),
            V2(data) => encode_with_version(Version(2), &data.encode()),
        }
    }
}

impl<B: Block, J: Justification> Decode for VersionedNetworkData<B, J> {
    fn decode<I: CodecInput>(input: &mut I) -> Result<Self, CodecError> {
        use VersionedNetworkData::*;
        let version = Version::decode(input)?;
        let num_bytes = ByteCount::decode(input)?;
        match version {
            Version(1) => Ok(V1(NetworkDataV1::decode(input)?)),
            Version(2) => Ok(V2(NetworkData::decode(input)?)),
            _ => {
                if num_bytes > MAX_SYNC_MESSAGE_SIZE {
                    Err("Sync message has unknown version and is encoded as more than the maximum size.")?;
                };
                let mut payload = vec![0; num_bytes as usize];
                input.read(payload.as_mut_slice())?;
                Ok(Other(version, payload))
            }
        }
    }
}

/// Wrap around a network to avoid thinking about versioning.
pub struct VersionWrapper<B: Block, J: Justification, N: GossipNetwork<VersionedNetworkData<B, J>>>
{
    inner: N,
    _phantom: PhantomData<(B, J)>,
}

impl<B: Block, J: Justification, N: GossipNetwork<VersionedNetworkData<B, J>>>
    VersionWrapper<B, J, N>
{
    /// Wrap the inner network.
    pub fn new(inner: N) -> Self {
        VersionWrapper {
            inner,
            _phantom: PhantomData,
        }
    }
}

#[async_trait::async_trait]
impl<B: Block, J: Justification, N: GossipNetwork<VersionedNetworkData<B, J>>>
    GossipNetwork<NetworkData<B, J>> for VersionWrapper<B, J, N>
{
    type Error = N::Error;
    type PeerId = N::PeerId;

    fn send_to(
        &mut self,
        data: NetworkData<B, J>,
        peer_id: Self::PeerId,
    ) -> Result<(), Self::Error> {
        self.inner.send_to(
            VersionedNetworkData::V1(data.clone().into()),
            peer_id.clone(),
        )?;
        self.inner.send_to(VersionedNetworkData::V2(data), peer_id)
    }

    fn send_to_random(
        &mut self,
        data: NetworkData<B, J>,
        peer_ids: HashSet<Self::PeerId>,
    ) -> Result<(), Self::Error> {
        self.inner.send_to_random(
            VersionedNetworkData::V1(data.clone().into()),
            peer_ids.clone(),
        )?;
        self.inner
            .send_to_random(VersionedNetworkData::V2(data), peer_ids)
    }

    fn broadcast(&mut self, data: NetworkData<B, J>) -> Result<(), Self::Error> {
        self.inner
            .broadcast(VersionedNetworkData::V1(data.clone().into()))?;
        self.inner.broadcast(VersionedNetworkData::V2(data))
    }

    /// Retrieves next message from the network.
    ///
    /// # Cancel safety
    ///
    /// This method is cancellation safe.
    async fn next(&mut self) -> Result<(NetworkData<B, J>, Self::PeerId), Self::Error> {
        loop {
            match self.inner.next().await? {
                (VersionedNetworkData::Other(version, _), _) => warn!(target: LOG_TARGET, "Received sync data of unsupported version {:?}, this node might be running outdated software.", version),
                (VersionedNetworkData::V1(data), peer_id) => return Ok((data.into(), peer_id)),
                (VersionedNetworkData::V2(data), peer_id) => return Ok((data, peer_id)),
            }
        }
    }
}
