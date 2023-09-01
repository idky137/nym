use crate::{
    config::PrivacyLevel,
    error::{BackendError, Result},
    models::{
        DirectoryService, DirectoryServiceProvider, Gateway, HarbourMasterService, PagedResult,
    },
    state::State,
};
use itertools::Itertools;
use nym_api_requests::models::GatewayBondAnnotated;
use nym_bin_common::version_checker::is_minor_version_compatible;
use nym_config::defaults::var_names::{NETWORK_NAME, NYM_API};
use nym_contracts_common::types::Percent;
use nym_topology::gateway;
use nym_validator_client::nym_api::Client as ApiClient;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use url::Url;

pub(crate) static WELLKNOWN_DIR: &str = "https://nymtech.net/.wellknown";

static SERVICE_PROVIDER_URL_PATH: &str = "connect/service-providers.json";

// List of network-requesters running with medium toggle enabled, for testing
static SERVICE_PROVIDER_MEDIUM_URL_PATH: &str = "connect/service-providers-medium.json";

// Harbour master is used to periodically keep track of which network-requesters are online
static HARBOUR_MASTER_URL: &str = "https://harbourmaster.nymtech.net/v1/services/?size=100";

// We only consider network requesters with a routing score above this threshold
const SERVICE_ROUTING_SCORE_THRESHOLD: f32 = 0.9;

// Only use gateways with a performnnce score above this
const GATEWAY_PERFORMANCE_SCORE_THRESHOLD: u64 = 90;

#[tauri::command]
pub async fn get_services(
    state: tauri::State<'_, Arc<RwLock<State>>>,
) -> Result<Vec<DirectoryServiceProvider>> {
    let guard = state.read().await;
    let privacy_level = guard.get_user_data().privacy_level.unwrap_or_default();

    log::trace!("Fetching services");
    let all_services_with_category = fetch_services(&privacy_level).await?;
    log::trace!("Received: {:#?}", all_services_with_category);

    // Flatten all services into a single vector (get rid of categories)
    // We currently don't care about categories, but we might in the future...
    let all_services = all_services_with_category
        .into_iter()
        .flat_map(|sp| sp.items)
        .collect_vec();

    // Early return if we're running with medium toggle enabled
    if let PrivacyLevel::Medium = privacy_level {
        return Ok(all_services);
    }

    // TODO: get paged
    log::trace!("Fetching active services");
    let active_services = fetch_active_services().await?;
    log::trace!("Active: {:#?}", active_services);

    if active_services.items.is_empty() {
        log::warn!("No active services found! Using all services instead as fallback");
        return Ok(all_services);
    }

    log::trace!("Filter out inactive");
    let filtered_services = filter_out_inactive_services(&all_services, active_services);
    log::trace!("After filtering: {:#?}", filtered_services);

    if filtered_services.is_empty() {
        log::warn!(
            "After filtering, no active services found! Using all services instead as fallback"
        );
        return Ok(all_services);
    }

    Ok(filtered_services)
}

async fn fetch_services(privacy_level: &PrivacyLevel) -> Result<Vec<DirectoryService>> {
    let services_url = match privacy_level {
        PrivacyLevel::Medium => SERVICE_PROVIDER_MEDIUM_URL_PATH,
        _ => SERVICE_PROVIDER_URL_PATH,
    };

    let network_name = std::env::var(NETWORK_NAME)?;
    let url = format!("{}/{}/{}", WELLKNOWN_DIR, network_name, services_url);
    let services_res = reqwest::get(url)
        .await?
        .json::<Vec<DirectoryService>>()
        .await?;
    Ok(services_res)
}

async fn fetch_active_services() -> Result<PagedResult<HarbourMasterService>> {
    let active_services = reqwest::get(HARBOUR_MASTER_URL)
        .await?
        .json::<PagedResult<HarbourMasterService>>()
        .await?;
    Ok(active_services)
}

fn filter_out_inactive_services(
    all_services: &[DirectoryServiceProvider],
    active_services: PagedResult<HarbourMasterService>,
) -> Vec<DirectoryServiceProvider> {
    all_services
        .iter()
        .filter(|sp| {
            active_services.items.iter().any(|active| {
                active.service_provider_client_id == sp.address
                    && active.routing_score > SERVICE_ROUTING_SCORE_THRESHOLD
            })
        })
        .cloned()
        .collect()
}

async fn fetch_gateways() -> Result<Vec<GatewayBondAnnotated>> {
    let api_client = ApiClient::new(Url::from_str(&std::env::var(NYM_API)?)?);
    let gateways = api_client.get_gateways_detailed().await?;
    let our_version = env!("CARGO_PKG_VERSION");
    log::debug!(
        "Our version that we use to filter compatible gateways: {}",
        our_version
    );
    let gateways = gateways
        .into_iter()
        .filter(|g| is_minor_version_compatible(&g.gateway_bond.gateway.version, our_version))
        .collect();
    Ok(gateways)
}

fn filter_out_low_performance_gateways(
    gateways: Vec<GatewayBondAnnotated>,
) -> Vec<GatewayBondAnnotated> {
    gateways
        .into_iter()
        .filter(|g| {
            g.node_performance.most_recent
                > Percent::from_percentage_value(GATEWAY_PERFORMANCE_SCORE_THRESHOLD).unwrap()
        })
        .collect()
}

async fn select_gateway_by_latency(gateways: Vec<GatewayBondAnnotated>) -> Result<gateway::Node> {
    let gateways_as_nodes: Vec<gateway::Node> = gateways
        .into_iter()
        .filter_map(|g| g.gateway_bond.try_into().ok())
        .collect();

    let mut rng = rand_07::rngs::OsRng;
    let selected_gateway =
        nym_client_core::init::helpers::choose_gateway_by_latency(&mut rng, &gateways_as_nodes)
            .await?;
    Ok(selected_gateway)
}

// Get all gateways satisfying the performance threshold.
#[tauri::command]
pub async fn get_gateways() -> Result<Vec<Gateway>> {
    log::trace!("Fetching gateways");
    let all_gateways = fetch_gateways().await?;
    log::trace!("Received: {:#?}", all_gateways);

    let gateways_filtered = filter_out_low_performance_gateways(all_gateways.clone())
        .into_iter()
        .map(|g| Gateway {
            identity: g.identity().clone(),
        })
        .collect_vec();
    log::trace!("Filtered: {:#?}", gateways_filtered);

    if gateways_filtered.is_empty() {
        log::error!("No gateways found! (with high enough performance score)");
        return Err(BackendError::NoGatewaysFound);
    }

    Ok(gateways_filtered)
}

// Lookup and select a single gateway with low latency.
#[tauri::command]
pub async fn get_gateway_with_low_latency() -> Result<Gateway> {
    log::trace!("Fetching gateways");
    let all_gateways = fetch_gateways().await?;
    log::trace!("Received: {:#?}", all_gateways);

    let gateways_filtered = filter_out_low_performance_gateways(all_gateways);
    let selected_gateway = select_gateway_by_latency(gateways_filtered).await?;
    log::debug!("Selected gateway: {}", selected_gateway);
    Ok(Gateway {
        identity: selected_gateway.identity().to_base58_string(),
    })
}

// From a given list of gateways, select the one with low latency.
#[tauri::command]
pub async fn select_gateway_with_low_latency_from_list(gateways: Vec<Gateway>) -> Result<Gateway> {
    log::debug!("Selecting a gateway with low latency");
    let gateways = gateways.into_iter().map(|g| g.identity).collect_vec();
    let all_gateways = fetch_gateways().await?;
    let gateways_union_set: Vec<GatewayBondAnnotated> = all_gateways
        .into_iter()
        .filter(|g| gateways.contains(g.identity()))
        .collect();
    let gateways_filtered = filter_out_low_performance_gateways(gateways_union_set);
    if gateways_filtered.is_empty() {
        log::error!("No gateways found! (with high enough performance score)");
        return Err(BackendError::NoGatewaysFound);
    }
    let selected_gateway = select_gateway_by_latency(gateways_filtered).await?;
    log::debug!("Selected gateway: {}", selected_gateway);
    Ok(Gateway {
        identity: selected_gateway.identity().to_base58_string(),
    })
}
