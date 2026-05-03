use core::fmt;

use heapless::String as HString;

use crate::k8s::conditions::NodeCondTracker;
use crate::k8s::models::{CustomCondEntry, HeapAnnotationPatch, LeaseBody, NodeStatusPatch};
use crate::kubelet::NodeIdentity;
use crate::net::client::SharedClient;
use crate::net::http::ApiError;

#[derive(Debug)]
pub enum ReconcileError {
    BuildJson(&'static str),
    Api {
        operation: &'static str,
        source: ApiError,
    },
    UnexpectedStatus {
        operation: &'static str,
        status: u16,
    },
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuildJson(operation) => write!(f, "{} body build failed", operation),
            Self::Api { operation, source } => {
                write!(f, "{} request failed: {:?}", operation, source)
            }
            Self::UnexpectedStatus { operation, status } => {
                write!(f, "{} returned HTTP {}", operation, status)
            }
        }
    }
}

pub enum LeaseRenewal {
    Renewed,
    Missing,
}

pub struct KubeApi {
    client: &'static SharedClient,
}

impl KubeApi {
    pub const fn new(client: &'static SharedClient) -> Self {
        Self { client }
    }

    pub async fn renew_lease(
        &self,
        identity: &NodeIdentity,
        renew_unix: u64,
    ) -> Result<LeaseRenewal, ReconcileError> {
        let mut body: HString<512> = HString::new();
        (LeaseBody { renew_unix })
            .write_json(&mut body)
            .map_err(|_| ReconcileError::BuildJson("lease renewal"))?;

        let mut c = self.client.lock().await;
        let status = c
            .patch_merge(identity.lease_path.as_str(), body.as_bytes())
            .await
            .map_err(|source| ReconcileError::Api {
                operation: "lease renewal",
                source,
            })?
            .status;

        match status {
            200 => Ok(LeaseRenewal::Renewed),
            404 => Ok(LeaseRenewal::Missing),
            other => Err(ReconcileError::UnexpectedStatus {
                operation: "lease renewal",
                status: other,
            }),
        }
    }

    pub async fn create_lease(&self, renew_unix: u64) -> Result<(), ReconcileError> {
        let mut body: HString<512> = HString::new();
        (LeaseBody { renew_unix })
            .write_json(&mut body)
            .map_err(|_| ReconcileError::BuildJson("lease create"))?;

        let mut c = self.client.lock().await;
        let status = c
            .post(
                "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
                body.as_bytes(),
            )
            .await
            .map_err(|source| ReconcileError::Api {
                operation: "lease create",
                source,
            })?
            .status;

        match status {
            201 | 409 => Ok(()),
            other => Err(ReconcileError::UnexpectedStatus {
                operation: "lease create",
                status: other,
            }),
        }
    }

    pub async fn patch_node_status(
        &self,
        identity: &NodeIdentity,
        tracker: &NodeCondTracker,
        custom: &[CustomCondEntry],
        heartbeat_unix: u64,
    ) -> Result<(), ReconcileError> {
        let mut body: HString<3072> = HString::new();
        (NodeStatusPatch {
            tracker,
            custom,
            heartbeat_unix,
        })
        .write_json(&mut body)
        .map_err(|_| ReconcileError::BuildJson("node status"))?;

        let mut c = self.client.lock().await;
        let status = c
            .patch_strategic(identity.status_path.as_str(), body.as_bytes())
            .await
            .map_err(|source| ReconcileError::Api {
                operation: "node status",
                source,
            })?
            .status;

        match status {
            200 => Ok(()),
            other => Err(ReconcileError::UnexpectedStatus {
                operation: "node status",
                status: other,
            }),
        }
    }

    pub async fn patch_heap_annotation(
        &self,
        identity: &NodeIdentity,
        free_bytes: usize,
    ) -> Result<(), ReconcileError> {
        let mut body: HString<256> = HString::new();
        (HeapAnnotationPatch { free_bytes })
            .write_json(&mut body)
            .map_err(|_| ReconcileError::BuildJson("heap annotation"))?;

        let mut c = self.client.lock().await;
        let status = c
            .patch_merge(identity.node_path.as_str(), body.as_bytes())
            .await
            .map_err(|source| ReconcileError::Api {
                operation: "heap annotation",
                source,
            })?
            .status;

        match status {
            200 => Ok(()),
            other => Err(ReconcileError::UnexpectedStatus {
                operation: "heap annotation",
                status: other,
            }),
        }
    }
}
