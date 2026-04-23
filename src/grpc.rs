//! gRPC surface for the ICP handler.
//!
//! Mirrors the JSON/HTTP surface one-to-one. Payload fields carry JCS-
//! canonicalized JSON bytes so the proto can remain stable while the
//! wire schema evolves additively.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::agent::{AgentIdentifier, ApiKeyInfo, ApiKeyStore};
use crate::discovery;
use crate::models::IntentEnvelope;
use crate::service::{IcpService, IntentInput};

pub mod proto {
    tonic::include_proto!("icp_handler.v1");
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("icp_handler_descriptor");
}

use proto::icp_handler_server::{IcpHandler, IcpHandlerServer};
use proto::{
    DiscoveryRequest, DiscoveryResponse, EventMessage, EventStreamRequest, GetMandateUsageRequest,
    GetReceiptRequest, GetTransactionRequest, IntentRequest, IntentResponse, MandateUsageResponse,
    ReceiptResponse, TransactionResponse, VerifyReceiptRequest, VerifyReceiptResponse,
};

pub struct GrpcHandler {
    pub service: Arc<IcpService>,
    pub keys: ApiKeyStore,
}

impl GrpcHandler {
    pub fn new(service: Arc<IcpService>, keys: ApiKeyStore) -> IcpHandlerServer<Self> {
        IcpHandlerServer::new(Self { service, keys })
    }
}

#[tonic::async_trait]
impl IcpHandler for GrpcHandler {
    type StreamEventsStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<EventMessage, Status>> + Send>>;

    async fn get_discovery(
        &self,
        _req: Request<DiscoveryRequest>,
    ) -> Result<Response<DiscoveryResponse>, Status> {
        let doc = discovery::build(&self.service.config, &self.service.signer);
        let bytes = serde_jcs::to_vec(&doc)
            .map_err(|e| Status::internal(format!("discovery canonicalize: {e}")))?;
        Ok(Response::new(DiscoveryResponse {
            payload_json: bytes,
            signature: String::new(),
            signature_kid: self.service.signer.kid.clone(),
        }))
    }

    async fn submit_intent(
        &self,
        req: Request<IntentRequest>,
    ) -> Result<Response<IntentResponse>, Status> {
        let api_key = extract_bearer_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let tenant = authenticate_bearer(&self.keys, &api_key)?;

        let inner = req.into_inner();
        let envelope = inner
            .envelope
            .ok_or_else(|| Status::invalid_argument("envelope required"))?;
        if self.service.config.require_icp_version
            && envelope.icp_version != self.service.config.icp_version
        {
            return Err(Status::invalid_argument(format!(
                "ICP version `{}` not supported; expected `{}`",
                envelope.icp_version, self.service.config.icp_version
            )));
        }
        if self.service.config.require_request_id && envelope.request_id.is_empty() {
            return Err(Status::invalid_argument("request_id required"));
        }
        let intent_env: IntentEnvelope = serde_json::from_slice(&inner.payload_json)
            .map_err(|e| Status::invalid_argument(format!("payload_json: {e}")))?;
        let agent = AgentIdentifier::parse(&envelope.agent_id);
        ensure_agent_allowed(&tenant, &agent)?;

        let mandate_jws = if envelope.mandate_jws.is_empty() {
            None
        } else {
            Some(envelope.mandate_jws.as_str())
        };
        let request_id = if envelope.request_id.is_empty() {
            format!("req_{}", uuid::Uuid::new_v4().simple())
        } else {
            envelope.request_id.clone()
        };
        let trace_id = if envelope.trace_id.is_empty() {
            None
        } else {
            Some(envelope.trace_id.clone())
        };

        let input =
            IntentInput::for_icp(intent_env, agent, tenant, mandate_jws, request_id, trace_id);

        let body = self
            .service
            .handle_intent(input)
            .await
            .map_err(api_error_to_status)?;

        let payload_json =
            serde_json::to_vec(&body).map_err(|e| Status::internal(format!("serialize: {e}")))?;

        Ok(Response::new(IntentResponse {
            payload_json,
            receipt_jws: body.receipt.jws,
            receipt_kid: body.receipt.kid,
        }))
    }

    async fn stream_events(
        &self,
        req: Request<EventStreamRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        use tokio_stream::wrappers::BroadcastStream;
        use tokio_stream::StreamExt;

        let api_key = extract_bearer_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let tenant = authenticate_bearer(&self.keys, &api_key)?;
        let tenant_id = tenant.tenant_id.clone();
        let service = self.service.clone();
        let rx = self.service.events.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(move |evt| match evt {
            Ok(e) => {
                if !service.event_belongs_to_tenant(&e, &tenant_id) {
                    return None;
                }
                let payload_json = serde_json::to_vec(&e.payload).unwrap_or_default();
                Some(Ok(EventMessage {
                    id: e.id,
                    r#type: e.r#type,
                    transaction_id: e.transaction_id.unwrap_or_default(),
                    occurred_at: e.occurred_at.to_rfc3339(),
                    payload_json,
                }))
            }
            Err(_) => None,
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_transaction(
        &self,
        req: Request<GetTransactionRequest>,
    ) -> Result<Response<TransactionResponse>, Status> {
        let api_key = extract_bearer_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let tenant = authenticate_bearer(&self.keys, &api_key)?;
        let inner = req.into_inner();
        let txn = self
            .service
            .transactions
            .get(&inner.transaction_id)
            .ok_or_else(|| Status::not_found("transaction not found"))?;
        if txn.tenant_id != tenant.tenant_id {
            return Err(Status::not_found("transaction not found"));
        }
        let payload_json =
            serde_json::to_vec(&txn).map_err(|e| Status::internal(format!("serialize: {e}")))?;
        Ok(Response::new(TransactionResponse { payload_json }))
    }

    async fn get_receipt(
        &self,
        req: Request<GetReceiptRequest>,
    ) -> Result<Response<ReceiptResponse>, Status> {
        let api_key = extract_bearer_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let tenant = authenticate_bearer(&self.keys, &api_key)?;
        let inner = req.into_inner();
        let r = self
            .service
            .receipts
            .get(&inner.receipt_jti)
            .ok_or_else(|| Status::not_found("receipt not found"))?;
        let txn_tenant = self
            .service
            .transactions
            .get(&r.claims.icp.transaction_id)
            .map(|t| t.tenant_id);
        if txn_tenant.as_deref() != Some(tenant.tenant_id.as_str()) {
            return Err(Status::not_found("receipt not found"));
        }
        let payload_json = serde_json::to_vec(&r.claims)
            .map_err(|e| Status::internal(format!("serialize: {e}")))?;
        Ok(Response::new(ReceiptResponse {
            receipt_jws: r.jws,
            receipt_kid: r.kid,
            payload_json,
        }))
    }

    async fn verify_receipt(
        &self,
        req: Request<VerifyReceiptRequest>,
    ) -> Result<Response<VerifyReceiptResponse>, Status> {
        let api_key = extract_bearer_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let tenant = authenticate_bearer(&self.keys, &api_key)?;
        let inner = req.into_inner();
        match self.service.signer.verify_receipt(
            &inner.receipt_jws,
            if inner.expected_body_json.is_empty() {
                None
            } else {
                Some(inner.expected_body_json.as_slice())
            },
        ) {
            Ok(claims) => {
                let txn_tenant = self
                    .service
                    .transactions
                    .get(&claims.icp.transaction_id)
                    .map(|t| t.tenant_id);
                if txn_tenant.as_deref() != Some(tenant.tenant_id.as_str()) {
                    return Ok(Response::new(VerifyReceiptResponse {
                        valid: false,
                        kid: self.service.signer.kid.clone(),
                        reason: "receipt does not belong to caller tenant".into(),
                        payload_json: Vec::new(),
                    }));
                }
                let payload_json = serde_json::to_vec(&claims)
                    .map_err(|e| Status::internal(format!("serialize: {e}")))?;
                Ok(Response::new(VerifyReceiptResponse {
                    valid: true,
                    kid: self.service.signer.kid.clone(),
                    reason: String::new(),
                    payload_json,
                }))
            }
            Err(reason) => Ok(Response::new(VerifyReceiptResponse {
                valid: false,
                kid: self.service.signer.kid.clone(),
                reason,
                payload_json: Vec::new(),
            })),
        }
    }

    async fn get_mandate_usage(
        &self,
        req: Request<GetMandateUsageRequest>,
    ) -> Result<Response<MandateUsageResponse>, Status> {
        let api_key = extract_bearer_metadata(req.metadata())
            .ok_or_else(|| Status::unauthenticated("missing bearer token"))?;
        let tenant = authenticate_bearer(&self.keys, &api_key)?;
        let mandate_jti = req.into_inner().mandate_jti;
        let usage = self
            .service
            .mandates
            .try_usage_for_tenant(&mandate_jti, &tenant.tenant_id)
            .map_err(|e| Status::unavailable(e.to_string()))?
            .ok_or_else(|| Status::not_found("mandate usage not found"))?;
        let payload = serde_json::json!({
            "spent_minor": usage.spent_minor,
            "window_start": usage.window_start,
        });
        let payload_json = serde_json::to_vec(&payload)
            .map_err(|e| Status::internal(format!("serialize: {e}")))?;
        Ok(Response::new(MandateUsageResponse { payload_json }))
    }
}

fn authenticate_bearer(keys: &ApiKeyStore, bearer: &str) -> Result<ApiKeyInfo, Status> {
    let tenant = keys
        .lookup(bearer)
        .ok_or_else(|| Status::unauthenticated("unknown API key"))?;
    if tenant.is_expired_at(chrono::Utc::now()) {
        return Err(Status::unauthenticated("API key expired"));
    }
    Ok(tenant)
}

fn ensure_agent_allowed(tenant: &ApiKeyInfo, agent: &AgentIdentifier) -> Result<(), Status> {
    if tenant.permits_agent(&agent.raw) {
        Ok(())
    } else {
        Err(Status::unauthenticated(format!(
            "agent `{}` is not allowed for this API key",
            agent.raw
        )))
    }
}

fn extract_bearer_metadata(md: &tonic::metadata::MetadataMap) -> Option<String> {
    md.get("authorization")
        .or_else(|| md.get("x-api-key"))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
}

fn api_error_to_status(err: crate::errors::ApiError) -> Status {
    use crate::errors::ApiError as E;
    match err {
        E::InvalidRequest(m) => Status::invalid_argument(m),
        E::AuthenticationFailed(m) | E::MandateInvalid(m) => Status::unauthenticated(m),
        E::MandateOutOfScope(m) => Status::permission_denied(m),
        E::MandateBudgetExceeded(m) => Status::resource_exhausted(m),
        E::IntentNotSupported(m) | E::ResourceNotFound(m) => Status::not_found(m),
        E::Conflict(m) | E::IdempotencyConflict(m) => Status::already_exists(m),
        E::PreconditionFailed(m) => Status::failed_precondition(m),
        E::RateLimited => Status::resource_exhausted("rate limited"),
        E::EngineUnavailable(m) => Status::unavailable(m),
        E::ProcessingError(m) => Status::internal(m),
    }
}
