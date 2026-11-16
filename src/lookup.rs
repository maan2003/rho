//! Native iroh address lookup backed by the portable local registry.

use iroh::EndpointId;
use iroh::address_lookup::{AddressLookup, EndpointData, EndpointInfo, Error, Item};
use n0_future::StreamExt;
use n0_future::boxed::BoxStream;

use crate::LocalDiscovery;

const PROVENANCE: &str = "ipc-transport";

impl AddressLookup for LocalDiscovery {
    fn resolve(&self, id: EndpointId) -> Option<BoxStream<Result<Item, Error>>> {
        let discovery = self.clone();
        Some(
            n0_future::stream::once_future(async move {
                match discovery.resolve(id).await {
                    Ok(Some(addr)) => Some(Ok(Item::new(
                        EndpointInfo::from_parts(id, EndpointData::from_iter([addr])),
                        PROVENANCE,
                        None,
                    ))),
                    Ok(None) => None,
                    Err(error) => Some(Err(Error::from_err(PROVENANCE, error))),
                }
            })
            .filter_map(std::convert::identity)
            .boxed(),
        )
    }
}
