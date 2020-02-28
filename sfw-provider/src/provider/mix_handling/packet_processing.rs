use crate::provider::storage::{ClientStorage, StoreData};
use crypto::encryption;
use sphinx::route::{DestinationAddressBytes, SURBIdentifier};
use sphinx::{ProcessedPacket, SphinxPacket};
use std::ops::Deref;
use std::sync::Arc;

#[derive(Debug)]
pub enum MixProcessingError {
    ReceivedForwardHopError,
    NonMatchingRecipient,
    InvalidPayload,
    SphinxProcessingError,
    InvalidHopAddress,
}

pub enum MixProcessingResult {
    #[allow(dead_code)]
    ForwardHop,
    FinalHop,
}

impl From<sphinx::ProcessingError> for MixProcessingError {
    // for time being just have a single error instance for all possible results of sphinx::ProcessingError
    fn from(_: sphinx::ProcessingError) -> Self {
        use MixProcessingError::*;

        SphinxProcessingError
    }
}

// PacketProcessor contains all data required to correctly unwrap and store sphinx packets
#[derive(Clone)]
pub struct PacketProcessor {
    secret_key: Arc<encryption::PrivateKey>,
    client_store: ClientStorage,
}

impl PacketProcessor {
    pub(crate) fn new(secret_key: encryption::PrivateKey, client_store: ClientStorage) -> Self {
        PacketProcessor {
            secret_key: Arc::new(secret_key),
            client_store,
        }
    }

    async fn process_final_hop(
        &self,
        client_address: DestinationAddressBytes,
        surb_id: SURBIdentifier,
        payload: Payload,
    ) -> Result<MixProcessingResult, MixProcessingError> {
        // TODO: should provider try to be recovering plaintext? this would potentially make client retrieve messages of non-constant length,
        // perhaps provider should be re-padding them on retrieval or storing full data?
        let (payload_destination, message) = payload
            .try_recover_destination_and_plaintext()
            .ok_or_else(|| MixProcessingError::InvalidPayload)?;
        if client_address != payload_destination {
            return Err(MixProcessingError::NonMatchingRecipient);
        }

        let store_data = StoreData::new(client_address, surb_id, message);
        self.client_store.store_processed_data(store_data).await?;

        Ok(MixProcessingResult::FinalHop)
    }

    pub(crate) async fn process_sphinx_packet(
        &self,
        raw_packet_data: [u8; sphinx::PACKET_SIZE],
    ) -> Result<MixProcessingResult, MixProcessingError> {
        let packet = SphinxPacket::from_bytes(&raw_packet_data)?;

        match packet.process(self.secret_key.deref().inner()) {
            Ok(ProcessedPacket::ProcessedPacketForwardHop(_, _, _)) => {
                warn!("Received a forward hop message - those are not implemented for providers");
                Err(MixProcessingError::ReceivedForwardHopError)
            }
            Ok(ProcessedPacket::ProcessedPacketFinalHop(client_address, surb_id, payload)) => {
                self.process_final_hop(client_address, surb_id, payload)
                    .await
            }
            Err(e) => {
                warn!("Failed to unwrap Sphinx packet: {:?}", e);
                Err(MixProcessingError::SphinxProcessingError)
            }
        }
    }
}
