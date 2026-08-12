//! Catalog request scope and pagination validation.

use tonic::Status;

use crate::pb;

pub(crate) fn ensure_enumerate_objects_scope_is_all(
    request: &pb::EnumerateObjectsRequest,
) -> Result<(), Status> {
    match request.scope.as_ref() {
        None | Some(pb::enumerate_objects_request::Scope::All(_)) => Ok(()),
        Some(_) => Err(Status::unimplemented(
            "scoped object enumeration is not wired in this slice",
        )),
    }
}

pub(crate) fn ensure_enumerate_units_scope_is_all(
    request: &pb::EnumerateUnitsRequest,
) -> Result<(), Status> {
    match request.scope.as_ref() {
        None | Some(pb::enumerate_units_request::Scope::All(_)) => Ok(()),
        Some(_) => Err(Status::unimplemented(
            "scoped catalog unit enumeration is not wired in this slice",
        )),
    }
}

pub(crate) fn ensure_unpaged(
    page_token: Option<&pb::PageToken>,
    page_size: u32,
) -> Result<(), Status> {
    if page_size != 0 || page_token.is_some_and(|token| !token.value.is_empty()) {
        return Err(Status::unimplemented(
            "paginated catalog listing is not wired in this slice",
        ));
    }
    Ok(())
}
