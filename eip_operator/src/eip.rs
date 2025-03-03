use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::core::crd::merge_crds;
use kube::{Client, CustomResourceExt};
use kube_runtime::wait::{await_condition, conditions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{event, info, instrument, Level};

use eip_operator_shared::Error;

const CRD_NAME: &str = "eips.materialize.cloud";

use v2::{Eip, EipSelector, EipSpec};

pub mod v1 {
    use kube::api::Api;
    use kube::{Client, CustomResource};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use super::EipStatus;

    /// The spec for the Eip Kubernetes custom resource.
    /// An `Eip` type is generated by deriving `CustomResource`.
    #[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    #[kube(
        group = "materialize.cloud",
        version = "v1",
        kind = "Eip",
        singular = "eip",
        plural = "eips",
        namespaced,
        status = "EipStatus",
        printcolumn = r#"{"name": "AllocationID", "type": "string", "description": "Allocation ID of the EIP.", "jsonPath": ".status.allocationId"}"#,
        printcolumn = r#"{"name": "PublicIP", "type": "string", "description": "Public IP address of the EIP.", "jsonPath": ".status.publicIpAddress"}"#,
        printcolumn = r#"{"name": "Pod", "type": "string", "description": "Pod name to associate the EIP with.", "jsonPath": ".spec.podName", "priority": 1}"#,
        printcolumn = r#"{"name": "ENI", "type": "string", "description": "ID of the Elastic Network Interface of the pod.", "jsonPath": ".status.eni", "priority": 1}"#,
        printcolumn = r#"{"name": "PrivateIP", "type": "string", "description": "Private IP address of the pod.", "jsonPath": ".status.privateIpAddress", "priority": 1}"#
    )]
    pub struct EipSpec {
        pub pod_name: String,
    }

    #[derive(Clone, Serialize, Deserialize, Debug)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct LaxEipSpec {
        pub(crate) pod_name: Option<String>,
    }

    pub(crate) type LaxEip = kube::api::Object<LaxEipSpec, kube::api::NotUsed>;

    impl Eip {
        pub(crate) fn lax_api(k8s_client: Client, namespace: Option<&str>) -> Api<LaxEip> {
            Api::<LaxEip>::namespaced_with(
                k8s_client,
                namespace.unwrap_or("default"),
                &kube::api::ApiResource::erase::<Self>(&()),
            )
        }
    }
}

pub mod v2 {
    use kube::api::Api;
    use kube::{Client, CustomResource, Resource};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    use eip_operator_shared::Error;

    use super::EipStatus;

    #[derive(Eq, PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub enum EipSelector {
        #[serde(rename_all = "camelCase")]
        Pod { pod_name: String },
        #[serde(rename_all = "camelCase")]
        Node { selector: BTreeMap<String, String> },
    }

    impl std::fmt::Display for EipSelector {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
            match self {
                Self::Pod { pod_name } => {
                    write!(f, "Pod({})", pod_name)
                }
                Self::Node { selector } => {
                    write!(f, "Node(")?;
                    let mut first = true;
                    for label in selector {
                        if !first {
                            write!(f, ", ")?;
                            first = false;
                        }
                        write!(f, "{}: {}", label.0, label.1)?;
                    }
                    write!(f, ")")?;
                    Ok(())
                }
            }
        }
    }

    /// The spec for the Eip Kubernetes custom resource.
    /// An `Eip` type is generated by deriving `CustomResource`.
    #[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    #[kube(
        group = "materialize.cloud",
        version = "v2",
        kind = "Eip",
        singular = "eip",
        plural = "eips",
        namespaced,
        status = "EipStatus",
        printcolumn = r#"{"name": "AllocationID", "type": "string", "description": "Allocation ID of the EIP.", "jsonPath": ".status.allocationId"}"#,
        printcolumn = r#"{"name": "PublicIP", "type": "string", "description": "Public IP address of the EIP.", "jsonPath": ".status.publicIpAddress"}"#,
        printcolumn = r#"{"name": "Selector", "type": "string", "description": "Selector for the pod or node to associate the EIP with.", "jsonPath": ".spec.selector", "priority": 1}"#,
        printcolumn = r#"{"name": "ENI", "type": "string", "description": "ID of the Elastic Network Interface of the pod.", "jsonPath": ".status.eni", "priority": 1}"#,
        printcolumn = r#"{"name": "PrivateIP", "type": "string", "description": "Private IP address of the pod.", "jsonPath": ".status.privateIpAddress", "priority": 1}"#
    )]
    pub struct EipSpec {
        pub selector: EipSelector,
    }

    impl Eip {
        pub fn version() -> String {
            <Self as kube::Resource>::version(&()).into_owned()
        }

        pub(crate) fn api(k8s_client: Client, namespace: Option<&str>) -> Api<Self> {
            Api::<Self>::namespaced(k8s_client, namespace.unwrap_or("default"))
        }

        pub fn name(&self) -> Option<&str> {
            self.metadata.name.as_deref()
        }

        pub fn attached(&self) -> bool {
            self.status
                .as_ref()
                .map_or(false, |status| status.private_ip_address.is_some())
        }

        pub fn matches_pod(&self, pod_name: &str) -> bool {
            match self.spec.selector {
                EipSelector::Pod {
                    pod_name: ref this_pod_name,
                } => pod_name == this_pod_name,
                _ => false,
            }
        }

        pub fn matches_node(&self, node_labels: &BTreeMap<String, String>) -> bool {
            match self.spec.selector {
                EipSelector::Node { ref selector } => {
                    for (key, value) in selector {
                        match node_labels.get(key) {
                            Some(node_value) => {
                                if value != node_value {
                                    return false;
                                }
                            }
                            None => return false,
                        }
                    }
                    true
                }
                _ => false,
            }
        }

        pub fn allocation_id(&self) -> Option<&str> {
            self.status
                .as_ref()
                .and_then(|status| status.allocation_id.as_deref())
        }
    }

    impl TryFrom<&super::v1::LaxEip> for Eip {
        type Error = Option<Error>;

        fn try_from(eip_v1: &super::v1::LaxEip) -> Result<Self, Self::Error> {
            if let Some(pod_name) = &eip_v1.spec.pod_name {
                let name = eip_v1.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
                let mut eip = Self::new(
                    name,
                    EipSpec {
                        selector: EipSelector::Pod {
                            pod_name: pod_name.to_string(),
                        },
                    },
                );
                eip.meta_mut().resource_version = eip_v1.metadata.resource_version.clone();
                Ok(eip)
            } else {
                Err(None)
            }
        }
    }
}

/// The status fields for the Eip Kubernetes custom resource.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EipStatus {
    pub allocation_id: Option<String>,
    pub public_ip_address: Option<String>,
    pub eni: Option<String>,
    pub private_ip_address: Option<String>,
}

/// Registers the Eip custom resource with Kubernetes,
/// the specification of which is automatically derived from the structs.
#[instrument(skip(k8s_client), err, fields(crd_data))]
pub async fn register_custom_resource(
    k8s_client: Client,
    namespace: Option<&str>,
) -> Result<(), Error> {
    // https://github.com/kube-rs/kube-rs/blob/master/examples/crd_derive_schema.rs#L224
    let crd_api = Api::<CustomResourceDefinition>::all(k8s_client.clone());
    let data = merge_crds(vec![v1::Eip::crd(), v2::Eip::crd()], "v2").unwrap();
    let crd_json = serde_json::to_string(&data)?;
    event!(Level::INFO, crd_json = %crd_json);
    let crd_patch = Patch::Apply(data);
    crd_api
        .patch(
            CRD_NAME,
            &PatchParams::apply(crate::FIELD_MANAGER),
            &crd_patch,
        )
        .await?;
    let establish = await_condition(crd_api.clone(), CRD_NAME, conditions::is_crd_established());
    tokio::time::timeout(std::time::Duration::from_secs(10), establish).await??;

    upgrade_old_resources(k8s_client, namespace).await?;

    Ok(())
}

async fn upgrade_old_resources(k8s_client: Client, namespace: Option<&str>) -> Result<(), Error> {
    let eip_v1_api = v1::Eip::lax_api(k8s_client.clone(), namespace);
    for eip_v1 in eip_v1_api.list(&ListParams::default()).await? {
        match v2::Eip::try_from(&eip_v1) {
            Ok(eip) => {
                event!(
                    Level::INFO,
                    eip_v1 = serde_json::to_string(&eip_v1)?,
                    "updating existing eip to latest version"
                );
                let eip_api =
                    v2::Eip::api(k8s_client.clone(), eip_v1.metadata.namespace.as_deref());
                eip_api
                    .replace(
                        eip.metadata.name.as_ref().unwrap(),
                        &PostParams::default(),
                        &eip,
                    )
                    .await?;
            }
            Err(Some(e)) => {
                return Err(e);
            }
            Err(None) => {
                // not a v1 Eip
            }
        }
    }

    Ok(())
}

/// Creates a K8S Eip resource.
#[instrument(skip(api), err)]
pub(crate) async fn create_for_pod(api: &Api<Eip>, pod_name: &str) -> Result<Eip, kube::Error> {
    //info!("Applying K8S Eip: {}", pod_name);
    let patch = Eip::new(
        pod_name,
        EipSpec {
            selector: EipSelector::Pod {
                pod_name: pod_name.to_owned(),
            },
        },
    );
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(crate::FIELD_MANAGER);
    api.patch(pod_name, &params, &patch).await
}

/// Deletes a K8S Eip resource, if it exists.
#[instrument(skip(api), err)]
pub(crate) async fn delete(api: &Api<Eip>, name: &str) -> Result<(), kube::Error> {
    //info!("Deleting K8S Eip: {}", name);
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            info!("Eip already deleted: {}", name);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Sets the allocationId and publicIpAddress fields in the Eip status.
#[instrument(skip(api), err)]
pub(crate) async fn set_status_created(
    api: &Api<v2::Eip>,
    name: &str,
    allocation_id: &str,
    public_ip_address: &str,
) -> Result<Eip, kube::Error> {
    event!(Level::INFO, "Updating status for created EIP.");
    let patch = serde_json::json!({
        "apiVersion": Eip::version(),
        "kind": "Eip",
        "status": {
            "allocationId": allocation_id,
            "publicIpAddress": public_ip_address,
        }
    });
    let patch = Patch::Merge(&patch);
    let params = PatchParams::default();
    let result = api.patch_status(name, &params, &patch).await;
    if result.is_ok() {
        event!(Level::INFO, "Done updating status for created EIP.");
    }
    result
}

/// Sets the eni and privateIpAddress fields in the Eip status.
#[instrument(skip(api), err)]
pub(crate) async fn set_status_attached(
    api: &Api<Eip>,
    name: &str,
    eni: &str,
    private_ip_address: &str,
) -> Result<Eip, kube::Error> {
    event!(Level::INFO, "Updating status for attached EIP.");
    let patch = serde_json::json!({
        "apiVersion": Eip::version(),
        "kind": "Eip",
        "status": {
            "eni": eni,
            "privateIpAddress": private_ip_address,
        }
    });
    let patch = Patch::Merge(&patch);
    let params = PatchParams::default();
    let result = api.patch_status(name, &params, &patch).await;
    if result.is_ok() {
        event!(Level::INFO, "Done updating status for attached EIP.");
    }
    result
}

/// Unsets the eni and privateIpAddress fields in the Eip status.
#[instrument(skip(api), err)]
pub(crate) async fn set_status_detached(api: &Api<Eip>, name: &str) -> Result<Eip, kube::Error> {
    event!(Level::INFO, "Updating status for detached EIP.");
    let patch = serde_json::json!({
        "apiVersion": Eip::version(),
        "kind": "Eip",
        "status": {
            "eni": None::<String>,
            "privateIpAddress": None::<String>,
        }
    });
    let patch = Patch::Merge(&patch);
    let params = PatchParams::default();
    let result = api.patch_status(name, &params, &patch).await;
    if result.is_ok() {
        event!(Level::INFO, "Done updating status for detached EIP.");
    }
    result
}
