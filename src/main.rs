use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

// use aws_sdk_ec2::client::fluent_builders::RequestSpotInstances;
use aws_sdk_ec2::error::{
    AllocateAddressError, AssociateAddressError, DescribeAddressesError, DescribeInstancesError,
    DisassociateAddressError, ReleaseAddressError,
};
use aws_sdk_ec2::model::Filter;
use aws_sdk_ec2::output::DescribeInstancesOutput;
use aws_sdk_ec2::types::SdkError;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_servicequotas::error::GetServiceQuotaError;
use aws_sdk_servicequotas::model::ServiceQuota;
use aws_sdk_servicequotas::types::SdkError as ServiceQuotaSdkError;
use aws_sdk_servicequotas::Client as ServiceQuotaClient;
use futures_util::StreamExt;
use json_patch::{PatchOperation, RemoveOperation, TestOperation};
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::{Client, CustomResource, CustomResourceExt, Resource, ResourceExt};
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use kube_runtime::finalizer::{finalizer, Event};
use kube_runtime::wait::{await_condition, conditions};
use opentelemetry::sdk::trace::{Config, Sampler};
use opentelemetry::sdk::Resource as OtelResource;
use opentelemetry::Key;
use opentelemetry_otlp::WithExportConfig;
use rand::{thread_rng, Rng};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::join;
use tokio::time::error::Elapsed;
use tracing::{debug, event, info, instrument, Level, Metadata, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::{Context as LayerContext, Filter as LayerFilter, SubscriberExt};
use tracing_subscriber::prelude::*;

mod eip;

const LEGACY_MANAGE_EIP_LABEL: &str = "eip.aws.materialize.com/manage";
const LEGACY_POD_FINALIZER_NAME: &str = "eip.aws.materialize.com/disassociate";

const FIELD_MANAGER: &str = "eip.materialize.cloud";
const MANAGE_EIP_LABEL: &str = "eip.materialize.cloud/manage";
const AUTOCREATE_EIP_LABEL: &str = "eip.materialize.cloud/autocreate_eip";
const POD_FINALIZER_NAME: &str = "eip.materialize.cloud/disassociate";
const EIP_API_VERSION: &str = "materialize.cloud/v1";
const EIP_FINALIZER_NAME: &str = "eip.materialize.cloud/destroy";
const EIP_ALLOCATION_ID_ANNOTATION: &str = "eip.materialize.cloud/allocation_id";
const EXTERNAL_DNS_TARGET_ANNOTATION: &str = "external-dns.alpha.kubernetes.io/target";

// See https://us-east-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas
// and filter in the UI for EC2 quotas like this, or use the CLI:
//   aws --profile=mz-cloud-staging-admin service-quotas list-service-quotas --service-code=ec2
const EIP_QUOTA_CODE: &str = "L-0263D0A3";

struct ContextData {
    cluster_name: String,
    default_tags: HashMap<String, String>,
    k8s_client: Client,
    ec2_client: Ec2Client,
}

impl std::fmt::Debug for ContextData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextData")
            .field("cluster_name", &self.cluster_name)
            .field("default_tags", &self.default_tags)
            .finish()
    }
}

impl ContextData {
    fn new(
        cluster_name: String,
        default_tags: HashMap<String, String>,
        k8s_client: Client,
        ec2_client: Ec2Client,
    ) -> ContextData {
        ContextData {
            cluster_name,
            default_tags,
            k8s_client,
            ec2_client,
        }
    }
}

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
struct EipSpec {
    pod_name: String,
}

/// The status fields for the Eip Kubernetes custom resource.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EipStatus {
    allocation_id: Option<String>,
    public_ip_address: Option<String>,
    eni: Option<String>,
    private_ip_address: Option<String>,
}

/// Registers the Eip custom resource with Kubernetes,
/// the specification of which is automatically derived from the structs.
#[instrument(skip(k8s_client), err, fields(crd_data))]
async fn register_eip_custom_resource(k8s_client: Client) -> Result<(), Error> {
    // https://github.com/kube-rs/kube-rs/blob/master/examples/crd_derive_schema.rs#L224
    let crd_api = Api::<CustomResourceDefinition>::all(k8s_client);
    let data = serde_json::json!(Eip::crd());
    let crd_json = serde_json::to_string(&data)?;
    event!(Level::INFO, crd_json = %crd_json);
    let crd_patch = Patch::Apply(data);
    crd_api
        .patch(
            "eips.materialize.cloud",
            &PatchParams::apply(FIELD_MANAGER),
            &crd_patch,
        )
        .await?;
    let establish = await_condition(
        crd_api.clone(),
        "eips.materialize.cloud",
        conditions::is_crd_established(),
    );
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), establish).await?;
    Ok(())
}

/// Applies annotation to pod specifying the target IP for external-dns.
#[instrument(skip(pod_api), err)]
async fn add_dns_target_annotation(
    pod_api: &Api<Pod>,
    pod_name: String,
    eip_address: String,
    allocation_id: String,
) -> Result<Pod, kube::Error> {
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "annotations": {
                EIP_ALLOCATION_ID_ANNOTATION: allocation_id,
                EXTERNAL_DNS_TARGET_ANNOTATION: eip_address
            }
        }
    });
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(FIELD_MANAGER);
    pod_api.patch(&pod_name, &params, &patch).await
}

/// Sets the allocationId and publicIpAddress fields in the Eip status.
#[instrument(skip(eip_api), err)]
async fn set_eip_status_created(
    eip_api: &Api<Eip>,
    eip_name: &str,
    allocation_id: String,
    public_ip_address: String,
) -> Result<Eip, kube::Error> {
    event!(Level::INFO, "Updating status for created EIP.");
    let patch = serde_json::json!({
        "apiVersion": EIP_API_VERSION,
        "kind": "Eip",
        "status": {
            "allocationId": allocation_id,
            "publicIpAddress": public_ip_address,
        }
    });
    let patch = Patch::Merge(&patch);
    let params = PatchParams::default();
    let result = eip_api.patch_status(eip_name, &params, &patch).await;
    if result.is_ok() {
        event!(Level::INFO, "Done updating status for created EIP.");
    }
    result
}

/// Sets the eni and privateIpAddress fields in the Eip status.
#[instrument(skip(eip_api), err)]
async fn set_eip_status_attached(
    eip_api: &Api<Eip>,
    eip_name: &str,
    eni: String,
    private_ip_address: String,
) -> Result<Eip, kube::Error> {
    event!(Level::INFO, "Updating status for attached EIP.");
    let patch = serde_json::json!({
        "apiVersion": EIP_API_VERSION,
        "kind": "Eip",
        "status": {
            "eni": eni,
            "privateIpAddress": private_ip_address,
        }
    });
    let patch = Patch::Merge(&patch);
    let params = PatchParams::default();
    let result = eip_api.patch_status(eip_name, &params, &patch).await;
    if result.is_ok() {
        event!(Level::INFO, "Done updating status for attached EIP.");
    }
    result
}

/// Unsets the eni and privateIpAddress fields in the Eip status.
#[instrument(skip(eip_api), err)]
async fn set_eip_status_detached(eip_api: &Api<Eip>, eip_name: &str) -> Result<Eip, kube::Error> {
    event!(Level::INFO, "Updating status for detached EIP.");
    let patch = serde_json::json!({
        "apiVersion": EIP_API_VERSION,
        "kind": "Eip",
        "status": {
            "eni": None::<String>,
            "privateIpAddress": None::<String>,
        }
    });
    let patch = Patch::Merge(&patch);
    let params = PatchParams::default();
    let result = eip_api.patch_status(eip_name, &params, &patch).await;
    if result.is_ok() {
        event!(Level::INFO, "Done updating status for detached EIP.");
    }
    result
}

/// Describes an AWS EC2 instance with the supplied instance_id.
#[instrument(skip(ec2_client), err)]
async fn describe_instance(
    ec2_client: &Ec2Client,
    instance_id: String,
) -> Result<DescribeInstancesOutput, SdkError<DescribeInstancesError>> {
    ec2_client
        .describe_instances()
        .instance_ids(instance_id)
        .send()
        .await
}

/// An annotation attached to a pod by EKS describing the branch network interfaces when using per-pod security groups.
/// example: [{"eniId":"eni-0e42914a33ee3c5ce","ifAddress":"0e:cb:3c:0d:97:3b","privateIp":"10.1.191.190","vlanId":1,"subnetCidr":"10.1.160.0/19"}]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EniDescription {
    eni_id: String,
}

/// Parse the vpc.amazonaws.com/pod-eni annotation if it exists, and return the ENI ID.
#[instrument(skip(pod))]
fn get_eni_id_from_annotation(pod: &Pod) -> Option<String> {
    event!(Level::INFO, "Getting ENI ID from annotation.");
    let annotation = pod
        .metadata
        .annotations
        .as_ref()?
        .get("vpc.amazonaws.com/pod-eni")?;
    event!(Level::INFO, annotation = %annotation);
    let eni_descriptions: Vec<EniDescription> = serde_json::from_str(annotation).ok()?;
    Some(eni_descriptions.first()?.eni_id.to_owned())
}

/// Checks if the autocreate label is set to true on a pod.
fn should_autocreate_eip(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|label| label.get(AUTOCREATE_EIP_LABEL).map(|s| (*s).as_ref()))
        .unwrap_or("false")
        .to_lowercase()
        == "true"
}

/// Creates a K8S Eip resource.
#[instrument(skip(eip_api), err)]
async fn create_k8s_eip(eip_api: &Api<Eip>, pod_name: &str) -> Result<Eip, kube::Error> {
    //info!("Applying K8S Eip: {}", pod_name);
    let patch = Eip::new(
        pod_name,
        EipSpec {
            pod_name: pod_name.to_owned(),
        },
    );
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(FIELD_MANAGER);
    eip_api.patch(pod_name, &params, &patch).await
}

/// Deletes a K8S Eip resource, if it exists.
#[instrument(skip(eip_api), err)]
async fn delete_k8s_eip(eip_api: &Api<Eip>, name: &str) -> Result<(), kube::Error> {
    //info!("Deleting K8S Eip: {}", name);
    match eip_api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            info!("Eip already deleted: {}", name);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Creates or updates EIP associations when creating or updating a pod.
#[instrument(skip(ec2_client, node_api, eip_api, pod_api, pod), err)]
async fn apply_pod(
    ec2_client: &Ec2Client,
    node_api: &Api<Node>,
    eip_api: &Api<Eip>,
    pod_api: &Api<Pod>,
    pod: Arc<Pod>,
) -> Result<ReconcilerAction, Error> {
    let pod_name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
    event!(Level::INFO, pod_name = %pod_name, "Applying pod.");
    if should_autocreate_eip(&pod) {
        event!(Level::INFO, should_autocreate_eip = true);
        create_k8s_eip(eip_api, pod_name).await?;
    }
    let pod_ip = pod
        .status
        .as_ref()
        .ok_or(Error::MissingPodIp)?
        .pod_ip
        .as_ref()
        .ok_or(Error::MissingPodIp)?;

    let node_name = pod
        .spec
        .as_ref()
        .ok_or(Error::MissingNodeName)?
        .node_name
        .as_ref()
        .ok_or(Error::MissingNodeName)?;

    let node = node_api.get(node_name).await?;

    let provider_id = node
        .spec
        .as_ref()
        .ok_or(Error::MissingProviderId)?
        .provider_id
        .as_ref()
        .ok_or(Error::MissingProviderId)?;

    let instance_id = provider_id
        .rsplit_once('/')
        .ok_or(Error::MalformedProviderId)?
        .1;

    let eni_id = match get_eni_id_from_annotation(&pod) {
        Some(eni_id) => eni_id,
        None => {
            let instance_description =
                describe_instance(ec2_client, instance_id.to_owned()).await?;

            instance_description
                .reservations
                .as_ref()
                .ok_or(Error::MissingReservations)?[0]
                .instances
                .as_ref()
                .ok_or(Error::MissingInstances)?[0]
                .network_interfaces
                .as_ref()
                .ok_or(Error::MissingNetworkInterfaces)?
                .iter()
                .find_map(|nic| {
                    nic.private_ip_addresses.as_ref()?.iter().find_map(|ip| {
                        match ip.private_ip_address.as_ref()? {
                            x if x == pod_ip => {
                                debug!(
                                    "Found matching NIC: {} {} {}",
                                    nic.network_interface_id.as_ref()?,
                                    pod_ip,
                                    ip.private_ip_address.as_ref()?,
                                );
                                Some(nic.network_interface_id.as_ref()?.to_owned())
                            }
                            _ => None,
                        }
                    })
                })
                .ok_or(Error::NoInterfaceWithThatIp)?
        }
    };

    let all_eips = eip_api.list(&ListParams::default()).await?.items;
    let eip = all_eips
        .into_iter()
        .find(|eip| &eip.spec.pod_name == pod_name)
        .ok_or_else(|| Error::NoEipResourceWithThatPodName(pod_name.to_owned()))?;
    let eip_name = eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
    let allocation_id = eip
        .status
        .as_ref()
        .ok_or(Error::MissingEipStatus)?
        .allocation_id
        .as_ref()
        .ok_or(Error::MissingAllocationId)?;
    let eip_description = eip::describe_address(ec2_client, allocation_id.to_owned())
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?
        .swap_remove(0);
    let public_ip = eip_description.public_ip.ok_or(Error::MissingPublicIp)?;
    if eip_description.network_interface_id != Some(eni_id.to_owned())
        || eip_description.private_ip_address != Some(pod_ip.to_owned())
    {
        eip::associate_eip_with_pod_eni(
            ec2_client,
            allocation_id.to_owned(),
            eni_id.to_owned(),
            pod_ip.to_owned(),
        )
        .await?;
    }
    set_eip_status_attached(eip_api, eip_name, eni_id, pod_ip.to_owned()).await?;
    add_dns_target_annotation(
        pod_api,
        pod_name.to_owned(),
        public_ip,
        allocation_id.to_owned(),
    )
    .await?;
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(thread_rng().gen_range(240..360))),
    })
}

#[instrument(skip(ec2_client, eip_api, eip), err)]
async fn apply_eip(
    ec2_client: &Ec2Client,
    eip_api: &Api<Eip>,
    eip: Arc<Eip>,
    cluster_name: &str,
    namespace: &str,
    default_tags: &HashMap<String, String>,
) -> Result<ReconcilerAction, Error> {
    let eip_uid = eip.metadata.uid.as_ref().ok_or(Error::MissingEipUid)?;
    let eip_name = eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
    let pod_name = &eip.spec.pod_name;
    event!(Level::INFO, %eip_uid, %eip_name, %pod_name, "Applying EIP.");
    let addresses =
        eip::describe_addresses_with_tag_value(ec2_client, eip::EIP_UID_TAG, eip_uid.to_owned())
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?;
    let (allocation_id, public_ip) = match addresses.len() {
        0 => {
            let response = eip::allocate_address(
                ec2_client,
                eip_uid,
                eip_name,
                pod_name,
                cluster_name,
                namespace,
                default_tags,
            )
            .await?;
            let allocation_id = response.allocation_id.ok_or(Error::MissingAllocationId)?;
            let public_ip = response.public_ip.ok_or(Error::MissingPublicIp)?;
            (allocation_id, public_ip)
        }
        1 => {
            let allocation_id = addresses[0]
                .allocation_id
                .as_ref()
                .ok_or(Error::MissingAllocationId)?;
            let public_ip = addresses[0]
                .public_ip
                .as_ref()
                .ok_or(Error::MissingPublicIp)?;
            (allocation_id.to_owned(), public_ip.to_owned())
        }
        _ => {
            return Err(Error::MultipleEipsTaggedForPod);
        }
    };
    set_eip_status_created(eip_api, eip_name, allocation_id, public_ip).await?;
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(thread_rng().gen_range(240..360))),
    })
}

/// Deletes AWS Elastic IP associated with a pod being destroyed.
#[instrument(skip(ec2_client, eip_api, pod), err)]
async fn cleanup_pod(
    ec2_client: &Ec2Client,
    eip_api: &Api<Eip>,
    pod: Arc<Pod>,
) -> Result<ReconcilerAction, Error> {
    let pod_name = pod.metadata.name.as_ref().ok_or(Error::MissingPodUid)?;
    event!(Level::INFO, pod_name = %pod_name, "Cleaning up pod.");
    let all_eips = eip_api.list(&ListParams::default()).await?.items;
    let eip = all_eips
        .into_iter()
        .find(|eip| &eip.spec.pod_name == pod_name);
    if let Some(eip) = eip {
        let allocation_id = eip
            .status
            .as_ref()
            .ok_or(Error::MissingEipStatus)?
            .allocation_id
            .as_ref()
            .ok_or(Error::MissingAllocationId)?;
        let addresses = eip::describe_address(ec2_client, allocation_id.to_owned())
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?;
        for address in addresses {
            if let Some(association_id) = address.association_id {
                eip::disassociate_eip(ec2_client, association_id).await?;
            }
        }
        set_eip_status_detached(
            eip_api,
            eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?,
        )
        .await?;
    };
    if should_autocreate_eip(&pod) {
        event!(Level::INFO, should_autocreate_eip = true);
        delete_k8s_eip(eip_api, pod_name).await?;
    }
    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

#[instrument(skip(ec2_client, eip), err)]
async fn cleanup_eip(ec2_client: &Ec2Client, eip: Arc<Eip>) -> Result<ReconcilerAction, Error> {
    let eip_name = eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
    let eip_uid = eip.metadata.uid.as_ref().ok_or(Error::MissingEipUid)?;
    event!(Level::INFO, eip_name = %eip_name, eip_uid = %eip_uid, "Cleaning up eip.");
    let addresses =
        eip::describe_addresses_with_tag_value(ec2_client, eip::EIP_UID_TAG, eip_uid.to_owned())
            .await?
            .addresses;
    if let Some(addresses) = addresses {
        for address in addresses {
            eip::disassociate_and_release_address(ec2_client, &address).await?;
        }
    }
    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

/// Finds all EIPs tagged for this cluster, then compares them to the pod UIDs. If the EIP is not
/// tagged with a pod UID, or the UID does not exist in this cluster, it deletes the EIP.
#[instrument(skip(ec2_client, eip_api, pod_api), err)]
async fn cleanup_orphan_eips(
    ec2_client: &Ec2Client,
    eip_api: &Api<Eip>,
    pod_api: &Api<Pod>,
    cluster_name: &str,
    namespace: Option<&str>,
) -> Result<(), Error> {
    let mut describe_addresses = ec2_client.describe_addresses().filters(
        Filter::builder()
            .name(format!("tag:{}", eip::CLUSTER_NAME_TAG))
            .values(cluster_name.to_owned())
            .build(),
    );
    if let Some(namespace) = namespace {
        describe_addresses = describe_addresses.filters(
            Filter::builder()
                .name(format!("tag:{}", eip::NAMESPACE_TAG))
                .values(namespace.to_owned())
                .build(),
        )
    }
    let mut addresses = describe_addresses
        .send()
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?;

    let mut legacy_addresses = eip::describe_addresses_with_tag_value(
        ec2_client,
        eip::LEGACY_CLUSTER_NAME_TAG,
        cluster_name.to_owned(),
    )
    .await?
    .addresses
    .ok_or(Error::MissingAddresses)?;

    addresses.append(&mut legacy_addresses);

    let eip_uids: HashSet<String> = eip_api
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter_map(|eip| eip.metadata.uid)
        .collect();

    for address in addresses {
        let eip_uid = eip::get_tag_from_address(&address, eip::EIP_UID_TAG);
        if eip_uid.is_none() || !eip_uids.contains(eip_uid.unwrap()) {
            event!(Level::WARN,
                allocation_id = %address.allocation_id.as_deref().unwrap_or("None"),
                eip_uid = %eip_uid.unwrap_or("None"),
                "Cleaning up orphaned EIP",
            );
            eip::disassociate_and_release_address(ec2_client, &address).await?;
        }
    }

    // Manually remove the old finalizer, since we just removed the EIPs.
    // https://docs.rs/kube-runtime/0.65.0/src/kube_runtime/finalizer.rs.html#133
    let legacy_pods = pod_api
        .list(&ListParams::default().labels(LEGACY_MANAGE_EIP_LABEL))
        .await?;
    for pod in legacy_pods {
        if let Some(position) = pod
            .finalizers()
            .iter()
            .position(|s| s == LEGACY_POD_FINALIZER_NAME)
        {
            let pod_name = pod.meta().name.as_ref().ok_or(Error::MissingPodName)?;
            let finalizer_path = format!("/metadata/finalizers/{}", position);
            pod_api
                .patch::<Pod>(
                    pod_name,
                    &PatchParams::default(),
                    &Patch::Json(json_patch::Patch(vec![
                        PatchOperation::Test(TestOperation {
                            path: finalizer_path.clone(),
                            value: LEGACY_POD_FINALIZER_NAME.into(),
                        }),
                        PatchOperation::Remove(RemoveOperation {
                            path: finalizer_path,
                        }),
                    ])),
                )
                .await?;
        }
    }
    Ok(())
}

/// Takes actions to create/associate an EIP with the pod or clean up if the pod is being deleted.
/// Wraps these operations with a finalizer to ensure the pod is not deleted without cleaning up
/// the Elastic IP associated with it.
#[instrument(skip(pod, context), err)]
async fn reconcile_pod(
    pod: Arc<Pod>,
    context: Context<ContextData>,
) -> Result<ReconcilerAction, kube_runtime::finalizer::Error<Error>> {
    let namespace = pod.namespace().unwrap();
    let k8s_client = context.get_ref().k8s_client.clone();
    let pod_api: Api<Pod> = Api::namespaced(k8s_client.clone(), &namespace);
    let eip_api = Api::<Eip>::namespaced(k8s_client.clone(), &namespace);
    let node_api: Api<Node> = Api::all(k8s_client.clone());
    let ec2_client = context.get_ref().ec2_client.clone();
    finalizer(&pod_api, POD_FINALIZER_NAME, pod, |event| async {
        match event {
            Event::Apply(pod) => apply_pod(&ec2_client, &node_api, &eip_api, &pod_api, pod).await,
            Event::Cleanup(pod) => cleanup_pod(&ec2_client, &eip_api, pod).await,
        }
    })
    .await
}

/// Takes actions to create an EIP or clean up if the Eip K8S resource is being deleted.
/// Wraps these operations with a finalizer to ensure the K8S Eip is not deleted without
/// cleaning up the Elastic IP associated with it.
#[instrument(skip(eip, context), err)]
async fn reconcile_eip(
    eip: Arc<Eip>,
    context: Context<ContextData>,
) -> Result<ReconcilerAction, kube_runtime::finalizer::Error<Error>> {
    let namespace = eip.namespace().unwrap();
    let cluster_name = &context.get_ref().cluster_name;
    let default_tags = &context.get_ref().default_tags;
    let k8s_client = context.get_ref().k8s_client.clone();
    let eip_api = Api::<Eip>::namespaced(k8s_client.clone(), &namespace);
    let ec2_client = context.get_ref().ec2_client.clone();
    finalizer(&eip_api, EIP_FINALIZER_NAME, eip, |event| async {
        match event {
            Event::Apply(eip) => {
                apply_eip(
                    &ec2_client,
                    &eip_api,
                    eip,
                    cluster_name,
                    &namespace,
                    default_tags,
                )
                .await
            }
            Event::Cleanup(eip) => cleanup_eip(&ec2_client, eip).await,
        }
    })
    .await
}

/// Requeues the operation if there is an error.
fn on_error(
    _error: &kube_runtime::finalizer::Error<Error>,
    _context: Context<ContextData>,
) -> ReconcilerAction {
    ReconcilerAction {
        requeue_after: Some(Duration::from_millis(thread_rng().gen_range(4000..8000))),
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("Kubernetes error: {source}")]
    Kube {
        #[from]
        source: kube::Error,
    },
    #[error("No EIP found with that podName.")]
    NoEipResourceWithThatPodName(String),
    #[error("EIP does not have a status.")]
    MissingEipStatus,
    #[error("EIP does not have a UID in its metadata.")]
    MissingEipUid,
    #[error("EIP does not have a name in its metadata.")]
    MissingEipName,
    #[error("Pod does not have a UID in its metadata.")]
    MissingPodUid,
    #[error("Pod does not have a name in its metadata.")]
    MissingPodName,
    #[error("Pod does not have an IP address.")]
    MissingPodIp,
    #[error("Pod does not have a node name in its spec.")]
    MissingNodeName,
    #[error("Node does not have a provider_id in its spec.")]
    MissingProviderId,
    #[error("Node provider_id is not in expected format.")]
    MalformedProviderId,
    #[error("Multiple elastic IPs are tagged with this pod's UID.")]
    MultipleEipsTaggedForPod,
    #[error("allocation_id was None.")]
    MissingAllocationId,
    #[error("public_ip was None.")]
    MissingPublicIp,
    #[error("DescribeInstancesResult.reservations was None.")]
    MissingReservations,
    #[error("DescribeInstancesResult.reservations[0].instances was None.")]
    MissingInstances,
    #[error("DescribeInstancesResult.reservations[0].instances[0].network_interfaces was None.")]
    MissingNetworkInterfaces,
    #[error("No interface found with IP matching pod.")]
    MissingAddresses,
    #[error("DescribeAddressesResult.addresses was None.")]
    NoInterfaceWithThatIp,
    #[error("AWS allocate_address reported error: {source}")]
    AllocateAddress {
        #[from]
        source: SdkError<AllocateAddressError>,
    },
    #[error("AWS describe_instances reported error: {source}")]
    AwsDescribeInstances {
        #[from]
        source: SdkError<DescribeInstancesError>,
    },
    #[error("AWS describe_addresses reported error: {source}")]
    AwsDescribeAddresses {
        #[from]
        source: SdkError<DescribeAddressesError>,
    },
    #[error("AWS associate_address reported error: {source}")]
    AwsAssociateAddress {
        #[from]
        source: SdkError<AssociateAddressError>,
    },
    #[error("AWS disassociate_address reported error: {source}")]
    AwsDisassociateAddress {
        #[from]
        source: SdkError<DisassociateAddressError>,
    },
    #[error("AWS release_address reported error: {source}")]
    AwsReleaseAddress {
        #[from]
        source: SdkError<ReleaseAddressError>,
    },
    #[error("AWS get service quota reported error: {source}")]
    AwsGetServiceQuota {
        #[from]
        source: ServiceQuotaSdkError<GetServiceQuotaError>,
    },

    #[error("serde_json error: {source}")]
    SerdeJson {
        #[from]
        source: serde_json::Error,
    },
    #[error("Tokio Timeout Elapsed: {source}")]
    TokioTimeoutElapsed {
        #[from]
        source: Elapsed,
    },
}

fn main() -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_with_tracing())?;
    Ok(())
}

struct MyEnvFilter(EnvFilter);

impl<S> LayerFilter<S> for MyEnvFilter
where
    S: Subscriber,
{
    fn enabled(&self, meta: &Metadata<'_>, ctx: &LayerContext<S>) -> bool {
        self.0.enabled(meta, ctx.to_owned())
    }
}

async fn run_with_tracing() -> Result<(), Error> {
    match std::env::var("OPENTELEMETRY_ENDPOINT") {
        Ok(otel_endpoint) => {
            let otel_headers: HashMap<String, String> = serde_json::from_str(
                &std::env::var("OPENTELEMETRY_HEADERS").unwrap_or_else(|_| "{}".to_owned()),
            )?;
            let otel_sample_rate =
                &std::env::var("OPENTELEMETRY_SAMPLE_RATE").unwrap_or_else(|_| "0.05".to_owned());
            let otlp_exporter = opentelemetry_otlp::new_exporter()
                .grpcio()
                .with_endpoint(&otel_endpoint)
                .with_tls(true)
                .with_timeout(Duration::from_secs(30))
                .with_headers(otel_headers);
            let tracer = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(otlp_exporter)
                .with_trace_config(
                    Config::default()
                        .with_sampler(Sampler::TraceIdRatioBased(
                            otel_sample_rate.parse().unwrap(),
                        ))
                        .with_resource(OtelResource::new([
                            Key::from_static_str("service.name").string("eip_operator")
                        ])),
                )
                .install_batch(opentelemetry::runtime::Tokio)
                .unwrap();
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            let stdout_layer =
                fmt::Layer::default().with_filter(MyEnvFilter(EnvFilter::from_default_env()));
            tracing_subscriber::Registry::default()
                .with(otel_layer)
                .with(stdout_layer)
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt::init();
        }
    };
    run().await
}

#[instrument(skip(ec2_client, quota_client), err)]
async fn report_eip_quota_status(
    ec2_client: &Ec2Client,
    quota_client: &ServiceQuotaClient,
) -> Result<(), Error> {
    let addresses_result = ec2_client.describe_addresses().send().await?;
    let allocated = addresses_result.addresses().unwrap_or_default().len();
    let quota_result = quota_client
        .get_service_quota()
        .service_code("ec2")
        .quota_code(EIP_QUOTA_CODE)
        .send()
        .await?;
    let quota = quota_result
        .quota()
        .and_then(|q: &ServiceQuota| q.value)
        .unwrap_or(0f64);
    event!(Level::INFO, eips_allocated = %allocated, eip_quota = %quota, "eip_quota_checked");
    Ok(())
}

async fn run() -> Result<(), Error> {
    debug!("Getting k8s_client...");
    let k8s_client = Client::try_default().await?;

    debug!("Getting ec2_client...");
    let aws_config = aws_config::load_from_env().await;
    let ec2_client = Ec2Client::new(&aws_config);

    debug!("Getting quota_client...");
    let aws_config = aws_config::load_from_env().await;
    let quota_client = ServiceQuotaClient::new(&aws_config);

    debug!("Getting namespace from env...");
    let namespace = std::env::var("NAMESPACE").ok();

    debug!("Getting cluster name from env...");
    let cluster_name =
        std::env::var("CLUSTER_NAME").expect("Environment variable CLUSTER_NAME is required.");

    debug!("Getting default tags from env...");
    let default_tags: HashMap<String, String> =
        serde_json::from_str(&std::env::var("DEFAULT_TAGS").unwrap_or_else(|_| "{}".to_owned()))?;

    register_eip_custom_resource(k8s_client.clone()).await?;

    debug!("Getting pod api");
    let pod_api = match namespace {
        Some(ref namespace) => Api::<Pod>::namespaced(k8s_client.clone(), namespace),
        None => Api::<Pod>::all(k8s_client.clone()),
    };

    debug!("Getting eip api");
    let eip_api = match namespace {
        Some(ref namespace) => Api::<Eip>::namespaced(k8s_client.clone(), namespace),
        None => Api::<Eip>::all(k8s_client.clone()),
    };

    debug!("Cleaning up any orphaned EIPs");
    cleanup_orphan_eips(
        &ec2_client,
        &eip_api,
        &pod_api,
        &cluster_name,
        namespace.as_deref(),
    )
    .await?;

    let ec3_client = ec2_client.clone();
    info!("Watching for events...");
    let context: Context<ContextData> = Context::new(ContextData::new(
        cluster_name,
        default_tags,
        k8s_client.clone(),
        ec2_client,
    ));
    let pod_controller = Controller::new(pod_api, ListParams::default().labels(MANAGE_EIP_LABEL))
        .run(reconcile_pod, on_error, context.clone())
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(resource) => {
                    event!(Level::INFO, pod_name = %resource.0.name, "Pod reconciliation successful.");
                }
                Err(err) => event!(Level::ERROR, err = %err, "Pod reconciliation error."),
            }
        });

    let eip_controller = Controller::new(eip_api, ListParams::default())
        .run(reconcile_eip, on_error, context)
        .then(|rr| async {
            if rr.is_ok() {
                // Note: the Err that might occur here will be handled by tracing
                // instrumentation, rather than directly here.
                report_eip_quota_status(&ec3_client, &quota_client).await;
            }
            rr
        })
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(resource) => {
                    event!(Level::INFO, eip_name = %resource.0.name, "EIP reconciliation successful.");
                }
                Err(err) => event!(Level::ERROR, err = %err, "EIP reconciliation error."),
            }
        });
    join!(pod_controller, eip_controller);
    debug!("exiting");
    Ok(())
}
