use iroh::EndpointId;
use iroh::endpoint::Connection;

use crate::shared::{EnrollmentCode, enrollment_code};

/// Client-side helper for deriving the code to display for an enrollment
/// connection.
///
/// On the client, `Connection::remote_id()` is the server id. iroh does not
/// expose the local endpoint id on `Connection`, so callers still pass their
/// own client endpoint id.
pub trait EnrollmentCodeExt {
    fn enrollment_code(&self, client_endpoint_id: EndpointId) -> EnrollmentCode;
}

impl EnrollmentCodeExt for Connection {
    fn enrollment_code(&self, client_endpoint_id: EndpointId) -> EnrollmentCode {
        enrollment_code(self, self.remote_id(), client_endpoint_id)
    }
}
