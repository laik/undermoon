use super::store::{MetaStore, MetaStoreError, MigrationType};
use crate::broker::store::InconsistentError;
use crate::common::cluster::{Cluster, DBName, MigrationTaskMeta, Node, Proxy};
use crate::common::version::UNDERMOON_VERSION;
use crate::coordinator::http_meta_broker::{
    ClusterNamesPayload, ClusterPayload, FailuresPayload, ProxyAddressesPayload, ProxyPayload,
};
use actix_web::{
    error, http, middleware, App, HttpRequest, HttpResponse, Json, Path, Responder, State,
};
use chrono;
use std::error::Error;
use std::sync::{Arc, RwLock};

pub fn gen_app(service: Arc<MemBrokerService>) -> App<Arc<MemBrokerService>> {
    App::with_state(service)
        .middleware(middleware::Logger::default())
        .prefix("/api")
        .resource("/version", |r| r.method(http::Method::GET).f(get_version))
        .resource("/metadata", |r| {
            r.method(http::Method::GET).f(get_all_metadata)
        })
        .resource("/validation", |r| {
            r.method(http::Method::POST).f(validate_meta)
        })
        .resource("/proxies/addresses", |r| {
            r.method(http::Method::GET).f(get_host_addresses)
        })
        .resource("/proxies/nodes/{proxy_address}", |r| {
            r.method(http::Method::DELETE).with(remove_proxy)
        })
        .resource("/proxies/nodes", |r| {
            r.method(http::Method::PUT).with(add_host)
        })
        .resource("/proxies/failover/{address}", |r| {
            r.method(http::Method::POST).with(replace_failed_node)
        })
        .resource("/proxies/meta/{address}", |r| {
            r.method(http::Method::GET).with(get_host_by_address)
        })
        .resource("/clusters/migrations", |r| {
            r.method(http::Method::PUT).with(commit_migration)
        })
        .resource("/clusters/meta/{cluster_name}", |r| {
            r.method(http::Method::GET).with(get_cluster_by_name)
        })
        .resource("/clusters/names", |r| {
            r.method(http::Method::GET).f(get_cluster_names)
        })
        .resource("/clusters/{cluster_name}/nodes/{proxy_address}", |r| {
            r.method(http::Method::DELETE)
                .with(remove_proxy_from_cluster);
        })
        .resource("/clusters/{cluster_name}/nodes", |r| {
            r.method(http::Method::POST).with(auto_add_nodes)
        })
        .resource("/clusters/{cluster_name}", |r| {
            r.method(http::Method::POST).with(add_cluster);
            r.method(http::Method::DELETE).with(remove_cluster);
        })
        .resource("/failures/{server_proxy_address}/{reporter_id}", |r| {
            r.method(http::Method::POST).with(add_failure)
        })
        .resource("/failures", |r| r.method(http::Method::GET).f(get_failures))
        .resource(
            "/clusters/{cluster_name}/migrations/half/{src_node}/{dst_node}",
            |r| r.method(http::Method::POST).with(migrate_half_slots),
        )
        .resource(
            "/clusters/{cluster_name}/migrations/all/{src_node}/{dst_node}",
            |r| r.method(http::Method::POST).with(migrate_all_slots),
        )
        .resource(
            "/clusters/{cluster_name}/migrations/{src_node}/{dst_node}",
            |r| r.method(http::Method::DELETE).with(stop_migrations),
        )
        .resource(
            "/clusters/{cluster_name}/replications/{master_node}/{replica_node}",
            |r| r.method(http::Method::POST).with(assign_replica),
        )
}

#[derive(Debug, Clone)]
pub struct MemBrokerConfig {
    pub address: String,
    pub failure_ttl: u64, // in seconds
}

pub struct MemBrokerService {
    config: MemBrokerConfig,
    store: Arc<RwLock<MetaStore>>,
}

impl MemBrokerService {
    pub fn new(config: MemBrokerConfig) -> Self {
        Self {
            config,
            store: Arc::new(RwLock::new(MetaStore::default())),
        }
    }

    pub fn get_all_data(&self) -> MetaStore {
        self.store
            .read()
            .expect("MemBrokerService::get_all_data")
            .clone()
    }

    pub fn get_host_addresses(&self) -> Vec<String> {
        self.store
            .read()
            .expect("MemBrokerService::get_host_addresses")
            .get_hosts()
    }

    pub fn get_host_by_address(&self, address: &str) -> Option<Proxy> {
        self.store
            .read()
            .expect("MemBrokerService::get_host_by_address")
            .get_host_by_address(address)
    }

    pub fn get_cluster_names(&self) -> Vec<DBName> {
        self.store
            .read()
            .expect("MemBrokerService::get_cluster_names")
            .get_cluster_names()
    }

    pub fn get_cluster_by_name(&self, name: &str) -> Option<Cluster> {
        self.store
            .read()
            .expect("MemBrokerService::get_cluster_by_name")
            .get_cluster_by_name(name)
    }

    pub fn add_hosts(&self, host_resource: ProxyResource) -> Result<(), MetaStoreError> {
        let ProxyResource {
            proxy_address,
            nodes,
        } = host_resource;
        self.store
            .write()
            .expect("MemBrokerService::add_hosts")
            .add_hosts(proxy_address, nodes)
    }

    pub fn add_cluster(&self, cluster_name: String) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::add_cluster")
            .add_cluster(cluster_name)
    }

    pub fn remove_cluster(&self, cluster_name: String) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::remove_cluster")
            .remove_cluster(cluster_name)
    }

    pub fn auto_add_node(&self, cluster_name: String) -> Result<Vec<Node>, MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::auto_add_node")
            .auto_add_nodes(cluster_name)
    }

    pub fn remove_proxy_from_cluster(
        &self,
        cluster_name: String,
        proxy_address: String,
    ) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::remove_proxy_from_cluster")
            .remove_proxy_from_cluster(cluster_name, proxy_address)
    }

    pub fn remove_proxy(&self, proxy_address: String) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::remove_proxy")
            .remove_proxy(proxy_address)
    }

    pub fn migrate_slots(
        &self,
        cluster_name: String,
        src_node_address: String,
        dst_node_address: String,
        migration_type: MigrationType,
    ) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::migrate_slots")
            .migrate_slots(
                cluster_name,
                src_node_address,
                dst_node_address,
                migration_type,
            )
    }

    pub fn stop_migrations(
        &self,
        cluster_name: String,
        src_node_address: String,
        dst_node_address: String,
    ) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::stop_migrations")
            .stop_migrations(cluster_name, src_node_address, dst_node_address)
    }

    pub fn assign_replica(
        &self,
        cluster_name: String,
        master_node_address: String,
        replica_node_address: String,
    ) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::assign_replica")
            .assign_replica(cluster_name, master_node_address, replica_node_address)
    }

    pub fn get_failures(&self) -> Vec<String> {
        let failure_ttl = chrono::Duration::seconds(self.config.failure_ttl as i64);
        self.store
            .write()
            .expect("MemBrokerService::get_failures")
            .get_failures(failure_ttl)
    }

    pub fn add_failure(&self, address: String, reporter_id: String) {
        self.store
            .write()
            .expect("MemBrokerService::add_failure")
            .add_failure(address, reporter_id)
    }

    pub fn commit_migration(&self, task: MigrationTaskMeta) -> Result<(), MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::commit_migration")
            .commit_migration(task)
    }

    pub fn replace_failed_node(
        &self,
        failed_proxy_address: String,
    ) -> Result<Proxy, MetaStoreError> {
        self.store
            .write()
            .expect("MemBrokerService::replace_failed_node")
            .replace_failed_proxy(failed_proxy_address)
    }

    pub fn validate_meta(&self) -> Result<(), InconsistentError> {
        self.store
            .read()
            .expect("MemBrokerService::validate_meta")
            .validate()
    }
}

fn get_version(_req: &HttpRequest<Arc<MemBrokerService>>) -> &'static str {
    UNDERMOON_VERSION
}

fn get_all_metadata(request: &HttpRequest<Arc<MemBrokerService>>) -> impl Responder {
    let metadata = request.state().get_all_data();
    Json(metadata)
}

fn get_host_addresses(request: &HttpRequest<Arc<MemBrokerService>>) -> impl Responder {
    let addresses = request.state().get_host_addresses();
    Json(ProxyAddressesPayload { addresses })
}

fn get_host_by_address((path, state): (Path<(String,)>, ServiceState)) -> impl Responder {
    let name = path.into_inner().0;
    let host = state.get_host_by_address(&name);
    Json(ProxyPayload { host })
}

fn get_cluster_names(request: &HttpRequest<Arc<MemBrokerService>>) -> impl Responder {
    let names = request.state().get_cluster_names();
    Json(ClusterNamesPayload { names })
}

fn get_cluster_by_name((path, state): (Path<(String,)>, ServiceState)) -> impl Responder {
    let name = path.into_inner().0;
    let cluster = state.get_cluster_by_name(&name);
    Json(ClusterPayload { cluster })
}

fn get_failures(request: &HttpRequest<Arc<MemBrokerService>>) -> impl Responder {
    let addresses = request.state().get_failures();
    Json(FailuresPayload { addresses })
}

#[derive(Deserialize, Serialize)]
pub struct ProxyResource {
    proxy_address: String,
    nodes: Vec<String>,
}

type ServiceState = State<Arc<MemBrokerService>>;

fn add_host(
    (host_resource, state): (Json<ProxyResource>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    state.add_hosts(host_resource.into_inner()).map(|()| "")
}

fn add_cluster(
    (path, state): (Path<(String,)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let cluster_name = path.into_inner().0;
    state.add_cluster(cluster_name).map(|()| "")
}

fn remove_cluster(
    (path, state): (Path<(String,)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let cluster_name = path.into_inner().0;
    state.remove_cluster(cluster_name).map(|()| "")
}

fn auto_add_nodes(
    (path, state): (Path<(String,)>, ServiceState),
) -> Result<Json<Vec<Node>>, MetaStoreError> {
    let cluster_name = path.into_inner().0;
    state.auto_add_node(cluster_name).map(Json)
}

fn remove_proxy_from_cluster(
    (path, state): (Path<(String, String)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let (cluster_name, proxy_address) = path.into_inner();
    state
        .remove_proxy_from_cluster(cluster_name, proxy_address)
        .map(|()| "")
}

fn remove_proxy(
    (path, state): (Path<(String,)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let (proxy_address,) = path.into_inner();
    state.remove_proxy(proxy_address).map(|()| "")
}

fn migrate_half_slots(
    (path, state): (Path<(String, String, String)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let (cluster_name, src_node_address, dst_node_address) = path.into_inner();
    state
        .migrate_slots(
            cluster_name,
            src_node_address,
            dst_node_address,
            MigrationType::Half,
        )
        .map(|()| "")
}

fn migrate_all_slots(
    (path, state): (Path<(String, String, String)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let (cluster_name, src_node_address, dst_node_address) = path.into_inner();
    state
        .migrate_slots(
            cluster_name,
            src_node_address,
            dst_node_address,
            MigrationType::All,
        )
        .map(|()| "")
}

fn stop_migrations(
    (path, state): (Path<(String, String, String)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let (cluster_name, src_node_address, dst_node_address) = path.into_inner();
    state
        .stop_migrations(cluster_name, src_node_address, dst_node_address)
        .map(|()| "")
}

fn assign_replica(
    (path, state): (Path<(String, String, String)>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    let (cluster_name, master_node_address, replica_node_address) = path.into_inner();
    state
        .assign_replica(cluster_name, master_node_address, replica_node_address)
        .map(|()| "")
}

fn add_failure((path, state): (Path<(String, String)>, ServiceState)) -> &'static str {
    let (server_proxy_address, reporter_id) = path.into_inner();
    state.add_failure(server_proxy_address, reporter_id);
    ""
}

fn commit_migration(
    (task, state): (Json<MigrationTaskMeta>, ServiceState),
) -> Result<&'static str, MetaStoreError> {
    state.commit_migration(task.into_inner()).map(|()| "")
}

fn replace_failed_node(
    (path, state): (Path<(String,)>, ServiceState),
) -> Result<Json<Proxy>, MetaStoreError> {
    let (proxy_address,) = path.into_inner();
    state.replace_failed_node(proxy_address).map(Json)
}

fn validate_meta(req: &HttpRequest<Arc<MemBrokerService>>) -> Result<String, InconsistentError> {
    req.state().validate_meta().map(|()| "".to_string())
}

impl error::ResponseError for MetaStoreError {
    fn error_response(&self) -> HttpResponse {
        let status_code = match self {
            MetaStoreError::NoAvailableResource => http::StatusCode::CONFLICT,
            _ => http::StatusCode::BAD_REQUEST,
        };
        let mut response = HttpResponse::new(status_code);
        response.set_body(self.description().to_string());
        response
    }
}

impl error::ResponseError for InconsistentError {
    fn error_response(&self) -> HttpResponse {
        let mut response = HttpResponse::new(http::StatusCode::INTERNAL_SERVER_ERROR);
        response.set_body(format!("{}", self));
        response
    }
}
